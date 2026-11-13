//! The Ely.by sign-in against a local mock of its authserver and skin system (recorded response
//! shapes, mock tokens only), the older launchers' Microsoft accounts, and the secret stores.
use super::*;

use std::io::{BufRead, BufReader, Read, Write};

use std::net::TcpListener;

use std::sync::Arc;

const UUID: &str = "069a79f444e94726a5befca90e38aaf5";
/// The client token every test account's access token was issued to.
const CLIENT: &str = "client-1";

/// What the mock answers and what it was asked.
#[derive(Default)]
struct Mock {
    /// Ely.by asks the account for a two-factor code.
    ely_two_factor: bool,
    /// The session server refuses `join` (the token was revoked since it was renewed).
    join_refused: bool,
    /// The UUID and name Ely.by answers with, instead of Notch's.
    profile: Option<(String, String)>,
    /// The skin system has no textures for any player (204).
    no_textures: bool,
    /// The skin system doesn't answer (500).
    skins_down: bool,
    /// Every request: method, path and body.
    log: Vec<(String, String, String)>,
}

fn reply(status: u16, body: Value) -> (u16, String) {
    (status, body.to_string())
}

fn ely_session(mock: &Mock, access: &str, client_token: &Value) -> (u16, String) {
    let (id, name) = mock.profile.clone().unwrap_or((
        "069a79f4-44e9-4726-a5be-fca90e38aaf5".into(),
        "Notch".into(),
    ));
    reply(
        200,
        json!({"accessToken": access, "clientToken": client_token, "selectedProfile": {"id": id, "name": name}}),
    )
}

/// Where the skin system keeps a player's textures: this, then the name.
const TEXTURES: &str = "/skins/textures/";

fn route(mock: &mut Mock, method: &str, path: &str, body: &str) -> (u16, String) {
    match (method, path) {
        ("POST", "/ely/auth/authenticate") => {
            let v: Value = serde_json::from_str(body).unwrap();
            let password = v["password"].as_str().unwrap_or_default();
            if password == "hunter2:123456" || (password == "hunter2" && !mock.ely_two_factor) {
                ely_session(mock, "ely-access-1", &v["clientToken"])
            } else if password == "hunter2" {
                reply(
                    401,
                    json!({"error": "ForbiddenOperationException", "errorMessage": "Account protected with two factor auth."}),
                )
            } else {
                reply(
                    401,
                    json!({"error": "ForbiddenOperationException", "errorMessage": "Invalid credentials. Invalid username or password."}),
                )
            }
        }
        ("POST", "/ely/auth/refresh") => {
            let v: Value = serde_json::from_str(body).unwrap();
            if v["accessToken"] == "revoked" {
                reply(
                    401,
                    json!({"error": "ForbiddenOperationException", "errorMessage": "Invalid token."}),
                )
            } else {
                ely_session(mock, "ely-renewed", &v["clientToken"])
            }
        }
        ("POST", "/ely/auth/invalidate") => (204, String::new()),
        ("POST", "/ely/session/join") => {
            let v: Value = serde_json::from_str(body).unwrap();
            if mock.join_refused {
                return reply(403, json!({"error": "ForbiddenOperationException"}));
            }
            assert_eq!(v["accessToken"], "ely-renewed");
            assert_eq!(
                (v["selectedProfile"].as_str(), v["serverId"].as_str()),
                (Some(UUID), Some("svc-42"))
            );
            (204, String::new())
        }
        ("GET", p) if p.starts_with(TEXTURES) => {
            let name = &p[TEXTURES.len()..];
            if mock.skins_down {
                reply(500, json!({"error": "Internal Server Error"}))
            } else if mock.no_textures {
                (204, String::new())
            } else {
                reply(
                    200,
                    json!({
                        "SKIN": {"url": format!("http://skins.test/{name}.png"), "metadata": {"model": "slim"}},
                        "CAPE": {"url": format!("http://skins.test/{name}-cape.png")},
                    }),
                )
            }
        }
        _ => reply(404, json!({"error": "NotFound"})),
    }
}

/// A one-thread HTTP/1.1 server: one request per connection.
fn serve(mock: Arc<Mutex<Mock>>) -> Endpoints {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let mut parts = line.split_whitespace();
            let (method, path) = (
                parts.next().unwrap_or("").to_owned(),
                parts.next().unwrap_or("").to_owned(),
            );
            let mut length = 0;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim_end();
                if header.is_empty() {
                    break;
                }
                let (k, v) = header.split_once(':').unwrap();
                if k.eq_ignore_ascii_case("content-length") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            let (status, text) = {
                let mut m = mock.lock().unwrap();
                // A bare `GET /` is a local tool probing every new port (T3 Code does, on the Mac
                // mini), not the launcher: no endpoint here is the root.
                if (method.as_str(), path.as_str()) != ("GET", "/") {
                    m.log.push((method.clone(), path.clone(), body.clone()));
                }
                route(&mut m, &method, &path, &body)
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}",
                text.len()
            );
        }
    });
    Endpoints {
        ely: format!("{base}/ely"),
        skins: format!("{base}/skins"),
    }
}

fn mock(f: impl FnOnce(&mut Mock)) -> (Arc<Mutex<Mock>>, Endpoints) {
    let mut m = Mock::default();
    f(&mut m);
    let m = Arc::new(Mutex::new(m));
    let ep = serve(m.clone());
    (m, ep)
}

/// The paths the mock was asked for, in order.
fn paths(m: &Mutex<Mock>) -> Vec<String> {
    let m = m.lock().unwrap();
    m.log.iter().map(|(_, p, _)| p.clone()).collect()
}

/// The mock `Accounts::remove` ends sessions at: it asks the address in `HORIZON_ELY_AUTH`, which
/// is set once for every test of this process so that none of them reaches the real authserver.
fn sign_out_mock() -> &'static Arc<Mutex<Mock>> {
    static MOCK: OnceLock<Arc<Mutex<Mock>>> = OnceLock::new();
    MOCK.get_or_init(|| {
        let (m, ep) = mock(|_| {});
        std::env::set_var("HORIZON_ELY_AUTH", &ep.ely);
        m
    })
}

#[test]
fn an_elyby_sign_in_asks_for_the_code_then_gives_the_session() {
    let (m, ep) = mock(|m| m.ely_two_factor = true);
    assert_eq!(
        elyby::authenticate(&ep.ely, "Notch", "hunter2", None, CLIENT)
            .err()
            .as_deref(),
        Some(elyby::CODE_NEEDED)
    );
    assert_eq!(
        elyby::authenticate(&ep.ely, "Notch", "hunter2", Some("000000"), CLIENT)
            .err()
            .as_deref(),
        Some(elyby::WRONG_CODE)
    );
    assert_eq!(
        elyby::authenticate(&ep.ely, "Notch", "nope", None, CLIENT)
            .err()
            .as_deref(),
        Some(elyby::WRONG_CREDENTIALS)
    );
    let tokens = elyby::authenticate(&ep.ely, "Notch", "hunter2", Some("123456"), CLIENT).unwrap();
    assert_eq!(
        (
            tokens.uuid.as_str(),
            tokens.name.as_str(),
            tokens.access.as_str(),
            tokens.client_token.as_str()
        ),
        (UUID, "Notch", "ely-access-1", CLIENT)
    );
    let s = elyby::secrets(&tokens);
    assert_eq!(
        (
            s.minecraft_token.as_str(),
            s.refresh_token.as_str(),
            s.sign_in_required
        ),
        ("ely-access-1", CLIENT, false)
    );
    assert!(s.minecraft_expires_at > now() + RENEW_BEFORE);
    // The code goes to Ely.by after the password, as one: `password:code`.
    let sent: Value = serde_json::from_str(&m.lock().unwrap().log.last().unwrap().2).unwrap();
    assert_eq!(
        (
            sent["username"].as_str(),
            sent["password"].as_str(),
            sent["clientToken"].as_str()
        ),
        (Some("Notch"), Some("hunter2:123456"), Some(CLIENT))
    );
    // Nothing listening: a message, not a URL.
    assert_eq!(
        elyby::authenticate(&offline().ely, "Notch", "hunter2", None, CLIENT).unwrap_err(),
        "Could not reach Ely.by. Check your internet connection."
    );
}

#[test]
fn an_elyby_sign_in_keeps_the_account_its_tokens_and_its_skin() {
    let (m, ep) = mock(|_| {});
    let store = MemoryStore::default();
    let mut accounts = Accounts::default();
    let account = elyby_sign_in_with(&ep, &store, &mut accounts, "Notch", "hunter2", None).unwrap();
    // The skin, its model and the cape come from the skin system, by the name Ely.by gave.
    assert_eq!(
        account,
        Account {
            uuid: UUID.into(),
            name: "Notch".into(),
            skin: Some(("http://skins.test/Notch.png".into(), "slim".into())),
            cape: Some("http://skins.test/Notch-cape.png".into()),
        }
    );
    assert_eq!(accounts.accounts, vec![account.clone()]);
    assert_eq!(accounts.selected(), Some(&account));
    // The access token is what the game plays with; the client token is the one sent at sign-in,
    // a fresh one for this sign-in (16 random bytes in hex).
    let sent: Value = serde_json::from_str(&m.lock().unwrap().log[0].2).unwrap();
    let saved = kept(&store);
    assert_eq!(saved.minecraft_token, "ely-access-1");
    assert_eq!(
        Some(saved.refresh_token.as_str()),
        sent["clientToken"].as_str()
    );
    assert_eq!(saved.refresh_token.len(), 32);
    assert!(!saved.sign_in_required && saved.minecraft_expires_at > now() + RENEW_BEFORE);
    // Never in accounts.json.
    let file = serde_json::to_string(&accounts).unwrap();
    assert!(!file.contains("ely-access-1") && !file.contains(&saved.refresh_token));
    assert_eq!(
        paths(&m),
        ["/ely/auth/authenticate", "/skins/textures/Notch"]
    );
}

