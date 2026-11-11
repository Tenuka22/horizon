//! Finding a player's account by name, as an online-mode server does for the
//! names given to `op`, `whitelist` and `ban` (CachedUserNameToIdResolver,
//! MinecraftServicesProfileRepository.findProfileByName): the services'
//! `profiles` / `getByName` endpoint, found through the discovery document.
//! A server with `auth-provider=elyby` asks Ely.by instead (`HORIZON_ELY_AUTH`).
use crate::{
    json::{self, Json},
    properties::AuthProvider,
    user_lists::NameAndId,
};
use horizon_protocol::rsa::RsaPublicKey;
use std::time::Duration;

/// Mojang's services discovery document.
pub const DEFAULT_DISCOVERY: &str = "https://discovery.minecraftservices.com/minecraft/client";

/// The discovery document's URL: `HORIZON_API_DISCOVERY` overrides it like
/// authlib's `-Dminecraft.api.discovery.host`.
pub fn discovery_url() -> String {
    std::env::var("HORIZON_API_DISCOVERY")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| DEFAULT_DISCOVERY.to_owned())
}

/// StringUtil.isValidPlayerName: at most 16 UTF-16 units, none a control
/// character, a space or above ASCII.
pub fn valid_player_name(name: &str) -> bool {
    name.encode_utf16().count() <= 16 && name.encode_utf16().all(|c| c > 32 && c < 127)
}

/// How a server resolves a name it was given (CachedUserNameToIdResolver.get,
/// without the usercache.json cache): an offline server (`None`) makes the
/// offline profile; an online one, whose accounts are `provider`'s, asks that
/// provider (for the name in lower case) and knows nobody it doesn't.
pub fn resolve(name: &str, online: Option<AuthProvider>) -> Option<NameAndId> {
    match online {
        Some(provider) if valid_player_name(name) => {
            find_by_name_of(provider, &name.to_ascii_lowercase())
        }
        Some(_) => None,
        None => Some(NameAndId::offline(name)),
    }
}

/// The account named `name` at `provider`'s service: Mojang's through the discovery document, Ely.by's through
/// `HORIZON_ELY_AUTH`.
pub fn find_by_name_of(provider: AuthProvider, name: &str) -> Option<NameAndId> {
    match provider {
        AuthProvider::Mojang => find_by_name(&discovery_url(), name),
        AuthProvider::Elyby => find_by_name_ely(&ely_base()?, name),
    }
}

/// The account with this id at `provider`'s service.
pub fn find_by_id_of(provider: AuthProvider, id: u128) -> Option<NameAndId> {
    match provider {
        AuthProvider::Mojang => find_by_id(&discovery_url(), id),
        AuthProvider::Elyby => find_by_id_ely(&ely_base()?, id),
    }
}

/// Ely.by's account service, from `HORIZON_ELY_AUTH` (the launcher sets it when it starts a server). None:
/// Ely.by names can't be looked up.
pub fn ely_base() -> Option<String> {
    std::env::var("HORIZON_ELY_AUTH")
        .ok()
        .map(|base| base.trim_end_matches('/').to_owned())
        .filter(|base| !base.is_empty())
}

/// The Ely.by account named `name` (any case), by `GET {base}/api/users/profiles/minecraft/{name}`; None when
/// there is none (204) or Ely.by doesn't answer.
pub fn find_by_name_ely(base: &str, name: &str) -> Option<NameAndId> {
    let url = format!(
        "{}/api/users/profiles/minecraft/{}",
        base.trim_end_matches('/'),
        percent_encoded(name)
    );
    found(&get(&url)?, name)
}

/// The Ely.by account with this id, by its Mojang-compatible `GET {base}/session/profile/{id}`, or None.
pub fn find_by_id_ely(base: &str, id: u128) -> Option<NameAndId> {
    let url = format!("{}/session/profile/{id:032x}", base.trim_end_matches('/'));
    let answer = get(&url)?;
    let name = json::parse(&answer, true).ok()?.get("name")?.as_string()?;
    found(&answer, &name).filter(|user| user.id == id)
}

/// The body of a successful GET; an error status gives None (a 204 gives an empty body, which `found` refuses).
fn get(url: &str) -> Option<String> {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .user_agent(&format!("Horizon/{}", horizon_core::release::VERSION))
        .build()
        .get(url)
        .call()
        .ok()?
        .into_string()
        .ok()
}

/// `name` as one URL path segment: everything but unreserved characters as `%XX`.
fn percent_encoded(name: &str) -> String {
    let mut out = String::new();
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// MinecraftServicesProfileRepository.findProfileByName: the account named
/// `name` (any case), or None when there is none or the services don't
/// answer (vanilla logs both as "Couldn't find profile with name").
pub fn find_by_name(discovery: &str, name: &str) -> Option<NameAndId> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .user_agent(&format!("Horizon/{}", horizon_core::release::VERSION))
        .build();
    let doc = json::parse(&agent.get(discovery).call().ok()?.into_string().ok()?, true).ok()?;
    let url = endpoint(&doc, "profiles", "getByName")?.replace("{name}", &name.to_lowercase());
    found(&agent.get(&url).call().ok()?.into_string().ok()?, name)
}

