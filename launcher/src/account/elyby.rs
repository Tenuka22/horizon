//! Ely.by accounts (authserver.ely.by): the sign-in with a username and password, the two-factor
//! code when the account asks for one, renewal and sign-out. The Ely access token takes the
//! Minecraft token's place in the secret store, the client token the refresh token's. Neither is
//! ever logged or shown.
use super::*;

/// What an Ely.by session is given when it is (re)issued: the answer carries no expiry.
pub(super) const LIFETIME: u64 = 24 * 3600;
pub const WRONG_CREDENTIALS: &str = "Wrong username or password.";
pub const WRONG_CODE: &str = "Wrong username, password or two-factor code.";
pub const CODE_NEEDED: &str =
    "This Ely.by account asks for a two-factor code. Enter it to sign in.";
pub(super) const EXPIRED: &str = "Your Ely.by sign-in expired. Sign in again.";
pub(super) const OTHER_PROFILE: &str = "The Ely.by account now has another profile. Sign in again.";
const FAILED: &str = "Could not sign in to Ely.by. Try again later.";
const RENEW_FAILED: &str = "Could not renew the Ely.by sign-in. Try again later.";

/// Ely.by's authserver: the built-in address, or `HORIZON_ELY_AUTH` (the mock in tests).
pub fn base() -> String {
    std::env::var("HORIZON_ELY_AUTH")
        .ok()
        .map(|b| b.trim().trim_end_matches('/').to_owned())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| horizon_addresses::ELY_AUTH.trim_end_matches('/').into())
}

/// A session at Ely.by. Holds tokens, so it is never printed.
pub struct ElyTokens {
    pub access: String,
    pub client_token: String,
    /// The profile's UUID without dashes.
    pub uuid: String,
    pub name: String,
}

impl std::fmt::Debug for ElyTokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ElyTokens(<hidden>)")
    }
}

/// A fresh client token for a sign-in, so that no two launchers share one.
pub fn client_token() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| e.to_string())?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn post(base: &str, path: &str, body: Value) -> Result<(u16, Value), String> {
    send(
        request_to("POST", &format!("{base}{path}")),
        Some(Body::Json(body)),
        "Ely.by",
    )
}

fn is_forbidden(v: &Value) -> bool {
    str_field(v, "error").as_deref() == Some("ForbiddenOperationException")
}

fn is_two_factor(status: u16, v: &Value) -> bool {
    status == 401 && str_field(v, "errorMessage").is_some_and(|m| m.contains("two factor"))
}

/// The session in an answer that carries one; `None` when it doesn't.
fn session(v: &Value, client_token: &str) -> Option<ElyTokens> {
    let profile = v.get("selectedProfile")?;
    Some(ElyTokens {
        access: str_field(v, "accessToken")?,
        client_token: client_token.to_owned(),
        uuid: str_field(profile, "id")?.replace('-', "").to_lowercase(),
        name: str_field(profile, "name")?,
    })
}

fn signed_in(
    status: u16,
    v: &Value,
    client_token: &str,
    refused: &str,
) -> Result<ElyTokens, String> {
    match status {
        200..=299 => session(v, client_token).ok_or_else(|| FAILED.into()),
        401 if is_forbidden(v) => Err(refused.into()),
        _ => Err(FAILED.into()),
    }
}

/// Signs in with the account's username and password. An account with two-factor auth gets
/// [`CODE_NEEDED`] until the same call comes with its code (`totp`).
pub fn authenticate(
    base: &str,
    username: &str,
    password: &str,
    totp: Option<&str>,
    client_token: &str,
) -> Result<ElyTokens, String> {
    let ask = |password: &str| {
        post(
            base,
            "/auth/authenticate",
            json!({
                "username": username,
                "password": password,
                "clientToken": client_token,
                "requestUser": true,
            }),
        )
    };
    let (status, v) = ask(password)?;
    if !is_two_factor(status, &v) {
        return signed_in(status, &v, client_token, WRONG_CREDENTIALS);
    }
    let Some(code) = totp else {
        return Err(CODE_NEEDED.into());
    };
    let (status, v) = ask(&format!("{password}:{code}"))?;
    signed_in(status, &v, client_token, WRONG_CODE)
}

/// Renews a session: the new access token is the one Ely.by answers with.
pub fn refresh(base: &str, access: &str, client_token: &str) -> Result<ElyTokens, String> {
    let (status, v) = post(
        base,
        "/auth/refresh",
        json!({
            "accessToken": access,
            "clientToken": client_token,
            "requestUser": true,
        }),
    )?;
    match status {
        200..=299 => session(&v, client_token).ok_or_else(|| RENEW_FAILED.into()),
        401 if is_forbidden(&v) => Err(EXPIRED.into()),
        _ => Err(RENEW_FAILED.into()),
    }
}

/// Ends the session at Ely.by. Best effort: a failure lets the token run out there.
pub fn invalidate(base: &str, access: &str, client_token: &str) {
    let _ = post(
        base,
        "/auth/invalidate",
        json!({"accessToken": access, "clientToken": client_token}),
    );
}

/// The secret-store entry for a new Ely.by session.
pub fn secrets(tokens: &ElyTokens) -> Secrets {
    Secrets {
        refresh_token: tokens.client_token.clone(),
        sign_in_required: false,
        minecraft_token: tokens.access.clone(),
        minecraft_expires_at: now() + LIFETIME,
    }
}

/// Ely.by's skin system: the built-in address, or `HORIZON_ELY_SKINS` (the mock in tests).
pub fn skins_base() -> String {
    std::env::var("HORIZON_ELY_SKINS")
        .ok()
        .map(|b| b.trim().trim_end_matches('/').to_owned())
        .filter(|b| !b.is_empty())
        .unwrap_or_else(|| horizon_addresses::ELY_SKINS.trim_end_matches('/').into())
}

/// The skin system's textures URL for the player `name`: the name is the player's own, so it is
/// encoded as one path segment.
pub fn textures_url(base: &str, name: &str) -> String {
    let segment: String = name
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' | b'.' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect();
    format!("{base}/textures/{segment}")
}

/// A player's skin (its URL and model) and cape (its URL); each `None` when they have none.
pub type Looks = (Option<(String, String)>, Option<String>);

/// What the skin system has for a player. A player with neither skin nor cape has `None` for both.
pub fn textures(base: &str, name: &str) -> Result<Looks, String> {
    let (status, v) = send(
        request_to("GET", &textures_url(base, name)),
        None,
        "Ely.by's skin system",
    )?;
    match status {
        // 204: no textures for this name.
        204 => Ok((None, None)),
        200 => {
            let url = |key: &str| v.get(key).and_then(|t| str_field(t, "url"));
            let model = v
                .pointer("/SKIN/metadata/model")
                .and_then(Value::as_str)
                .filter(|m| *m == "slim")
                .unwrap_or("classic");
            Ok((url("SKIN").map(|u| (u, model.to_owned())), url("CAPE")))
        }
        _ => Err("Ely.by's skin system did not answer.".into()),
    }
}

/// `account` with its skin and cape from the skin system. When it doesn't answer, the account
/// keeps the ones it has: a skin is never worth failing a sign-in or a renewal for.
pub(super) fn with_textures(base: &str, account: Account) -> Account {
    match textures(base, &account.name) {
        Ok((skin, cape)) => Account {
            skin,
            cape,
            ..account
        },
        Err(_) => account,
    }
}