#[test]
fn signing_in_selects_the_account_and_signing_in_again_replaces_it() {
    let (_, ep) = mock(|_| {});
    let store = MemoryStore::default();
    let mut accounts = Accounts::default();
    accounts
        .add(&store, other("Alex", "other"), &secrets("other-token"))
        .unwrap();
    elyby_sign_in_with(&ep, &store, &mut accounts, "Notch", "hunter2", None).unwrap();
    let names = |a: &Accounts| {
        a.accounts
            .iter()
            .map(|a| a.name.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(names(&accounts), ["Alex", "Notch"]);
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
    // The same account again: its entry is replaced, with a new client token.
    let first = kept(&store).refresh_token;
    elyby_sign_in_with(&ep, &store, &mut accounts, "Notch", "hunter2", None).unwrap();
    assert_eq!(names(&accounts), ["Alex", "Notch"]);
    assert_ne!(kept(&store).refresh_token, first);
    // The other account's sign-in is untouched.
    assert_eq!(
        secrets_in(&store, "other").unwrap().minecraft_token,
        "other-token"
    );
}

#[test]
fn a_failed_elyby_sign_in_keeps_nothing() {
    let (_, ep) = mock(|m| m.ely_two_factor = true);
    let store = MemoryStore::default();
    let mut accounts = Accounts::default();
    let sign_in = |accounts: &mut Accounts, password: &str, code: Option<&str>| {
        elyby_sign_in_with(&ep, &store, accounts, "Notch", password, code)
    };
    assert_eq!(
        sign_in(&mut accounts, "hunter2", None).unwrap_err(),
        elyby::CODE_NEEDED
    );
    assert_eq!(
        sign_in(&mut accounts, "nope", None).unwrap_err(),
        elyby::WRONG_CREDENTIALS
    );
    assert_eq!(
        sign_in(&mut accounts, "hunter2", Some("1")).unwrap_err(),
        elyby::WRONG_CODE
    );
    let none = elyby_sign_in_with(&offline(), &store, &mut accounts, "Notch", "hunter2", None);
    assert_eq!(
        none.unwrap_err(),
        "Could not reach Ely.by. Check your internet connection."
    );
    assert_eq!(store.get(UUID).unwrap(), None);
    assert_eq!(accounts, Accounts::default());
    // The same call with the code signs in.
    let account = sign_in(&mut accounts, "hunter2", Some("123456")).unwrap();
    assert_eq!(account.name, "Notch");
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
}

#[test]
fn a_skin_system_with_nothing_or_nothing_to_say_never_fails_a_sign_in() {
    // 204: no textures for this player.
    let (m, ep) = mock(|m| m.no_textures = true);
    let mut accounts = Accounts::default();
    let account = elyby_sign_in_with(
        &ep,
        &MemoryStore::default(),
        &mut accounts,
        "Notch",
        "hunter2",
        None,
    )
    .unwrap();
    assert_eq!((account.skin, account.cape), (None, None));
    assert!(paths(&m).contains(&"/skins/textures/Notch".to_string()));
    // A skin system that fails, or isn't there at all: the account signs in without a skin.
    let (_, down) = mock(|m| m.skins_down = true);
    let gone = Endpoints {
        skins: offline().skins,
        ..down.clone()
    };
    for ep in [down, gone] {
        let store = MemoryStore::default();
        let mut accounts = Accounts::default();
        let account =
            elyby_sign_in_with(&ep, &store, &mut accounts, "Notch", "hunter2", None).unwrap();
        assert_eq!(
            (account.name.as_str(), account.skin, account.cape),
            ("Notch", None, None)
        );
        assert_eq!(accounts.selected.as_deref(), Some(UUID));
        assert_eq!(kept(&store).minecraft_token, "ely-access-1");
    }
}

#[test]
fn signing_out_ends_the_session_at_ely_by() {
    let m = sign_out_mock();
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("signing-out-token"));
    accounts.remove(&store, UUID).unwrap();
    assert_eq!(store.get(UUID).unwrap(), None);
    assert!(accounts.accounts.is_empty() && accounts.selected.is_none());
    let log = m.lock().unwrap().log.clone();
    let ended: Vec<Value> = log
        .iter()
        .filter(|(_, p, b)| p == "/ely/auth/invalidate" && b.contains("signing-out-token"))
        .map(|(_, _, b)| serde_json::from_str(b).unwrap())
        .collect();
    assert_eq!(ended.len(), 1);
    assert_eq!(
        (
            ended[0]["accessToken"].as_str(),
            ended[0]["clientToken"].as_str()
        ),
        (Some("signing-out-token"), Some(CLIENT))
    );
}

fn notch() -> Account {
    Account {
        uuid: UUID.into(),
        name: "Notch".into(),
        skin: None,
        cape: None,
    }
}

/// Another signed-in player's profile.
fn other(name: &str, uuid: &str) -> Account {
    Account {
        uuid: uuid.into(),
        name: name.into(),
        skin: None,
        cape: None,
    }
}

/// A signed-in account's secrets: a token good for two hours.
fn secrets(token: &str) -> Secrets {
    Secrets {
        refresh_token: CLIENT.into(),
        sign_in_required: false,
        minecraft_token: token.into(),
        minecraft_expires_at: now() + 7200,
    }
}

/// [`secrets`] whose token has run out.
fn expired(token: &str) -> Secrets {
    Secrets {
        minecraft_expires_at: 0,
        ..secrets(token)
    }
}

/// Nothing listens there: every request fails as with no network.
fn offline() -> Endpoints {
    Endpoints {
        ely: "http://127.0.0.1:9/ely".into(),
        skins: "http://127.0.0.1:9/skins".into(),
    }
}

#[test]
fn refresh_only_when_the_token_runs_out() {
    let (m, ep) = mock(|_| {});
    let Check::Ready(account, held) = check(&ep, &notch(), secrets("ely-access-1")) else {
        panic!("ready");
    };
    // The token had two hours left: no request, nothing changed.
    assert!(m.lock().unwrap().log.is_empty());
    assert_eq!(
        (account, held.minecraft_token.as_str()),
        (notch(), "ely-access-1")
    );
    let stale = Secrets {
        minecraft_expires_at: now() + 60,
        ..held
    };
    let Check::Ready(renewed_account, renewed) = check(&ep, &notch(), stale) else {
        panic!("ready");
    };
    assert_eq!(
        (
            renewed.refresh_token.as_str(),
            renewed.minecraft_token.as_str()
        ),
        (CLIENT, "ely-renewed")
    );
    assert!(renewed.minecraft_expires_at > now() + 80000);
    // The skin system's textures come with the renewal.
    assert_eq!(
        renewed_account.skin,
        Some(("http://skins.test/Notch.png".into(), "slim".into()))
    );
    assert_eq!(paths(&m), ["/ely/auth/refresh", "/skins/textures/Notch"]);
    let sent: Value = serde_json::from_str(&m.lock().unwrap().log[0].2).unwrap();
    assert_eq!(
        (sent["accessToken"].as_str(), sent["clientToken"].as_str()),
        (Some("ely-access-1"), Some(CLIENT))
    );
    // Ely.by refuses the token: the account is sent back to sign in, and remembered so.
    assert!(matches!(
        check(&ep, &notch(), expired("revoked")),
        Check::NoAnswer(e, s) if e == elyby::EXPIRED && s.sign_in_required
    ));
}

#[test]
fn a_renewal_that_names_another_profile_sends_the_account_back_to_sign_in() {
    let (_, ep) = mock(|m| m.profile = Some(("1".repeat(32), "Eve".into())));
    let Check::NoAnswer(said, held) = check(&ep, &notch(), expired("ely-access-1")) else {
        panic!("another profile's session never plays as this account");
    };
    assert_eq!(said, elyby::OTHER_PROFILE);
    assert!(held.sign_in_required);
    assert_eq!(held.minecraft_token, "ely-access-1");
}

/// An accounts.json with Notch signed in and playing, his secrets in `store`.
fn signed_in(store: &MemoryStore, secrets: &Secrets) -> Accounts {
    let mut accounts = Accounts::default();
    accounts.add(store, notch(), secrets).unwrap();
    accounts
}

/// An accounts.json that is never there: a refresh in these tests saves no profile, and never
/// touches the developer's own launcher data.
fn nowhere() -> PathBuf {
    std::env::temp_dir()
        .join(format!(
            "horizon-launcher-no-accounts-{}",
            std::process::id()
        ))
        .join("accounts.json")
}

fn kept(store: &MemoryStore) -> Secrets {
    secrets_in(store, UUID).unwrap()
}

/// An accounts.json of the test's own in a folder of its own, gone first.
fn accounts_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("horizon-accounts-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("accounts.json")
}

#[test]
fn splitscreen_gets_the_other_accounts_that_still_hold() {
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token"));
    for (name, uuid, s) in [
        ("Bea", "b", secrets("bea-token")),
        ("Cal", "c", expired("old")),
    ] {
        accounts.add(&store, other(name, uuid), &s).unwrap();
    }
    // Dan's sign-in is gone from the keychain.
    accounts.accounts.push(other("Dan", "d"));
    accounts.selected = Some(UUID.into());
    let dir = std::env::temp_dir().join(format!("hz-split-sessions-{}", std::process::id()));
    // No network: Cal can't be renewed and Dan has no sign-in; neither stops the launch.
    let env = account_sessions(&offline(), &store, &dir, &mut accounts);
    let (key, json) = &env[0];
    assert_eq!(*key, "HORIZON_ACCOUNT_SESSIONS");
    let list: Vec<Value> = serde_json::from_str(json).unwrap();
    assert_eq!(list.len(), 3);
    assert_eq!(list[0]["name"], "Bea");
    for (n, name) in [(1, "Cal"), (2, "Dan")] {
        assert_eq!(list[n]["name"], name);
        assert!(list[n]["skipped"]
            .as_str()
            .unwrap()
            .contains("Sign in again"));
        assert!(list[n].get("file").is_none());
    }
    assert!(!json.contains("access_token") && !json.contains("bea-token"));
    let path = PathBuf::from(list[0]["file"].as_str().unwrap());
    let file = std::fs::read_to_string(&path).unwrap();
    // Only the access token goes: the client token stays in the keychain.
    assert_eq!(
        serde_json::from_str::<Value>(&file).unwrap(),
        json!({"name": "Bea", "uuid": "b", "access_token": "bea-token", "user_type": "elyby", "xuid": null})
    );
    assert!(!file.contains("mc-token") && !file.contains(CLIENT));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "only the player reads it");
        assert_eq!(mode(&dir), 0o700, "nor lists the folder");
    }
    let _ = std::fs::remove_dir_all(&dir);
    // Alone: nothing.
    let mut alone = signed_in(&store, &secrets("mc-token"));
    let env = account_sessions(&offline(), &store, &dir, &mut alone);
    assert!(env.is_empty());
    // Only for a game whose Splitscreen setting is on.
    std::fs::create_dir_all(&dir).unwrap();
    assert!(!splitscreen_on(&dir));
    std::fs::write(
        dir.join("horizon-extras.txt"),
        "zoom:false\nsplitscreen:false\n",
    )
    .unwrap();
    assert!(!splitscreen_on(&dir));
    std::fs::write(
        dir.join("horizon-extras.txt"),
        "zoom:false\nsplitscreen:true\n",
    )
    .unwrap();
    assert!(splitscreen_on(&dir));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn splitscreen_renews_an_account_whose_token_is_due() {
    let (m, ep) = mock(|m| m.profile = Some(("b".into(), "Bea".into())));
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token"));
    accounts
        .add(&store, other("Bea", "b"), &expired("old"))
        .unwrap();
    accounts.selected = Some(UUID.into());
    let dir = std::env::temp_dir().join(format!("hz-split-renew-{}", std::process::id()));
    let env = account_sessions(&ep, &store, &dir, &mut accounts);
    let list: Vec<Value> = serde_json::from_str(&env[0].1).unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["name"], "Bea");
    let file = std::fs::read_to_string(list[0]["file"].as_str().unwrap()).unwrap();
    let given: Value = serde_json::from_str(&file).unwrap();
    assert_eq!(given["access_token"], "ely-renewed");
    // Kept for the next launch, with the profile as Ely.by has it now.
    assert_eq!(
        secrets_in(&store, "b").unwrap().minecraft_token,
        "ely-renewed"
    );
    assert_eq!(
        accounts.accounts[1].skin,
        Some(("http://skins.test/Bea.png".into(), "slim".into()))
    );
    assert_eq!(paths(&m), ["/ely/auth/refresh", "/skins/textures/Bea"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn splitscreen_never_hands_over_an_account_sent_back_to_sign_in() {
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token"));
    // A token with hours left, for an account the services sent back to sign-in (a refused
    // refresh, or a token that now names another profile).
    let refused = Secrets {
        sign_in_required: true,
        ..secrets("other-profile-token")
    };
    let eve = Account {
        uuid: "e".into(),
        name: "Eve".into(),
        skin: None,
        cape: None,
    };
    accounts.add(&store, eve, &refused).unwrap();
    accounts.selected = Some(UUID.into());
    let dir = std::env::temp_dir().join(format!("hz-split-refused-{}", std::process::id()));
    // With time to renew, and out of time: never offered either way.
    for deadline in [
        std::time::Instant::now() + std::time::Duration::from_secs(10),
        std::time::Instant::now(),
    ] {
        let env = account_sessions_until(&offline(), &store, &dir, &mut accounts, deadline);
        let list: Vec<Value> = serde_json::from_str(&env[0].1).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["name"], "Eve");
        assert!(list[0].get("file").is_none());
        assert!(list[0]["skipped"]
            .as_str()
            .unwrap()
            .contains("Sign in again"));
    }
    let written = std::fs::read_dir(&dir).map_or(0, |d| d.count());
    assert_eq!(written, 0, "no session file for it");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unread_session_files_go_once_the_game_would_refuse_them() {
    let dir = std::env::temp_dir().join(format!("hz-session-reap-{}", std::process::id()));
    let secrets = secrets("not-a-real-token");
    let waiting = write_session_file(&dir, &notch(), &secrets).unwrap();
    let stale = write_session_file(&dir, &notch(), &secrets).unwrap();
    let read = write_session_file(&dir, &notch(), &secrets).unwrap();
    // A game that never started: its file is past the age the game takes.
    std::fs::File::options()
        .write(true)
        .open(&stale)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(300))
        .unwrap();
    // A game that started took its file (and deleted it).
    std::fs::remove_file(&read).unwrap();
    let mut unread = vec![waiting.clone(), stale.clone(), read];
    reap(&mut unread, SESSION_FILE_LIFE);
    assert!(waiting.exists(), "a game may still take it");
    assert!(!stale.exists(), "no token left behind");
    assert_eq!(unread, std::slice::from_ref(&waiting));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_token_that_holds_plays_without_a_request() {
    let (m, ep) = mock(|_| {});
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token"));
    let pass = launch(&ep, &store, &mut accounts).unwrap();
    let (account, session) = pass.session.unwrap();
    assert_eq!(
        (account.name.as_str(), session.minecraft_token.as_str()),
        ("Notch", "mc-token")
    );
    assert!(pass.offline.is_none() && pass.unsaved.is_none());
    assert!(m.lock().unwrap().log.is_empty());
}

#[test]
fn a_due_token_is_renewed_and_the_profile_follows() {
    let (m, ep) = mock(|m| m.profile = Some((UUID.into(), "Notch_2".into())));
    let store = MemoryStore::default();
    let due = Secrets {
        minecraft_expires_at: now() + 60,
        ..secrets("old-token")
    };
    let mut accounts = signed_in(&store, &due);
    let pass = launch(&ep, &store, &mut accounts).unwrap();
    let (account, session) = pass.session.unwrap();
    assert_eq!(
        (account.name.as_str(), session.minecraft_token.as_str()),
        ("Notch_2", "ely-renewed")
    );
    // The skin system is asked for the name Ely.by gave now.
    assert_eq!(
        account.skin,
        Some(("http://skins.test/Notch_2.png".into(), "slim".into()))
    );
    assert_eq!(
        account.cape.as_deref(),
        Some("http://skins.test/Notch_2-cape.png")
    );
    assert_eq!(accounts.selected(), Some(&account));
    assert_eq!(kept(&store).minecraft_token, "ely-renewed");
    assert_eq!(paths(&m), ["/ely/auth/refresh", "/skins/textures/Notch_2"]);
    // The caller saves the accounts: the next start has the new name and skin.
    let file = accounts_file("launch-renewal");
    accounts.save_to(&file).unwrap();
    let saved = Accounts::load_from(&file);
    assert_eq!(saved.accounts, vec![account]);
    assert_eq!(saved.selected.as_deref(), Some(UUID));
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[test]
fn nobody_signed_in_means_no_game() {
    let (m, ep) = mock(|_| {});
    let refused = launch(&ep, &MemoryStore::default(), &mut Accounts::default()).unwrap_err();
    assert_eq!(refused, NoAccount::default());
    assert_eq!(
        refused.to_string(),
        "Sign in with an Ely.by account to play."
    );
    assert!(m.lock().unwrap().log.is_empty());
}

#[test]
fn an_account_without_its_secrets_is_no_longer_signed_in() {
    let (m, ep) = mock(|_| {});
    let mut accounts = Accounts {
        accounts: vec![notch()],
        ..Accounts::default()
    };
    let refused = launch(&ep, &MemoryStore::default(), &mut accounts).unwrap_err();
    assert_eq!(
        refused.keyring.as_deref(),
        Some("Notch is no longer signed in on this computer. Sign in again.")
    );
    // The first account plays when none is chosen.
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
    assert!(m.lock().unwrap().log.is_empty());
}

#[test]
fn no_network_never_stops_a_token_that_holds() {
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token"));
    let pass = launch(&offline(), &store, &mut accounts).unwrap();
    assert_eq!(pass.session.unwrap().1.minecraft_token, "mc-token");
    assert!(pass.offline.is_none());
}

#[test]
fn an_expired_token_with_no_network_plays_offline_with_the_last_tokens() {
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &expired("last-token"));
    let pass = launch(&offline(), &store, &mut accounts).unwrap();
    // Plays, offline, and says why.
    assert!(pass.session.is_none());
    assert_eq!(
        pass.offline.as_deref(),
        Some("Could not reach Ely.by. Check your internet connection.")
    );
    // Nothing was lost: the sign-in isn't sent back, and renews when the network is.
    assert_eq!(kept(&store).minecraft_token, "last-token");
    assert!(!kept(&store).sign_in_required);
    let (_, ep) = mock(|_| {});
    let pass = launch(&ep, &store, &mut accounts).unwrap();
    assert_eq!(pass.session.unwrap().1.minecraft_token, "ely-renewed");
}

#[test]
fn the_selected_account_plays_and_none_selected_plays_the_first() {
    let (m, ep) = mock(|_| {});
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token"));
    // A second account, no longer in the keychain, plays: Notch's sign-in doesn't stand in for it.
    accounts.accounts.push(other("Alex", "other"));
    accounts.selected = Some("other".into());
    let refused = launch(&ep, &store, &mut accounts).unwrap_err();
    assert!(refused
        .keyring
        .unwrap()
        .starts_with("Alex is no longer signed in"));
    // Nobody selected: the first account plays.
    accounts.selected = None;
    let pass = launch(&ep, &store, &mut accounts).unwrap();
    assert_eq!(pass.session.unwrap().0.name, "Notch");
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
    assert!(m.lock().unwrap().log.is_empty());
}

/// A keychain that reads but refuses to keep anything new (#3249).
struct Refusing(MemoryStore);

impl SecretStore for Refusing {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        self.0.get(key)
    }
    fn set(&self, _: &str, _: &str) -> Result<(), String> {
        Err("the keyring is locked".into())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.0.delete(key)
    }
    fn persistent(&self) -> bool {
        true
    }
    fn operations(&self) -> &Mutex<()> {
        self.0.operations()
    }
}

#[test]
fn a_renewed_sign_in_the_keychain_refuses_still_plays_and_says_so() {
    let (_, ep) = mock(|_| {});
    let store = MemoryStore::default();
    let stale = Secrets {
        minecraft_expires_at: now() + 60,
        ..secrets("old-token")
    };
    let mut accounts = signed_in(&store, &stale);
    let refusing = Refusing(store);
    let pass = launch(&ep, &refusing, &mut accounts).unwrap();
    assert_eq!(pass.session.unwrap().1.minecraft_token, "ely-renewed");
    let said = "Notch's renewed sign-in couldn't be saved in the keychain (the keyring is \
                locked): you may have to sign in again next time.";
    assert_eq!(pass.unsaved.as_deref(), Some(said));
    assert_eq!(take_unsaved().as_deref(), Some(said));
    assert_eq!(take_unsaved(), None, "said once");
    // Nothing to save, nothing to say.
    let mut accounts = signed_in(&refusing.0, &secrets("mc-token"));
    let pass = launch(&ep, &refusing, &mut accounts).unwrap();
    assert!(pass.unsaved.is_none());
}

#[test]
fn files_from_launchers_with_offline_names_load_and_lose_them_on_save() {
    let dir = std::env::temp_dir().join(format!("horizon-old-offline-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("skins")).unwrap();
    let store = MemoryStore::default();
    let mut old = serde_json::to_value(signed_in(&store, &secrets("mc-token"))).unwrap();
    // A file without a version: its Ely.by accounts say so.
    old["accounts"][0]["provider"] = json!("ely");
    old["selected"] = serde_json::Value::Null;
    old["offline"] = serde_json::json!(["Builder_1"]);
    old["offline_selected"] = serde_json::json!("Builder_1");
    old["looks"] =
        serde_json::json!({ "builder_1": { "skin": true, "cape": true, "model": "slim" } });
    let path = dir.join("accounts.json");
    std::fs::write(&path, old.to_string()).unwrap();
    for file in ["builder_1.png", "builder_1.cape.png"] {
        std::fs::write(dir.join("skins").join(file), b"png").unwrap();
    }
    let mut accounts = Accounts::load_from(&path);
    assert_eq!(accounts.accounts, vec![notch()]);
    assert!(accounts.selected().is_none());
    // The account plays, and the save drops the names, their looks and their files.
    let (_, ep) = mock(|_| {});
    let pass = launch(&ep, &store, &mut accounts).unwrap();
    assert_eq!(pass.session.unwrap().0.uuid, UUID);
    accounts.save_to(&path).unwrap();
    let saved: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        saved,
        serde_json::json!({ "accounts": accounts.accounts, "selected": UUID, "version": 2 })
    );
    assert!(!dir.join("skins").exists());
    // The developer option of 0.2.58 reads as nothing and is not written back.
    let settings = crate::backend::Settings::parse(r#"{"dev_offline": true, "keep_versions": 3}"#);
    let saved = serde_json::to_value(&settings).unwrap();
    assert!(saved.get("dev_offline").is_none(), "{saved}");
    assert_eq!(saved["keep_versions"], 3);
    let _ = std::fs::remove_dir_all(&dir);
}

const ALEX: &str = "0f22b7ed5e3d4a2c9bfe2f6f8f1f6a01";
const STEVE: &str = "8667ba71b85a4004af54457a9734eed7";

/// An accounts.json an older launcher wrote, without a `version`: two Microsoft accounts (one
/// with no `provider`, one that says so) beside an Ely.by one; a Microsoft account plays.
fn old_launchers_accounts() -> Value {
    json!({
        "accounts": [
            {"uuid": ALEX, "name": "Alex", "skin": null, "cape": null},
            {"provider": "ely", "uuid": UUID, "name": "Notch", "skin": ["http://skins.test/Notch.png", "slim"], "cape": null},
            {"provider": "microsoft", "uuid": STEVE, "name": "Steve", "skin": null, "cape": null},
        ],
        "selected": ALEX,
    })
}

fn microsoft(uuid: &str, name: &str) -> Microsoft {
    Microsoft {
        uuid: uuid.into(),
        name: name.into(),
    }
}

#[test]
fn an_accounts_file_without_a_version_loads_only_its_ely_accounts() {
    let path = accounts_file("legacy-load");
    std::fs::write(&path, old_launchers_accounts().to_string()).unwrap();
    let accounts = Accounts::load_from(&path);
    assert_eq!(
        accounts.accounts,
        vec![Account {
            uuid: UUID.into(),
            name: "Notch".into(),
            skin: Some(("http://skins.test/Notch.png".into(), "slim".into())),
            cape: None,
        }]
    );
    // The Microsoft accounts are only remembered, in file order, until their items are deleted.
    assert_eq!(
        accounts.microsoft().to_vec(),
        vec![microsoft(ALEX, "Alex"), microsoft(STEVE, "Steve")]
    );
    // The player was a Microsoft account: nobody is selected now.
    assert_eq!(accounts.selected, None);
    assert!(accounts.selected().is_none());
    // An Ely.by account that was selected stays selected.
    let mut file = old_launchers_accounts();
    file["selected"] = json!(UUID);
    std::fs::write(&path, file.to_string()).unwrap();
    let accounts = Accounts::load_from(&path);
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
    assert_eq!(accounts.microsoft().len(), 2);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn dropping_the_microsoft_accounts_deletes_their_keychain_items() {
    let path = accounts_file("legacy-drop");
    std::fs::write(&path, old_launchers_accounts().to_string()).unwrap();
    let store = MemoryStore::default();
    store.set(ALEX, "alex-microsoft-tokens").unwrap();
    store.set(STEVE, "steve-microsoft-tokens").unwrap();
    store
        .set(UUID, &serde_json::to_string(&secrets("ely-token")).unwrap())
        .unwrap();
    let mut accounts = Accounts::load_from(&path);
    // Saved before the items are deleted, the accounts are still remembered: the next start
    // deletes them.
    accounts.save_to(&path).unwrap();
    assert_eq!(Accounts::load_from(&path).microsoft().len(), 2);
    let names = accounts.drop_microsoft(&store).unwrap();
    assert_eq!(names, ["Alex", "Steve"]);
    assert_eq!(
        (store.get(ALEX).unwrap(), store.get(STEVE).unwrap()),
        (None, None)
    );
    // The Ely.by account's sign-in is untouched.
    assert_eq!(kept(&store).minecraft_token, "ely-token");
    assert!(accounts.microsoft().is_empty());
    assert_eq!(
        accounts.drop_microsoft(&store).unwrap(),
        Vec::<String>::new()
    );
    // Saved, nothing of them is left, and the file is the launcher's version 2.
    accounts.save_to(&path).unwrap();
    let file: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(file["version"], 2);
    assert!(file.get("microsoft").is_none());
    assert_eq!(file["accounts"].as_array().unwrap().len(), 1);
    let again = Accounts::load_from(&path);
    assert!(again.microsoft().is_empty());
    assert_eq!(again.accounts.len(), 1);
    assert_eq!(again.accounts[0].name, "Notch");
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

/// A keychain that reads and writes but refuses to delete anything.
struct Undeletable(MemoryStore);
impl SecretStore for Undeletable {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        self.0.get(key)
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        self.0.set(key, secret)
    }
    fn delete(&self, _: &str) -> Result<(), String> {
        Err("the keyring is locked".into())
    }
    fn persistent(&self) -> bool {
        true
    }
    fn operations(&self) -> &Mutex<()> {
        self.0.operations()
    }
}

#[test]
fn a_keychain_that_refuses_to_delete_keeps_the_microsoft_accounts_remembered() {
    let path = accounts_file("legacy-refused");
    std::fs::write(&path, old_launchers_accounts().to_string()).unwrap();
    let store = Undeletable(MemoryStore::default());
    store.set(ALEX, "alex-microsoft-tokens").unwrap();
    let mut accounts = Accounts::load_from(&path);
    assert_eq!(
        accounts.drop_microsoft(&store).unwrap_err(),
        "the keyring is locked"
    );
    // Nothing was forgotten: the next start tries again.
    assert_eq!(accounts.microsoft().len(), 2);
    assert!(store.get(ALEX).unwrap().is_some());
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn a_version_2_file_keeps_every_account_whatever_its_provider_field() {
    let path = accounts_file("version-2");
    let file = json!({
        "version": 2,
        "accounts": [
            {"provider": "microsoft", "uuid": ALEX, "name": "Alex", "skin": null, "cape": null},
            {"uuid": STEVE, "name": "Steve", "skin": null, "cape": null},
            {"provider": "ely", "uuid": UUID, "name": "Notch", "skin": null, "cape": null},
        ],
        "selected": ALEX,
    });
    std::fs::write(&path, file.to_string()).unwrap();
    let accounts = Accounts::load_from(&path);
    let names: Vec<&str> = accounts.accounts.iter().map(|a| a.name.as_str()).collect();
    assert_eq!(names, ["Alex", "Steve", "Notch"]);
    assert!(accounts.microsoft().is_empty());
    assert_eq!(accounts.selected.as_deref(), Some(ALEX));
    // Saved again, still all three.
    accounts.save_to(&path).unwrap();
    assert_eq!(Accounts::load_from(&path), accounts);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn accounts_keep_secrets_in_the_store_only() {
    sign_out_mock();
    let store = MemoryStore::default();
    let mut accounts = Accounts::default();
    let account = notch();
    let secrets = Secrets {
        refresh_token: "r".into(),
        sign_in_required: false,
        minecraft_token: "mc-secret".into(),
        minecraft_expires_at: 1,
    };
    accounts.add(&store, account.clone(), &secrets).unwrap();
    assert_eq!(accounts.selected(), Some(&account));
    assert_eq!(accounts.find("notch"), Some(&account));
    assert_eq!(
        accounts.find("069a79f4-44e9-4726-a5be-fca90e38aaf5"),
        Some(&account)
    );
    let saved = serde_json::to_string(&accounts).unwrap();
    assert!(!saved.contains("mc-secret") && !saved.contains("\"r\""));
    assert!(store.get(UUID).unwrap().unwrap().contains("mc-secret"));
    accounts.remove(&store, UUID).unwrap();
    assert_eq!(
        (accounts.selected(), store.get(UUID).unwrap()),
        (None, None)
    );
    assert!(!store.persistent());
}

/// The OS keychain itself: Credential Manager and the Keychain take one secret per item, and a
/// sign-in's secrets are longer than one item holds. Ignored by default: it writes to the keychain
/// (a test key, removed again).
#[cfg(any(target_os = "macos", target_os = "windows"))]
#[test]
#[ignore = "writes to the OS keychain"]
fn system_keychain_keeps_long_secrets() {
    let key = "test-long-secret";
    let secret = format!("{{\"refresh_token\":\"{}\"}}", "r".repeat(3000));
    SystemStore.set(key, &secret).unwrap();
    assert_eq!(SystemStore.get(key).unwrap(), Some(secret.clone()));
    SystemStore.delete(key).unwrap();
    assert_eq!(SystemStore.get(key).unwrap(), None);
}

#[test]
fn long_secrets_split_into_keychain_sized_pieces() {
    // Mixed 1-, 2- and 4-byte characters, so the cuts land inside some of them.
    let secret = "aé🔑".repeat(300);
    let pieces = split_secret(&secret);
    assert!(pieces.len() > 1);
    assert_eq!(pieces.concat(), secret);
    for piece in &pieces {
        assert!(piece.len() <= PIECE);
        assert!(piece.encode_utf16().count() * 2 <= 2560);
    }
    assert_eq!(split_secret("short"), vec!["short"]);
}

/// Keychain items in memory; writes fail once `room` items are kept, as a full Credential Manager
/// refuses new items while it still overwrites the ones it has.
struct FullItems {
    items: Mutex<HashMap<String, String>>,
    room: usize,
}

impl Items for FullItems {
    fn read(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.items.lock().unwrap().get(key).cloned())
    }
    fn write(&self, key: &str, piece: &str) -> Result<(), String> {
        let mut items = self.items.lock().unwrap();
        if !items.contains_key(key) && items.len() >= self.room {
            return Err(credential_manager_says(8).unwrap().into());
        }
        items.insert(key.into(), piece.into());
        Ok(())
    }
    fn remove(&self, key: &str) -> Result<bool, String> {
        Ok(self.items.lock().unwrap().remove(key).is_some())
    }
}

#[test]
fn a_secret_the_keychain_cant_finish_keeps_the_old_one() {
    let items = FullItems {
        items: Mutex::default(),
        room: 2,
    };
    let old = "o".repeat(PIECE + 1);
    set_pieces(&items, UUID, &old).unwrap();
    assert_eq!(get_pieces(&items, UUID).unwrap(), Some(old.clone()));
    // A renewal three pieces long: the first two overwrite, the third finds the store full.
    let new = "n".repeat(2 * PIECE + 1);
    let said = set_pieces(&items, UUID, &new).unwrap_err();
    assert_eq!(said, CREDENTIAL_MANAGER_FULL);
    assert_eq!(get_pieces(&items, UUID).unwrap(), Some(old));
    // A sign-in into a full store keeps nothing, and says why.
    let full = FullItems {
        items: Mutex::default(),
        room: 0,
    };
    assert_eq!(
        set_pieces(&full, UUID, "s").unwrap_err(),
        CREDENTIAL_MANAGER_FULL
    );
    assert_eq!(get_pieces(&full, UUID).unwrap(), None);
    // Room again: the longer secret replaces the old one whole.
    let items = FullItems { room: 3, ..items };
    set_pieces(&items, UUID, &new).unwrap();
    assert_eq!(get_pieces(&items, UUID).unwrap(), Some(new));
    set_pieces(&items, UUID, "short").unwrap();
    assert_eq!(get_pieces(&items, UUID).unwrap(), Some("short".into()));
    assert_eq!(items.items.lock().unwrap().len(), 1);
}

#[test]
fn a_split_secret_folds_into_one_item() {
    // The layout launchers before #4912 wrote, in memory.
    let items = FullItems {
        items: Mutex::default(),
        room: 99,
    };
    let secret = "s".repeat(2 * PIECE + 1);
    set_pieces(&items, UUID, &secret).unwrap();
    assert_eq!(items.items.lock().unwrap().len(), 3);
    // The first read answers the whole secret and leaves a single item behind.
    assert_eq!(get_item(&items, UUID).unwrap(), Some(secret.clone()));
    assert_eq!(items.items.lock().unwrap().len(), 1);
    assert_eq!(items.read(UUID).unwrap(), Some(secret));
    // A rewrite stays one item; a shorter secret leaves no pieces behind.
    set_item(&items, UUID, "short").unwrap();
    assert_eq!(items.items.lock().unwrap().len(), 1);
    assert_eq!(get_item(&items, UUID).unwrap(), Some("short".into()));
    delete_pieces(&items, UUID).unwrap();
    assert_eq!(get_item(&items, UUID).unwrap(), None);
}

/// Items that answer `left` removals, then refuse them, as a keychain denying a delete does.
struct RefusedDeletes {
    items: Mutex<HashMap<String, String>>,
    left: Mutex<usize>,
}

impl Items for RefusedDeletes {
    fn read(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.items.lock().unwrap().get(key).cloned())
    }
    fn write(&self, key: &str, piece: &str) -> Result<(), String> {
        self.items.lock().unwrap().insert(key.into(), piece.into());
        Ok(())
    }
    fn remove(&self, key: &str) -> Result<bool, String> {
        let mut left = self.left.lock().unwrap();
        if *left == 0 {
            return Err("the keychain refused the delete".into());
        }
        *left -= 1;
        Ok(self.items.lock().unwrap().remove(key).is_some())
    }
}

#[test]
fn a_refused_delete_puts_the_split_secret_back() {
    // The layout launchers before #4912 wrote, in memory.
    let old = "o".repeat(2 * PIECE + 1);
    let pieces: Vec<String> = split_secret(&old).iter().map(|p| p.to_string()).collect();
    for left in [0, 1] {
        let items = RefusedDeletes {
            items: Mutex::new(
                pieces
                    .iter()
                    .enumerate()
                    .map(|(n, p)| (piece_key(UUID, n), p.clone()))
                    .collect(),
            ),
            left: Mutex::new(left),
        };
        // The fold's clean-up is refused, halfway or at once: the read still answers the whole
        // secret, and every old item is back behind it.
        assert_eq!(get_item(&items, UUID).unwrap(), Some(old.clone()));
        assert_eq!(items.items.lock().unwrap().len(), pieces.len());
        assert_eq!(get_item(&items, UUID).unwrap(), Some(old.clone()));
        // A writer's set over the split layout fails the same way, and reads back the same.
        *items.left.lock().unwrap() = left;
        assert_eq!(
            set_item(&items, UUID, "new").unwrap_err(),
            "the keychain refused the delete"
        );
        assert_eq!(get_item(&items, UUID).unwrap(), Some(old.clone()));
        assert_eq!(items.items.lock().unwrap().len(), pieces.len());
    }
    // Windows' set_pieces rolls a refused leftover delete back the same way.
    let items = RefusedDeletes {
        items: Mutex::new(
            pieces
                .iter()
                .enumerate()
                .map(|(n, p)| (piece_key(UUID, n), p.clone()))
                .collect(),
        ),
        left: Mutex::new(0),
    };
    assert!(set_pieces(&items, UUID, "shorter").is_err());
    assert_eq!(get_pieces(&items, UUID).unwrap(), Some(old));
}

/// The keys `has_session` asked for, in order (each read can ask for the keychain on macOS).
struct Reading<'a>(&'a MemoryStore, Mutex<Vec<String>>);

impl SecretStore for Reading<'_> {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        self.1.lock().unwrap().push(key.into());
        self.0.get(key)
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        self.0.set(key, secret)
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.0.delete(key)
    }
    fn persistent(&self) -> bool {
        false
    }
    fn operations(&self) -> &Mutex<()> {
        self.0.operations()
    }
}

#[test]
fn has_session_asks_the_selected_account_first() {
    let store = MemoryStore::default();
    // The selected account is second in the list: its item alone is read.
    let mut accounts = Accounts::default();
    accounts
        .add(&store, other("Other", "other"), &secrets("x-token"))
        .unwrap();
    accounts.add(&store, notch(), &secrets("mc-token")).unwrap();
    let reading = Reading(&store, Mutex::default());
    assert!(accounts.has_session(&reading));
    assert_eq!(*reading.1.lock().unwrap(), vec![UUID]);
    // Its sign-in gone, another account's still counts, asked second.
    store.delete(UUID).unwrap();
    let reading = Reading(&store, Mutex::default());
    assert!(accounts.has_session(&reading));
    assert_eq!(*reading.1.lock().unwrap(), vec![UUID, "other"]);
    // Nobody has one: no.
    store.delete("other").unwrap();
    let reading = Reading(&store, Mutex::default());
    assert!(!accounts.has_session(&reading));
    assert_eq!(*reading.1.lock().unwrap(), vec![UUID, "other"]);
    assert!(!Accounts::default().has_session(&store));
}

#[test]
fn credential_manager_error_8_says_the_store_is_full() {
    let said = credential_manager_says(8).unwrap();
    assert!(said.contains("Credential Manager is full"));
    assert!(said.contains("Windows Credentials"));
    // Other codes keep the keyring crate's own words.
    assert_eq!(credential_manager_says(5), None);
    assert_eq!(credential_manager_says(1312), None);
}

#[test]
fn macos_removed_executable_error_requests_a_restart_and_new_sign_in() {
    let said = macos_keychain_says(100002).unwrap();
    assert_eq!(said, MACOS_LAUNCHER_RESTART);
    assert!(said.contains("Restart the launcher"));
    assert!(said.to_lowercase().contains("sign in"));
    for code in [0, 2, -25300, -25293, -25291, 100001, 100003] {
        assert_eq!(macos_keychain_says(code), None, "OSStatus {code}");
    }
}

/// Constructs native errors, without creating or reading any Keychain entry.
#[cfg(target_os = "macos")]
#[test]
fn only_the_typed_macos_removed_executable_failure_gets_the_restart_message() {
    let failure = |code| {
        keyring::Error::PlatformFailure(Box::new(security_framework::base::Error::from_code(code)))
    };
    assert_eq!(write_error(failure(100002)), MACOS_LAUNCHER_RESTART);
    for code in [-25300, -25293, -25291, 100003] {
        let error = failure(code);
        let original = error.to_string();
        assert_ne!(original, MACOS_LAUNCHER_RESTART);
        assert_eq!(write_error(error), original);
    }
    let unrelated = keyring::Error::PlatformFailure(Box::new(std::io::Error::other(
        "OSStatus 100002: unrelated provider",
    )));
    let original = unrelated.to_string();
    assert_eq!(write_error(unrelated), original, "not a string match");
    assert_eq!(
        write_error(keyring::Error::NoEntry),
        keyring::Error::NoEntry.to_string()
    );
}

#[test]
fn session_files_are_private_and_complete() {
    let dir = std::env::temp_dir().join(format!("horizon-launcher-session-{}", std::process::id()));
    let account = notch();
    let secrets = Secrets {
        refresh_token: "r".into(),
        sign_in_required: false,
        minecraft_token: "mc-token".into(),
        minecraft_expires_at: 1,
    };
    let path = write_session_file(&dir, &account, &secrets).unwrap();
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        v,
        json!({"name": "Notch", "uuid": UUID, "access_token": "mc-token", "user_type": "elyby", "xuid": null})
    );
    // The client token never leaves the keychain.
    assert!(!v.to_string().contains("\"r\""));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let other = write_session_file(&dir, &account, &secrets).unwrap();
    assert_ne!(path, other);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn play_as_picks_a_signed_in_account() {
    let mut a = Accounts {
        accounts: vec![Account {
            uuid: "abc".into(),
            name: "Ludo".into(),
            skin: None,
            cape: None,
        }],
        ..Accounts::default()
    };
    a.play_as("abc").unwrap();
    assert_eq!(a.selected().unwrap().name, "Ludo");
    assert!(a.play_as("zzz").is_err());
    assert_eq!(a.selected.as_deref(), Some("abc"));
    // An accounts.json written now has nothing but the accounts and who plays.
    assert_eq!(
        serde_json::to_string(&Accounts::default()).unwrap(),
        r#"{"accounts":[],"selected":null}"#
    );
}

#[test]
fn the_java_edition_plays_as_the_selected_account_even_offline() {
    let (m, ep) = mock(|_| {});
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token"));
    let player = java_player_with(&ep, &store, &mut accounts).unwrap();
    assert_eq!(
        (
            player.name.as_str(),
            player.uuid.as_str(),
            player.token.as_str()
        ),
        ("Notch", UUID, "mc-token")
    );
    assert!(m.lock().unwrap().log.is_empty(), "the token holds");
    // The selected account plays, not the first.
    accounts
        .add(&store, other("Alex", "other"), &secrets("alex-token"))
        .unwrap();
    let player = java_player_with(&offline(), &store, &mut accounts).unwrap();
    assert_eq!(
        (
            player.name.as_str(),
            player.uuid.as_str(),
            player.token.as_str()
        ),
        ("Alex", "other", "alex-token")
    );
    // A due token is renewed through Ely.by.
    let mut accounts = signed_in(&store, &expired("old-token"));
    let player = java_player_with(&ep, &store, &mut accounts).unwrap();
    assert_eq!(player.token, "ely-renewed");
    // No answer from Ely.by: the last token.
    let mut accounts = signed_in(&store, &expired("last-token"));
    let player = java_player_with(&offline(), &store, &mut accounts).unwrap();
    assert_eq!(player.token, "last-token");
    // Unless the sign-in was sent back.
    let sent_back = Secrets {
        sign_in_required: true,
        ..secrets("sent-back-token")
    };
    let mut accounts = signed_in(&store, &sent_back);
    assert_eq!(
        java_player_with(&offline(), &store, &mut accounts).unwrap_err(),
        elyby::EXPIRED
    );
    // Nobody signed in.
    assert_eq!(
        java_player_with(&offline(), &store, &mut Accounts::default()).unwrap_err(),
        NoAccount::default().to_string()
    );
}

/// A secrets file of the test's own, gone first.
fn secrets_file(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("horizon-secrets-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir.join("launcher").join(SECRETS_FILE)
}

fn remove_secrets_dir(path: &Path) {
    let _ = std::fs::remove_dir_all(path.parent().unwrap().parent().unwrap());
}

/// The files beside the secrets file, by name.
fn files_beside(path: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(path.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn the_secrets_file_keeps_sign_ins_across_starts() {
    sign_out_mock();
    let path = secrets_file("starts");
    let store = FileStore::new(path.clone());
    assert!(store.persistent());
    assert_eq!(store.get(UUID).unwrap(), None);
    let signed_in = secrets("mc-secret");
    let mut accounts = Accounts::default();
    accounts.add(&store, notch(), &signed_in).unwrap();
    // The next start reads the same file.
    let again = FileStore::new(path.clone());
    let read = secrets_in(&again, UUID).expect("kept across starts");
    assert_eq!(
        (read.minecraft_token, read.refresh_token),
        (signed_in.minecraft_token, signed_in.refresh_token)
    );
    // Never in accounts.json.
    assert!(!serde_json::to_string(&accounts)
        .unwrap()
        .contains("mc-secret"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "only the player reads it");
    }
    // No temporary file left behind.
    assert_eq!(
        files_beside(&path),
        [
            "account-secrets.json",
            "account-secrets.key",
            "account-secrets.lock"
        ]
    );
    // Signing the last account out removes the file.
    accounts.remove(&again, UUID).unwrap();
    assert_eq!(again.get(UUID).unwrap(), None);
    assert!(!path.exists());
    again.delete(UUID).unwrap();
    remove_secrets_dir(&path);
}

#[test]
fn a_broken_secrets_file_is_an_error_that_never_shows_it() {
    let path = secrets_file("broken");
    let store = FileStore::new(path.clone());
    // Cut short, and a lone string: serde's own messages quote the latter.
    for broken in [
        "{\"a\":\"leaked-secret",
        "\"leaked-secret\"",
        "[\"leaked-secret\"]",
    ] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let sealed = store.seal(broken.as_bytes()).unwrap();
        std::fs::write(&path, &sealed).unwrap();
        let said = store.get("a").unwrap_err();
        assert!(!said.contains("leaked-secret"), "{said}");
        assert!(said.contains("line 1"), "{said}");
        // Never written over: the secrets in it aren't lost in silence.
        assert!(store.set("c", "four").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), sealed);
    }
    remove_secrets_dir(&path);
}

#[test]
fn the_secrets_file_is_never_plain_text() {
    let path = secrets_file("sealed");
    let store = FileStore::new(path.clone());
    store.set("a", "kept-secret").unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.starts_with(super::SEALED));
    assert!(
        !bytes.windows(11).any(|w| w == b"kept-secret"),
        "the secret is encrypted"
    );
    assert_eq!(store.get("a").unwrap().as_deref(), Some("kept-secret"));
    // Plain JSON, or one byte changed in the sealed file: refused, never quoted, never written over.
    let mut changed = bytes.clone();
    *changed.last_mut().unwrap() ^= 1;
    for other in [b"{\"a\":\"leaked-secret\"}".to_vec(), changed] {
        std::fs::write(&path, &other).unwrap();
        let said = store.get("a").unwrap_err();
        assert!(!said.contains("leaked-secret"), "{said}");
        assert!(said.contains("Remove it and sign in again"), "{said}");
        assert!(store.set("c", "four").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), other);
    }
    remove_secrets_dir(&path);
}