/// YggdrasilMinecraftSessionService.fetchProfile(id, true), as the management protocol finds a player given by
/// id that the user cache doesn't know: the `session` / `getProfileById` endpoint, or None.
pub fn find_by_id(discovery: &str, id: u128) -> Option<NameAndId> {
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .user_agent(&format!("Horizon/{}", horizon_core::release::VERSION))
        .build();
    let doc = json::parse(&agent.get(discovery).call().ok()?.into_string().ok()?, true).ok()?;
    let url =
        endpoint(&doc, "session", "getProfileById")?.replace("{profileId}", &format!("{id:032x}"));
    let separator = if url.contains('?') { '&' } else { '?' };
    let answer = agent
        .get(&format!("{url}{separator}unsigned=false"))
        .call()
        .ok()?
        .into_string()
        .ok()?;
    let name = json::parse(&answer, true).ok()?.get("name")?.as_string()?;
    found(&answer, &name).filter(|user| user.id == id)
}

/// MinecraftServicesKeyInfo.get: the services' public keys from the
/// discovery document's `authentication` / `getPublicKeys` endpoint. The
/// profile-key keys (`playerCertificateKeys`, ServicesKeyType.PROFILE_KEY)
/// sign the players' chat keys; secure chat checks them. The error is the
/// line vanilla logs, after which it has no keys.
pub fn fetch_profile_key_keys(discovery: &str) -> Result<Vec<RsaPublicKey>, &'static str> {
    const FAILED: &str = "Failed to request Minecraft Services public key";
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(15))
        .user_agent(&format!("Horizon/{}", horizon_core::release::VERSION))
        .build();
    let get = |url: &str| agent.get(url).call().ok()?.into_string().ok();
    let doc = get(discovery)
        .and_then(|d| json::parse(&d, true).ok())
        .ok_or(FAILED)?;
    let url = endpoint(&doc, "authentication", "getPublicKeys").ok_or(FAILED)?;
    profile_key_keys(&get(&url).ok_or(FAILED)?)
}

/// The key set answer: `{"profilePropertyKeys": [...], "playerCertificateKeys":
/// [...]}`, each `{"publicKey": base64 X.509}`. One bad key makes the whole
/// answer malformed (parseList throws).
pub fn profile_key_keys(answer: &str) -> Result<Vec<RsaPublicKey>, &'static str> {
    use base64::Engine;
    const MALFORMED: &str = "Received malformed Minecraft Services public key data";
    let doc = json::parse(answer, true).map_err(|_| MALFORMED)?;
    let list = |name: &str| -> Result<Vec<RsaPublicKey>, &'static str> {
        let Some(entries) = doc.get(name) else {
            return Ok(Vec::new());
        };
        let Json::Array(entries) = entries else {
            return Err(MALFORMED);
        };
        entries
            .iter()
            .map(|e| {
                let text = e
                    .get("publicKey")
                    .and_then(Json::as_string)
                    .ok_or(MALFORMED)?;
                let der = base64::engine::general_purpose::STANDARD
                    .decode(text)
                    .map_err(|_| MALFORMED)?;
                RsaPublicKey::from_der(&der).map_err(|_| MALFORMED)
            })
            .collect()
    };
    list("profilePropertyKeys")?;
    list("playerCertificateKeys")
}

/// MinecraftServicesDiscoveryService.getUrl(service, endpoint).
fn endpoint(doc: &Json, service: &str, name: &str) -> Option<String> {
    doc.get("discovery")?
        .get(service)?
        .get("endpoints")?
        .get(name)?
        .get("uri")?
        .as_string()
}