#[test]
fn the_key_takes_the_install_secret_beside_the_file() {
    let path = secrets_file("install");
    let store = FileStore::new(path.clone());
    store.set("a", "kept-secret").unwrap();
    let secret = path.with_extension("key");
    assert_eq!(std::fs::read(&secret).unwrap().len(), 32);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&secret).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "only the player reads it");
    }
    // The same file in another install's folder, with a secret of its own: unreadable there.
    let other = secrets_file("install-other");
    let elsewhere = FileStore::new(other.clone());
    elsewhere.set("b", "other-secret").unwrap();
    std::fs::copy(&path, &other).unwrap();
    let said = elsewhere.get("a").unwrap_err();
    assert!(said.contains("Remove it and sign in again"), "{said}");
    // Without its secret, the file can't be read either: no key is left to work out.
    std::fs::remove_file(&secret).unwrap();
    assert!(store.get("a").is_err());
    assert!(!secret.exists(), "a read never makes a new secret");
    remove_secrets_dir(&path);
    remove_secrets_dir(&other);
}

#[test]
fn a_file_sealed_under_the_old_key_is_read_and_sealed_again() {
    use chacha20poly1305::{aead::Aead, KeyInit};
    let path = secrets_file("format1");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // As launchers before format 2 wrote it.
    let nonce = [7u8; 12];
    let sealed = chacha20poly1305::ChaCha20Poly1305::new(&super::file_key_v1())
        .encrypt(&nonce.into(), &b"{\"a\":\"old-secret\"}"[..])
        .unwrap();
    std::fs::write(&path, [b"HZSECv1\n".as_slice(), &nonce, &sealed].concat()).unwrap();
    let store = FileStore::new(path.clone());
    assert_eq!(store.get("a").unwrap().as_deref(), Some("old-secret"));
    // Sealed again at once, under the install's secret, and still read.
    let bytes = std::fs::read(&path).unwrap();
    assert!(bytes.starts_with(b"HZSECv2\n"));
    assert!(path.with_extension("key").exists());
    assert_eq!(
        FileStore::new(path.clone()).get("a").unwrap().as_deref(),
        Some("old-secret")
    );
    remove_secrets_dir(&path);
}

const WRITER: &str = "HORIZON_TEST_SECRETS_WRITER";

/// One of the processes [`processes_sharing_the_secrets_file_lose_nothing`] starts: does nothing
/// unless it gives the file and the writer's number.
#[test]
#[ignore = "a child process of processes_sharing_the_secrets_file_lose_nothing"]
fn secrets_file_writer() {
    let Some(spec) = std::env::var_os(WRITER) else {
        return;
    };
    let spec = spec.into_string().unwrap();
    let (w, path) = spec.split_once(':').unwrap();
    let store = FileStore::new(PathBuf::from(path));
    for i in 0..25 {
        store
            .set(&format!("{w}-{i}"), &format!("secret-{w}-{i}"))
            .unwrap();
    }
    if w == "0" {
        store.delete("signed-out").unwrap();
    }
}

#[test]
fn processes_sharing_the_secrets_file_lose_nothing() {
    let path = secrets_file("processes");
    FileStore::new(path.clone()).set("signed-out", "x").unwrap();
    let exe = std::env::current_exe().unwrap();
    let writers: Vec<_> = (0..6)
        .map(|w| {
            std::process::Command::new(&exe)
                .args([
                    "--ignored",
                    "--exact",
                    "account::tests::secrets_file_writer",
                    "--test-threads=1",
                ])
                .env(WRITER, format!("{w}:{}", path.display()))
                .stdout(std::process::Stdio::null())
                .spawn()
                .unwrap()
        })
        .collect();
    for mut writer in writers {
        assert!(writer.wait().unwrap().success());
    }
    let store = FileStore::new(path.clone());
    for w in 0..6 {
        for i in 0..25 {
            assert_eq!(
                store.get(&format!("{w}-{i}")).unwrap(),
                Some(format!("secret-{w}-{i}"))
            );
        }
    }
    assert_eq!(store.get("signed-out").unwrap(), None);
    assert_eq!(
        files_beside(&path),
        [
            "account-secrets.json",
            "account-secrets.key",
            "account-secrets.lock"
        ]
    );
    remove_secrets_dir(&path);
}