/// The answer, authlib's NameAndId: `{"id", "name"}`, the id undashed.
pub fn found(answer: &str, name: &str) -> Option<NameAndId> {
    let profile = json::parse(answer, true).ok()?;
    let found = profile.get("name")?.as_string()?;
    let id = profile.get("id")?.as_string()?.replace('-', "");
    (found.eq_ignore_ascii_case(name) && id.len() == 32).then_some(())?;
    Some(NameAndId {
        id: u128::from_str_radix(&id, 16).ok()?,
        name: found,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_answers() {
        assert!(valid_player_name("Alex_123"));
        assert!(!valid_player_name("two words"));
        assert!(!valid_player_name("ThisNameIsTooLong"));
        assert!(!valid_player_name("caf\u{e9}"));
        assert_eq!(resolve("Alex", None), Some(NameAndId::offline("Alex")));
        assert_eq!(resolve("not valid", Some(AuthProvider::Mojang)), None);
        let answer = r#"{"id":"069a79f444e94726a5befca90e38aaf5","name":"Notch"}"#;
        assert_eq!(
            found(answer, "notch"),
            Some(NameAndId {
                id: 0x069a79f444e94726a5befca90e38aaf5,
                name: "Notch".into()
            })
        );
        assert_eq!(found(answer, "Steve"), None);
        assert_eq!(found("{}", "Notch"), None);
        assert_eq!(found("", "Notch"), None);
    }

    #[test]
    fn services_key_sets() {
        // The 1024-bit test key of horizon-protocol's RSA tests, base64.
        let key = "MIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQCrAmB0+SRQqPcm84lbsRF2yo8GXzEAzUFDusT302dO3P+VtBoFJdI2WlScEG7GaFiBs5IXdCD9Mf9cFE/xG6JDEedRzSxrXRgXtq9mhqQ8ldYSQ2EwBeaUW2CTzZUiPOxeiFaCbanS7hsyM2KsHOvqs2WcvEUxmptUDdqBakeY/wIDAQAB";
        let answer = format!(
            r#"{{"profilePropertyKeys":[{{"publicKey":"{key}"}}],"playerCertificateKeys":[{{"publicKey":"{key}"}},{{"publicKey":"{key}"}}]}}"#
        );
        let keys = profile_key_keys(&answer).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].size(), 128);
        assert_eq!(profile_key_keys("{}").unwrap().len(), 0);
        let bad = r#"{"profilePropertyKeys":[{"publicKey":"AAAA"}],"playerCertificateKeys":[]}"#;
        assert!(profile_key_keys(bad).is_err());
        assert!(profile_key_keys("not json").is_err());
    }

    /// A local Ely.by: answers each request, in turn, with the given status line and body, and reports the
    /// request lines it got.
    fn ely(
        answers: &'static [(&'static str, &'static str)],
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                requests.push(line.trim_end().to_owned());
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line.trim().is_empty() {
                        break;
                    }
                }
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .unwrap();
            }
            requests
        });
        (base, server)
    }

    #[test]
    fn ely_names_are_asked_of_the_api_users_endpoint() {
        let (base, server) = ely(&[
            (
                "200 OK",
                r#"{"id":"ffc8fdc95824509e8a57c99b940fb996","name":"ErickSkrauch"}"#,
            ),
            ("204 No Content", ""),
            ("404 Not Found", r#"{"error":"nope"}"#),
            ("200 OK", r#"{"id":"ffc8fdc95824509e8a57c99b940fb996","name":"Someone"}"#),
        ]);
        assert_eq!(
            find_by_name_ely(&base, "ericksKRAUCH"),
            Some(NameAndId {
                id: 0xffc8fdc95824509e8a57c99b940fb996,
                name: "ErickSkrauch".into()
            })
        );
        assert_eq!(find_by_name_ely(&base, "Nobody"), None);
        assert_eq!(find_by_name_ely(&base, "Gone"), None);
        // An answer for another name is not the account asked for.
        assert_eq!(find_by_name_ely(&base, "Else"), None);
        assert_eq!(
            server.join().unwrap(),
            [
                "GET /api/users/profiles/minecraft/ericksKRAUCH HTTP/1.1",
                "GET /api/users/profiles/minecraft/Nobody HTTP/1.1",
                "GET /api/users/profiles/minecraft/Gone HTTP/1.1",
                "GET /api/users/profiles/minecraft/Else HTTP/1.1",
            ]
        );
        // Nothing answers at all.
        assert_eq!(find_by_name_ely("http://127.0.0.1:1", "Alex"), None);
    }

    #[test]
    fn ely_ids_are_asked_of_the_session_profile_endpoint() {
        let (base, server) = ely(&[
            (
                "200 OK",
                r#"{"id":"ffc8fdc95824509e8a57c99b940fb996","name":"ErickSkrauch","properties":[]}"#,
            ),
            ("204 No Content", ""),
            (
                "200 OK",
                r#"{"id":"00000000000000000000000000000002","name":"Other"}"#,
            ),
        ]);
        assert_eq!(
            find_by_id_ely(&base, 0xffc8fdc95824509e8a57c99b940fb996),
            Some(NameAndId {
                id: 0xffc8fdc95824509e8a57c99b940fb996,
                name: "ErickSkrauch".into()
            })
        );
        assert_eq!(find_by_id_ely(&base, 1), None);
        // The profile of another id is not the one asked for.
        assert_eq!(find_by_id_ely(&base, 1), None);
        assert_eq!(
            server.join().unwrap(),
            [
                "GET /session/profile/ffc8fdc95824509e8a57c99b940fb996 HTTP/1.1",
                "GET /session/profile/00000000000000000000000000000001 HTTP/1.1",
                "GET /session/profile/00000000000000000000000000000001 HTTP/1.1",
            ]
        );
    }

    #[test]
    fn names_are_one_url_segment() {
        assert_eq!(percent_encoded("Alex_1.2-x~"), "Alex_1.2-x~");
        assert_eq!(percent_encoded("a/b?c#d%"), "a%2Fb%3Fc%23d%25");
    }

    #[test]
    fn resolving_by_provider() {
        // Offline: the offline profile whatever the provider, with no lookup.
        assert_eq!(resolve("Alex", None), Some(NameAndId::offline("Alex")));
        // Online: a name no account can have is refused before any lookup.
        for provider in [AuthProvider::Mojang, AuthProvider::Elyby] {
            assert_eq!(resolve("not valid", Some(provider)), None);
        }
    }
}