/// A keychain that can be away (a Secret Service that didn't start, a dismissed unlock prompt),
/// refuse some keys, or hold every call while `prompt` is held (an unlock prompt left open).
#[derive(Default)]
struct Keychain {
    kept: MemoryStore,
    away: std::sync::atomic::AtomicBool,
    refuses: Mutex<Vec<String>>,
    prompt: Mutex<()>,
    /// Calls that reached it: each one could have been a prompt.
    asked: std::sync::atomic::AtomicUsize,
    /// Run once, right after the next call it didn't answer.
    after_away: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// Run once, while it takes the next secret.
    while_taking: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Keychain {
    fn answer(&self) -> Result<(), String> {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop(self.prompt.lock().unwrap());
        if self.away.load(std::sync::atomic::Ordering::Relaxed) {
            if let Some(then) = self.after_away.lock().unwrap().take() {
                then();
            }
            return Err("The name org.freedesktop.secrets was not provided".into());
        }
        Ok(())
    }
    fn set_away(&self, away: bool) {
        self.away.store(away, std::sync::atomic::Ordering::Relaxed);
    }
    fn asked(&self) -> usize {
        self.asked.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// One keychain seen by several launchers.
struct Shared(Arc<Keychain>);

impl SecretStore for Shared {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        self.0.answer()?;
        self.0.kept.get(key)
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        self.0.answer()?;
        if self.0.refuses.lock().unwrap().iter().any(|k| k == key) {
            return Err("the keychain is full".into());
        }
        if let Some(meanwhile) = self.0.while_taking.lock().unwrap().take() {
            meanwhile();
        }
        self.0.kept.set(key, secret)
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.0.answer()?;
        self.0.kept.delete(key)
    }
    fn persistent(&self) -> bool {
        true
    }
}

/// A launcher starting with this keychain and secrets file.
fn launcher_with(keychain: &Arc<Keychain>, path: &Path) -> Layered {
    Layered::new(
        Box::new(Shared(keychain.clone())),
        FileStore::new(path.to_path_buf()),
    )
}

#[test]
fn the_keychain_takes_back_what_the_file_kept_while_it_was_away() {
    let path = secrets_file("recovery");
    let keychain = Arc::new(Keychain::default());
    // A is signed in while the keychain answers: no file.
    launcher_with(&keychain, &path)
        .set("A", "secret-a")
        .unwrap();
    assert!(!path.exists());
    // A start the keychain misses: B goes to the file, and A can't be read.
    keychain.set_away(true);
    let missed = launcher_with(&keychain, &path);
    missed.set("B", "secret-b").unwrap();
    assert!(missed.get("A").is_err());
    assert_eq!(missed.get("B").unwrap(), Some("secret-b".into()));
    // The keychain is back: the next start hands it B, and the file is gone.
    keychain.set_away(false);
    let back = launcher_with(&keychain, &path);
    assert_eq!(
        (
            keychain.kept.get("A").unwrap(),
            keychain.kept.get("B").unwrap()
        ),
        (Some("secret-a".into()), Some("secret-b".into()))
    );
    assert_eq!(back.get("B").unwrap(), Some("secret-b".into()));
    assert!(!path.exists());
    remove_secrets_dir(&path);
}

#[test]
fn a_launcher_on_the_file_keeps_working_after_another_hands_it_to_the_keychain() {
    let path = secrets_file("running");
    let keychain = Arc::new(Keychain::default());
    keychain.set_away(true);
    let running = launcher_with(&keychain, &path);
    running.set("A", "secret-a").unwrap();
    // The keychain comes back and a second launcher (or the CLI) starts.
    keychain.set_away(false);
    let second = launcher_with(&keychain, &path);
    assert!(!path.exists());
    // The first one still reads, renews and signs out.
    assert_eq!(running.get("A").unwrap(), Some("secret-a".into()));
    running.set("A", "renewed-a").unwrap();
    assert_eq!(second.get("A").unwrap(), Some("renewed-a".into()));
    running.delete("A").unwrap();
    assert_eq!(
        (second.get("A").unwrap(), keychain.kept.get("A").unwrap()),
        (None, None)
    );
    assert!(!path.exists());
    remove_secrets_dir(&path);
}

#[test]
fn launchers_started_on_an_old_file_find_it_after_another_hands_it_over() {
    let path = secrets_file("inherited");
    // A previous run kept A in the file.
    FileStore::new(path.clone()).set("A", "secret-a").unwrap();
    // Two launchers start while the keychain is away: their hand-overs fail. One reads A then.
    let keychain = Arc::new(Keychain::default());
    keychain.set_away(true);
    let read_before = launcher_with(&keychain, &path);
    let never_read = launcher_with(&keychain, &path);
    assert_eq!(read_before.get("A").unwrap(), Some("secret-a".into()));
    // The keychain is back and a third launcher hands A over.
    keychain.set_away(false);
    launcher_with(&keychain, &path);
    assert!(!path.exists());
    // Both running launchers still read A.
    assert_eq!(read_before.get("A").unwrap(), Some("secret-a".into()));
    assert_eq!(never_read.get("A").unwrap(), Some("secret-a".into()));
    remove_secrets_dir(&path);
}

#[test]
fn a_hand_over_by_another_process_right_after_a_start_missed_the_keychain() {
    let path = secrets_file("inherited-race");
    FileStore::new(path.clone()).set("A", "secret-a").unwrap();
    let keychain = Arc::new(Keychain::default());
    keychain.set_away(true);
    // The moment this start's hand-over fails, the keychain is back and another process hands A
    // over (the keychain takes it, the file lets it go), before this start has done anything else.
    *keychain.after_away.lock().unwrap() = Some(Box::new({
        let (keychain, path) = (keychain.clone(), path.clone());
        move || {
            keychain.set_away(false);
            keychain.kept.set("A", "secret-a").unwrap();
            FileStore::new(path).delete("A").unwrap();
        }
    }));
    let started = launcher_with(&keychain, &path);
    assert!(!path.exists());
    assert_eq!(started.get("A").unwrap(), Some("secret-a".into()));
    remove_secrets_dir(&path);
}

#[test]
fn a_secret_renewed_in_the_file_during_the_hand_over_is_found_after_the_next() {
    let path = secrets_file("renewed-in-file");
    let file = FileStore::new(path.clone());
    file.set("A", "old-a").unwrap();
    file.set("B", "secret-b").unwrap();
    let keychain = Arc::new(Keychain::default());
    // While this start hands A over, a launcher on the file renews A there; then the keychain
    // refuses B, and this run stops asking it.
    *keychain.while_taking.lock().unwrap() = Some(Box::new({
        let path = path.clone();
        move || FileStore::new(path).set("A", "renewed-a").unwrap()
    }));
    keychain.refuses.lock().unwrap().push("B".into());
    let started = launcher_with(&keychain, &path);
    assert_eq!(keychain.kept.get("A").unwrap(), Some("old-a".into()));
    // Another start hands both over: this run, which hasn't read A since, still reads both, the
    // renewed A.
    keychain.refuses.lock().unwrap().clear();
    launcher_with(&keychain, &path);
    assert!(!path.exists());
    assert_eq!(
        (started.get("A").unwrap(), started.get("B").unwrap()),
        (Some("renewed-a".into()), Some("secret-b".into()))
    );
    remove_secrets_dir(&path);
}

#[test]
fn a_move_the_keychain_half_refuses_then_a_sign_out_leaves_no_copy() {
    let path = secrets_file("half");
    let keychain = Arc::new(Keychain::default());
    keychain.set_away(true);
    let away = launcher_with(&keychain, &path);
    away.set("A", "secret-a").unwrap();
    away.set("B", "secret-b").unwrap();
    // Back, but it refuses B: A moves, B stays in the file.
    keychain.set_away(false);
    keychain.refuses.lock().unwrap().push("B".into());
    let back = launcher_with(&keychain, &path);
    assert_eq!(
        (
            keychain.kept.get("A").unwrap(),
            keychain.kept.get("B").unwrap()
        ),
        (Some("secret-a".into()), None)
    );
    assert_eq!(back.get("B").unwrap(), Some("secret-b".into()));
    // A signs out on that run, which stopped asking the keychain: no launcher reads A again...
    back.delete("A").unwrap();
    assert_eq!(back.get("A").unwrap(), None);
    // (A start where the keychain still refuses B then reads A as "away": no secret either.)
    assert_eq!(
        launcher_with(&keychain, &path).get("A").ok().flatten(),
        None
    );
    // ...and the keychain's copy goes at the first start it takes everything.
    keychain.refuses.lock().unwrap().clear();
    let later = launcher_with(&keychain, &path);
    assert_eq!(
        (keychain.kept.get("A").unwrap(), later.get("B").unwrap()),
        (None, Some("secret-b".into()))
    );
    assert!(!path.exists());
    remove_secrets_dir(&path);
}

#[test]
fn a_keychain_waiting_on_a_prompt_never_holds_the_file_up() {
    let path = secrets_file("prompt");
    let keychain = Arc::new(Keychain::default());
    keychain.set_away(true);
    launcher_with(&keychain, &path)
        .set("B", "secret-b")
        .unwrap();
    keychain.set_away(false);
    // An unlock prompt nobody answers yet, while one launcher signs A in.
    let prompt = keychain.prompt.lock().unwrap();
    let asked = keychain.asked();
    let signing_in = {
        let (keychain, path) = (keychain.clone(), path.clone());
        std::thread::spawn(move || {
            Layered {
                keychain: Box::new(Shared(keychain)),
                file: FileStore::new(path),
                away: Default::default(),
                parked: Default::default(),
            }
            .set("A", "secret-a")
        })
    };
    while keychain.asked() == asked {
        std::thread::sleep(Duration::from_millis(1));
    }
    // Another process still reads and writes the file meanwhile.
    let (done, said) = std::sync::mpsc::channel();
    let other = path.clone();
    std::thread::spawn(move || {
        let file = FileStore::new(other);
        let _ = done.send((file.get("B"), file.set("C", "secret-c")));
    });
    let (read, written) = said
        .recv_timeout(Duration::from_secs(10))
        .expect("the file waited on the keychain's prompt");
    assert_eq!((read.unwrap(), written), (Some("secret-b".into()), Ok(())));
    // The prompt is answered: A goes to the keychain, B and C are still in the file.
    drop(prompt);
    signing_in.join().unwrap().unwrap();
    let file = FileStore::new(path.clone());
    assert_eq!(keychain.kept.get("A").unwrap(), Some("secret-a".into()));
    assert_eq!(
        (
            file.get("A").unwrap(),
            file.get("B").unwrap(),
            file.get("C").unwrap()
        ),
        (None, Some("secret-b".into()), Some("secret-c".into()))
    );
    remove_secrets_dir(&path);
}

/// A start handing the file's A to the keychain, held up mid-way by an unlock prompt, while a
/// launcher already running does `meanwhile` to A. The running one waits for the hand-over instead
/// of racing it, and goes on once the prompt is answered.
fn hand_over_racing(
    name: &str,
    meanwhile: impl FnOnce(&Layered) + Send + 'static,
) -> (Arc<Keychain>, PathBuf) {
    let path = secrets_file(name);
    let keychain = Arc::new(Keychain::default());
    // Started before the file had anything: it asked the keychain nothing.
    let running = launcher_with(&keychain, &path);
    // A run the keychain missed kept A in the file.
    FileStore::new(path.clone()).set("A", "old-a").unwrap();
    let prompt = keychain.prompt.lock().unwrap();
    let asked = keychain.asked();
    let starting = {
        let (keychain, path) = (keychain.clone(), path.clone());
        std::thread::spawn(move || drop(launcher_with(&keychain, &path)))
    };
    while keychain.asked() == asked {
        std::thread::sleep(Duration::from_millis(1));
    }
    let racing = std::thread::spawn(move || meanwhile(&running));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        keychain.asked(),
        asked + 1,
        "the running launcher reached the keychain during the hand-over"
    );
    drop(prompt);
    starting.join().unwrap();
    racing.join().unwrap();
    (keychain, path)
}

#[test]
fn a_hand_over_never_brings_back_an_account_signed_out_meanwhile() {
    let (keychain, path) = hand_over_racing("race-sign-out", |running| {
        running.delete("A").unwrap();
    });
    assert_eq!(keychain.kept.get("A").unwrap(), None);
    assert_eq!(launcher_with(&keychain, &path).get("A").unwrap(), None);
    assert!(!path.exists());
    remove_secrets_dir(&path);
}

#[test]
fn a_hand_over_never_writes_over_a_token_renewed_meanwhile() {
    let (keychain, path) = hand_over_racing("race-renewal", |running| {
        running.set("A", "renewed-a").unwrap();
    });
    assert_eq!(keychain.kept.get("A").unwrap(), Some("renewed-a".into()));
    assert_eq!(
        launcher_with(&keychain, &path).get("A").unwrap(),
        Some("renewed-a".into())
    );
    assert!(!path.exists());
    remove_secrets_dir(&path);
}

#[test]
fn the_secrets_file_counts_only_sign_ins() {
    let path = secrets_file("counts");
    let file = FileStore::new(path.clone());
    assert!(!file.holds_secrets());
    file.put("A", SIGNED_OUT).unwrap();
    assert!(!file.holds_secrets(), "a sign-out mark alone");
    file.put("B", "secret-b").unwrap();
    assert!(file.holds_secrets());
    std::fs::write(&path, "{\"B\":\"secret-b").unwrap();
    assert!(!file.holds_secrets(), "a file that can't be read");
    remove_secrets_dir(&path);
}

#[test]
fn a_dismissed_prompt_is_asked_once_a_run() {
    let path = secrets_file("dismissed");
    FileStore::new(path.clone()).set("A", "secret-a").unwrap();
    let keychain = Arc::new(Keychain::default());
    keychain.set_away(true);
    // The start asks once to hand A over; nothing after asks again.
    let run = launcher_with(&keychain, &path);
    assert_eq!(keychain.asked(), 1);
    run.set("B", "secret-b").unwrap();
    run.set("A", "renewed-a").unwrap();
    assert_eq!(run.get("A").unwrap(), Some("renewed-a".into()));
    assert!(run.get("never-signed-in").is_err());
    run.delete("B").unwrap();
    assert_eq!(run.get("B").unwrap(), None);
    assert_eq!(keychain.asked(), 1);
    remove_secrets_dir(&path);
}

/// A keyring that never answers until the test lets it go: a stuck Secret Service daemon
/// (#12179).
struct Silent {
    hung: Arc<Mutex<()>>,
    asked: Arc<std::sync::atomic::AtomicUsize>,
}

impl Silent {
    fn answer(&self) {
        self.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        drop(self.hung.lock().unwrap());
    }
}

impl SecretStore for Silent {
    fn get(&self, _: &str) -> Result<Option<String>, String> {
        self.answer();
        Ok(None)
    }
    fn set(&self, _: &str, _: &str) -> Result<(), String> {
        self.answer();
        Ok(())
    }
    fn delete(&self, _: &str) -> Result<(), String> {
        self.answer();
        Ok(())
    }
    fn persistent(&self) -> bool {
        true
    }
}

/// How long the tests' keyrings get: short, the behaviour is the same as with [`KEYCHAIN_WAIT`].
const SILENT_WAIT: Duration = Duration::from_millis(300);

#[test]
fn a_keyring_that_never_answers_ends_play_with_what_is_wrong() {
    let path = secrets_file("silent");
    let hung = Arc::new(Mutex::new(()));
    let held = hung.lock().unwrap();
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let keyring = Silent {
        hung: hung.clone(),
        asked: asked.clone(),
    };
    let store = Arc::new(Layered::new(
        Box::new(Bounded::new(keyring, SILENT_WAIT)),
        FileStore::new(path.clone()),
    ));
    let mut accounts = signed_in(&MemoryStore::default(), &secrets("mc-token"));
    let started = Instant::now();
    // The window's sign-in check at start-up asks first, and holds the account lock meanwhile.
    let checking = {
        let store = store.clone();
        std::thread::spawn(move || session::status_with_progress(store.as_ref(), UUID, &|_| {}))
    };
    while asked.load(std::sync::atomic::Ordering::SeqCst) == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    // Then Play, behind it.
    let (done, played) = std::sync::mpsc::channel();
    {
        let store = store.clone();
        std::thread::spawn(move || {
            let _ = done.send(launch(&offline(), store.as_ref(), &mut accounts));
        });
    }
    let refused = played
        .recv_timeout(Duration::from_secs(10))
        .expect("Play waited on the keyring forever")
        .unwrap_err();
    assert!(started.elapsed() < Duration::from_secs(5));
    let status = checking.join().unwrap();
    assert_eq!(status.state, session::State::Retry);
    // Not "Sign in with an Ely.by account": the player is signed in.
    let said = format!("Can't read Notch's sign-in: {KEYCHAIN_AWAY}");
    assert_eq!(refused.keyring.as_deref(), Some(said.as_str()));
    assert_eq!(
        (refused.to_string(), refused.in_window()),
        (said.clone(), said)
    );
    // The next start asks the keyring again, and says it didn't answer.
    let restarted = Layered::new(
        Box::new(Bounded::new(
            Silent {
                hung: hung.clone(),
                asked: asked.clone(),
            },
            SILENT_WAIT,
        )),
        FileStore::new(path.clone()),
    );
    let mut accounts = signed_in(&MemoryStore::default(), &secrets("mc-token"));
    let refused = launch(&offline(), &restarted, &mut accounts).unwrap_err();
    assert_eq!(
        refused.keyring,
        Some(format!(
            "Can't read Notch's sign-in: {}",
            keyring_no_answer(SILENT_WAIT)
        ))
    );
    drop(held);
    remove_secrets_dir(&path);
}

#[test]
fn a_keyring_given_up_on_is_not_asked_again_until_it_answers() {
    let hung = Arc::new(Mutex::new(()));
    let held = hung.lock().unwrap();
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let keyring = Bounded::new(
        Silent {
            hung: hung.clone(),
            asked: asked.clone(),
        },
        SILENT_WAIT,
    );
    let started = Instant::now();
    assert_eq!(keyring.get("A"), Err(keyring_no_answer(SILENT_WAIT)));
    assert!(started.elapsed() >= SILENT_WAIT);
    // Still stuck: the next calls fail at once, without a second call piling up behind it.
    let again = Instant::now();
    assert_eq!(
        keyring.set("A", "secret-a"),
        Err(keyring_no_answer(SILENT_WAIT))
    );
    assert!(again.elapsed() < SILENT_WAIT);
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    // It answers at last: it is asked again, and its answers come back.
    drop(held);
    let deadline = Instant::now() + Duration::from_secs(10);
    while keyring.delete("A").is_err() {
        assert!(Instant::now() < deadline, "the late answer never counted");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(keyring.get("A"), Ok(None));
}

#[test]
fn signing_out_while_the_keychain_is_away_forgets_its_copy() {
    let path = secrets_file("sign-out");
    let keychain = Arc::new(Keychain::default());
    launcher_with(&keychain, &path)
        .set("A", "secret-a")
        .unwrap();
    keychain.set_away(true);
    let away = launcher_with(&keychain, &path);
    away.delete("A").unwrap();
    assert_eq!(away.get("A").unwrap(), None);
    // The keychain's copy goes at the first start it answers, and the file with it.
    keychain.set_away(false);
    let back = launcher_with(&keychain, &path);
    assert_eq!(
        (back.get("A").unwrap(), keychain.kept.get("A").unwrap()),
        (None, None)
    );
    assert!(!path.exists());
    remove_secrets_dir(&path);
}

#[test]
fn manual_refresh_renews_a_fresh_token_without_selecting_another_account() {
    let store = MemoryStore::default();
    let accounts = signed_in(&store, &secrets("original-minecraft"));
    let (m, ep) = mock(|_| {});
    let status = session::refresh_with(&ep, &store, &notch(), &nowhere());
    assert_eq!(status.state, session::State::Ready);
    let saved = kept(&store);
    assert_eq!(saved.refresh_token, CLIENT);
    assert_eq!(saved.minecraft_token, "ely-renewed");
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
    // Renewed although the token had two hours left: once, with the token and the client token.
    let log = m.lock().unwrap().log.clone();
    let refreshes: Vec<Value> = log
        .iter()
        .filter(|(_, path, _)| path == "/ely/auth/refresh")
        .map(|(_, _, body)| serde_json::from_str(body).unwrap())
        .collect();
    assert_eq!(refreshes.len(), 1);
    assert_eq!(
        (
            refreshes[0]["accessToken"].as_str(),
            refreshes[0]["clientToken"].as_str()
        ),
        (Some("original-minecraft"), Some(CLIENT))
    );
    let public = serde_json::to_string(&status).unwrap();
    for secret in ["ely-renewed", "original-minecraft", CLIENT] {
        assert!(!public.contains(secret));
    }
}

#[test]
fn a_refresh_saves_a_renewed_profile_and_leaves_an_unchanged_one() {
    let store = MemoryStore::default();
    let accounts = signed_in(&store, &secrets("original-minecraft"));
    let (m, ep) = mock(|_| {});
    let file = accounts_file("refresh");
    accounts.save_to(&file).unwrap();
    // The skin system has a skin and a cape now; the saved account has neither.
    let status = session::refresh_with(&ep, &store, &notch(), &file);
    assert_eq!(status.state, session::State::Ready);
    let saved = Accounts::load_from(&file);
    let account = saved.find(UUID).unwrap();
    assert_eq!(
        (account.skin.as_ref(), account.cape.as_deref()),
        (
            Some(&(
                "http://skins.test/Notch.png".to_string(),
                "slim".to_string()
            )),
            Some("http://skins.test/Notch-cape.png")
        )
    );
    assert_eq!(saved.selected.as_deref(), Some(UUID));
    // Already Ely.by's profile: the file is not written again.
    saved.save_to(&file).unwrap();
    let text = std::fs::read_to_string(&file).unwrap();
    let renewed = saved.find(UUID).unwrap().clone();
    session::refresh_with(&ep, &store, &renewed, &file);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), text);
    // A new name is: the row and its head follow the refresh, not the next Play.
    m.lock().unwrap().profile = Some((UUID.into(), "Notch_2".into()));
    session::refresh_with(&ep, &store, &renewed, &file);
    let saved = Accounts::load_from(&file);
    assert_eq!(saved.find(UUID).unwrap().name, "Notch_2");
    assert_eq!(
        saved
            .find(UUID)
            .unwrap()
            .skin
            .as_ref()
            .map(|s| s.0.as_str()),
        Some("http://skins.test/Notch_2.png")
    );
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[test]
fn a_skin_system_that_is_down_never_fails_a_renewal() {
    let store = MemoryStore::default();
    let accounts = signed_in(&store, &secrets("old-minecraft"));
    let (_, ep) = mock(|m| m.skins_down = true);
    let file = accounts_file("refresh-skins-down");
    accounts.save_to(&file).unwrap();
    let status = session::refresh_with(&ep, &store, &notch(), &file);
    assert_eq!(status.state, session::State::Ready);
    assert_eq!(kept(&store).minecraft_token, "ely-renewed");
    // The account keeps the skin it has: none was ever saved here, so it stays as it was.
    assert_eq!(Accounts::load_from(&file), accounts);
    let _ = std::fs::remove_dir_all(file.parent().unwrap());
}

#[test]
fn a_running_game_gets_a_renewed_session_only_when_ready() {
    let store = MemoryStore::default();
    signed_in(&store, &secrets("old-minecraft"));
    let (_, ep) = mock(|_| {});
    let dir = std::env::temp_dir().join(format!("horizon-launcher-renew-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let file = session::renew_for_game_with(&ep, &store, &notch(), &nowhere(), &dir).unwrap();
    let handed: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(handed["uuid"], UUID);
    assert_eq!(handed["access_token"], "ely-renewed");
    assert_eq!(handed["user_type"], "elyby");
    // A refused refresh hands over nothing and says to sign in again.
    signed_in(&store, &secrets("revoked"));
    let refused =
        session::renew_for_game_with(&ep, &store, &notch(), &nowhere(), &dir).unwrap_err();
    assert_eq!(refused.state, session::State::SignInRequired);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn revoked_refresh_is_remembered_until_a_successful_sign_in() {
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("revoked"));
    let (m, ep) = mock(|_| {});
    let status = session::refresh_with(&ep, &store, &notch(), &nowhere());
    assert_eq!(status.state, session::State::SignInRequired);
    let requests = m.lock().unwrap().log.len();
    assert_eq!(requests, 1);
    // Remembered: neither another refresh nor a launch asks Ely.by again.
    let _ = session::refresh_with(&ep, &store, &notch(), &nowhere());
    let pass = launch(&ep, &store, &mut accounts).unwrap();
    assert!(pass.session.is_none());
    assert_eq!(pass.offline.as_deref(), Some(elyby::EXPIRED));
    assert_eq!(m.lock().unwrap().log.len(), requests);
    assert_eq!(
        session::status(&store, UUID).state,
        session::State::SignInRequired
    );
    accounts
        .add(&store, notch(), &secrets("new-minecraft"))
        .unwrap();
    assert_eq!(session::status(&store, UUID).state, session::State::Ready);
}

#[test]
fn a_connection_failure_is_retryable_and_keeps_the_saved_sign_in() {
    let store = MemoryStore::default();
    let _accounts = signed_in(&store, &secrets("still-usable"));
    let before = store.get(UUID).unwrap();
    let status = session::refresh_with(&offline(), &store, &notch(), &nowhere());
    assert_eq!(status.state, session::State::Retry);
    assert_eq!(store.get(UUID).unwrap(), before);
    assert!(!kept(&store).sign_in_required);
}

struct Fixed(Vec<SocketAddr>);

impl ureq::Resolver for Fixed {
    fn resolve(&self, _: &str) -> std::io::Result<Vec<SocketAddr>> {
        Ok(self.0.clone())
    }
}

#[test]
fn addresses_alternate_families_starting_with_the_first() {
    let v6 = |n: u16| SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], n));
    let v4 = |n: u16| SocketAddr::from(([127, 0, 0, 1], n));
    assert_eq!(
        interleave(vec![v6(1), v6(2), v6(3), v4(4), v4(5)]),
        vec![v6(1), v4(4), v6(2), v4(5), v6(3)]
    );
    assert_eq!(interleave(vec![v4(1), v6(2)]), vec![v4(1), v6(2)]);
    assert_eq!(
        interleave(vec![v4(1), v4(2), v4(3), v6(4), v6(5)]),
        vec![v4(1), v6(4), v4(2), v6(5), v4(3)]
    );
    assert_eq!(interleave(vec![v4(1), v4(2)]), vec![v4(1), v4(2)]);
    assert_eq!(interleave(vec![v6(1), v6(2)]), vec![v6(1), v6(2)]);
    assert!(interleave(Vec::new()).is_empty());
}

#[test]
fn a_dead_ipv6_address_falls_back_to_ipv4() {
    // Loopback only: a closed IPv6 port stands for the broken route, a local listener for the host's
    // IPv4 address. Both IPv6 answers come first, as a resolver lists them.
    let dead = |p: u16| SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 1], p));
    let closed = TcpListener::bind("[::1]:0")
        .map(|l| l.local_addr().unwrap().port())
        .unwrap_or(9);
    let live = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = live.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut conn, _) = live.accept().unwrap();
        let mut line = String::new();
        BufReader::new(conn.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        conn.write_all(
            b"HTTP/1.1 200 OK
Content-Length: 2
Connection: close

ok",
        )
        .unwrap();
    });
    let addrs = vec![
        dead(closed),
        dead(closed),
        SocketAddr::from(([127, 0, 0, 1], port)),
    ];
    let agent = builder().resolver(Interleaved(Fixed(addrs))).build();
    let body = agent
        .get("http://horizon.test/")
        .call()
        .unwrap()
        .into_string()
        .unwrap();
    assert_eq!(body, "ok");
    server.join().unwrap();
}

#[test]
fn expiry_is_refreshable_and_a_missing_credential_needs_sign_in() {
    let store = MemoryStore::default();
    assert_eq!(
        session::status(&store, UUID).state,
        session::State::SignInRequired
    );
    let mut accounts = signed_in(
        &store,
        &Secrets {
            minecraft_expires_at: 0,
            ..secrets("expired")
        },
    );
    assert_eq!(
        session::status(&store, UUID).state,
        session::State::NeedsRefresh
    );
    let (_, ep) = mock(|_| {});
    assert!(launch(&ep, &store, &mut accounts)
        .unwrap()
        .session
        .is_some());
    assert_eq!(session::status(&store, UUID).state, session::State::Ready);
}

#[test]
fn refresh_serializes_with_sign_out_and_cannot_restore_a_deleted_credential() {
    sign_out_mock();
    let store = Arc::new(MemoryStore::default());
    let mut accounts = signed_in(&store, &secrets("old-minecraft"));
    let (_, ep) = mock(|_| {});
    let refresh_store = store.clone();
    let refresh = std::thread::spawn(move || {
        session::refresh_with(&ep, &*refresh_store, &notch(), &nowhere())
    });
    accounts.remove(&*store, UUID).unwrap();
    let _ = refresh.join().unwrap();
    assert!(store.get(UUID).unwrap().is_none());
}

#[test]
fn a_damaged_saved_sign_in_is_not_overwritten_or_exposed() {
    let store = MemoryStore::default();
    store.set(UUID, "damaged-secret-contents").unwrap();
    let status = session::refresh_with(&offline(), &store, &notch(), &nowhere());
    assert_eq!(status.state, session::State::SignInRequired);
    assert_eq!(
        store.get(UUID).unwrap().as_deref(),
        Some("damaged-secret-contents")
    );
    assert!(!serde_json::to_string(&status)
        .unwrap()
        .contains("damaged-secret-contents"));
}

#[test]
fn an_unreadable_keychain_is_retryable_without_any_write() {
    struct Locked;
    impl SecretStore for Locked {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            Err("locked-keychain-details".into())
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            panic!("must not overwrite an unreadable credential")
        }
        fn delete(&self, _: &str) -> Result<(), String> {
            panic!("must not delete an unreadable credential")
        }
        fn persistent(&self) -> bool {
            false
        }
    }
    let status = session::refresh_with(&offline(), &Locked, &notch(), &nowhere());
    assert_eq!(status.state, session::State::Retry);
    assert!(!status.message.contains("locked-keychain-details"));
}

#[test]
fn refresh_reports_the_steps_in_order() {
    use session::Phase::*;
    let store = MemoryStore::default();
    let _accounts = signed_in(&store, &secrets("old"));
    let (_, ep) = mock(|_| {});
    let phases = std::cell::RefCell::new(Vec::new());
    let report = |phase| phases.borrow_mut().push(phase);
    let status = session::refresh_account(&ep, &store, &notch(), &nowhere(), &report);
    assert_eq!(status.state, session::State::Ready);
    assert_eq!(*phases.borrow(), [Queued, Keychain, Elyby, Saving]);
    // Nothing changed, nothing saved.
    phases.borrow_mut().clear();
    let status = session::refresh_account(&offline(), &store, &notch(), &nowhere(), &report);
    assert_eq!(status.state, session::State::Retry);
    assert_eq!(*phases.borrow(), [Queued, Keychain, Elyby]);
    // A status read asks no service.
    phases.borrow_mut().clear();
    session::status_with_progress(&store, UUID, &report);
    assert_eq!(*phases.borrow(), [Queued, Keychain]);
}

#[test]
fn a_status_read_gives_up_behind_a_long_operation() {
    let store = MemoryStore::default();
    let held = session::lock(&store).unwrap();
    let start = std::time::Instant::now();
    let busy = session::lock_within(&store, Some(std::time::Duration::from_millis(150)));
    assert!(busy.is_err(), "a held lock must not be taken twice");
    assert!(start.elapsed() < std::time::Duration::from_secs(2));
    // Another memory store (another test running beside this one) has its own lock.
    let other = MemoryStore::default();
    assert!(session::lock_within(&other, Some(std::time::Duration::ZERO)).is_ok());
    drop(held);
    assert!(session::lock_within(&store, Some(std::time::Duration::from_secs(5))).is_ok());
}

#[test]
fn a_launch_as_another_account_leaves_the_selected_one() {
    let store = MemoryStore::default();
    // Notch is signed in and selected, but his token has run out; Alex is listed but no longer
    // has tokens in the keychain.
    let mut accounts = signed_in(&store, &expired("mc-token"));
    let alex = "0f22b7ed5e3d4a2c9bfe2f6f8f1f6a01";
    accounts.accounts.push(other("Alex", alex));
    // This launch plays as Alex: it is Alex who is told to sign in again (Notch being signed in
    // doesn't let Alex play), and Notch is still the one `account select` picked.
    let refused = launch_as(&offline(), &store, &mut accounts, alex);
    assert!(refused
        .unwrap_err()
        .starts_with("Alex is no longer signed in"));
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
    // Notch plays offline, with his last tokens, while Ely.by can't be reached.
    let pass = launch_as(&offline(), &store, &mut accounts, UUID).unwrap();
    assert!(pass.session.is_none() && pass.offline.is_some());
    // Alex signed in: this launch is his, and Notch is still the selected one.
    store
        .set(
            alex,
            &serde_json::to_string(&secrets("alex-token")).unwrap(),
        )
        .unwrap();
    let pass = launch_as(&offline(), &store, &mut accounts, alex).unwrap();
    let (played, held) = pass.session.unwrap();
    assert_eq!(
        (played.name.as_str(), held.minecraft_token.as_str()),
        ("Alex", "alex-token")
    );
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
    // An account that isn't in the launcher is refused, and changes nothing either.
    let gone = launch_as(&offline(), &store, &mut accounts, "not-signed-in");
    assert_eq!(gone.unwrap_err(), "That account isn't signed in.");
    assert_eq!(accounts.selected.as_deref(), Some(UUID));
}

#[test]
fn splitscreen_stops_renewing_other_accounts_once_out_of_time() {
    // A sign-in service that never answers: any request would hang the test.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/x", silent.local_addr().unwrap());
    let ep = Endpoints {
        ely: url.clone(),
        skins: url,
    };
    let store = MemoryStore::default();
    let mut accounts = signed_in(&store, &secrets("mc-token"));
    let expired = Secrets {
        minecraft_expires_at: 0,
        ..secrets("old")
    };
    let cal = Account {
        uuid: "c".into(),
        name: "Cal".into(),
        skin: None,
        cape: None,
    };
    accounts.add(&store, cal, &expired).unwrap();
    accounts.selected = Some(UUID.into());
    let dir = std::env::temp_dir().join(format!("hz-split-late-{}", std::process::id()));
    let start = std::time::Instant::now();
    let env = account_sessions_until(&ep, &store, &dir, &mut accounts, std::time::Instant::now());
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "no request started"
    );
    let list: Vec<Value> = serde_json::from_str(&env[0].1).unwrap();
    assert!(list[0]["skipped"]
        .as_str()
        .unwrap()
        .contains("Sign in again"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// A service's account check: the launcher renews the account, then tells Ely.by's session server
/// it joins the id the service gave.
#[test]
fn joining_a_service_id_tells_elybys_session_server() {
    fn joins(m: &Mutex<Mock>) -> usize {
        let asked = paths(m);
        asked
            .iter()
            .filter(|p| p.as_str() == "/ely/session/join")
            .count()
    }
    let (m, ep) = mock(|_| {});
    let store = MemoryStore::default();
    signed_in(&store, &secrets("mc-token"));
    session::join_server_with(&ep, &store, &notch(), &nowhere(), "svc-42").unwrap();
    assert_eq!(joins(&m), 1);
    // Ely.by refuses: the error says to sign in again.
    m.lock().unwrap().join_refused = true;
    let refused = session::join_server_with(&ep, &store, &notch(), &nowhere(), "svc-42");
    assert!(refused.unwrap_err().contains("Sign in again"));
    assert_eq!(joins(&m), 2);
    // An account Ely.by sent back to sign-in never joins.
    signed_in(&store, &secrets("revoked"));
    let refused = session::join_server_with(&ep, &store, &notch(), &nowhere(), "svc-42");
    assert_eq!(refused.unwrap_err(), elyby::EXPIRED);
    assert_eq!(joins(&m), 2);
}

/// A launch loads the accounts, asks the services for a while, then saves: a sign-in or sign-out
/// made meanwhile stays, and only the names and skins it renewed are written.
#[test]
fn a_launch_saving_late_keeps_sign_ins_and_outs_made_meanwhile() {
    let store = MemoryStore::default();
    let dir = std::env::temp_dir().join(format!("horizon-launcher-late-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let file = dir.join("accounts.json");
    let other = |uuid: &str, name: &str| Account {
        uuid: uuid.into(),
        name: name.into(),
        skin: None,
        cape: None,
    };
    let mut before = signed_in(&store, &secrets("mc-token"));
    before.accounts.push(other("gone", "Leaving"));
    before.save_to(&file).unwrap();
    // The launch's copy, with Notch's new skin.
    let mut renewed = Accounts::load_from(&file);
    renewed.accounts[0].skin = Some(("https://textures.example/notch".into(), "classic".into()));
    // Meanwhile: one account signs out, another signs in.
    let mut now = Accounts::load_from(&file);
    now.accounts.retain(|a| a.uuid != "gone");
    now.accounts.push(other("new", "Arriving"));
    now.save_to(&file).unwrap();
    renewed.save_renewed_to(&store, &file).unwrap();
    let saved = Accounts::load_from(&file);
    let uuids: Vec<&str> = saved.accounts.iter().map(|a| a.uuid.as_str()).collect();
    assert_eq!(uuids, [UUID, "new"]);
    assert!(
        saved.find(UUID).unwrap().skin.is_some(),
        "the renewed skin is kept"
    );
    assert_eq!(saved.selected.as_deref(), Some(UUID));
    // Written whole through a new file: nothing else is left beside it.
    let names: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, [std::ffi::OsString::from("accounts.json")]);
    let _ = std::fs::remove_dir_all(&dir);
}
