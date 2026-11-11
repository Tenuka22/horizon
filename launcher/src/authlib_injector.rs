//! authlib-injector, the Java agent that points Minecraft: Java Edition's sign-in and session
//! checks at Ely.by (<https://docs.ely.by/en/authlib-injector.html>): the game is started with
//! `-javaagent:<jar>=ely.by`, and the launcher still passes the Ely.by access token itself.
//!
//! The jar is Ely.by's documented way in, not Mojang's file. It is fetched once from
//! authlib-injector's own download API (`GET /artifact/latest.json`, which names the jar's URL and
//! its SHA-256, <https://yushijinhun.github.io/authlib-injector/en/get-authlib-injector.html>),
//! checked against that SHA-256, and kept in the launcher's data folder under a name that holds
//! the checksum. Every launch checks the kept jar again; it is reused offline, and looked up
//! afresh at most once a week. A jar that does not match its checksum is never started.

use crate::i18n::{tr, trf};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// authlib-injector's own metadata for its newest build.
pub const LATEST: &str = horizon_addresses::AUTHLIB_INJECTOR_LATEST;

/// The agent's argument for Ely.by's servers (its documented shorthand).
pub const ELY_BY: &str = "ely.by";

/// How long a kept jar is used without asking [`LATEST`] whether there is a newer build.
const REFRESH: Duration = Duration::from_secs(7 * 24 * 3600);

/// What is kept about the current jar: its version and SHA-256. The file's age is the age of the
/// last look at [`LATEST`].
const INFO: &str = "authlib-injector.json";

/// A file's body from the network (`backend::get_bytes`: it goes through the player's proxy).
pub type Fetch<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, String>;

/// A build of authlib-injector as its download API describes it.
#[derive(Debug, PartialEq, Eq)]
pub struct Artifact {
    pub version: String,
    pub url: String,
    /// Lowercase hex, 64 digits.
    pub sha256: String,
}

/// A jar that is kept and has just been checked against its SHA-256.
struct Kept {
    version: String,
    sha256: String,
    jar: PathBuf,
}

/// Where the jar is kept.
pub fn dir() -> PathBuf {
    crate::backend::data_dir()
        .join("minecraft")
        .join("authlib-injector")
}

/// The JVM argument that starts `jar` as the agent for Ely.by. It goes before every other one.
pub fn javaagent(jar: &Path) -> String {
    format!("-javaagent:{}={ELY_BY}", jar.display())
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn is_sha256(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The jar's own name: the checksum makes a changed build a different file, so replacing it never
/// touches one a running game has open.
fn jar_name(sha256: &str) -> String {
    format!("authlib-injector-{sha256}.jar")
}

/// The answer of `latest.json`: `version`, `download_url` and `checksums.sha256`. The URL must be
/// HTTPS: the checksum comes from the same service, so the transport is half of the protection.
pub fn parse_latest(body: &[u8]) -> Result<Artifact, String> {
    let bad =
        || tr("authlib-injector's download information is not in the expected form").to_string();
    let json: serde_json::Value = serde_json::from_slice(body).map_err(|_| bad())?;
    let text = |value: &serde_json::Value| value.as_str().map(str::to_owned);
    let version = text(&json["version"])
        .filter(|v| !v.is_empty())
        .ok_or_else(bad)?;
    let url = text(&json["download_url"])
        .filter(|u| u.starts_with("https://"))
        .ok_or_else(bad)?;
    let sha256 = text(&json["checksums"]["sha256"])
        .filter(|s| is_sha256(s))
        .ok_or_else(bad)?
        .to_ascii_lowercase();
    Ok(Artifact {
        version,
        url,
        sha256,
    })
}

/// The jar `dir` keeps, if it is there and matches the checksum recorded with it.
fn kept(dir: &Path) -> Option<Kept> {
    let info: serde_json::Value = serde_json::from_slice(&fs::read(dir.join(INFO)).ok()?).ok()?;
    let sha256 = info["sha256"].as_str().filter(|s| is_sha256(s))?.to_owned();
    let jar = dir.join(jar_name(&sha256));
    if sha256_hex(&fs::read(&jar).ok()?) != sha256 {
        return None;
    }
    Some(Kept {
        version: info["version"].as_str().unwrap_or_default().to_owned(),
        sha256,
        jar,
    })
}

/// Written whole or not at all: a crash never leaves half a file under its name.
fn write_atomic(path: &Path, body: &[u8]) -> Result<(), String> {
    let part = path.with_extension("part");
    fs::write(&part, body)
        .and_then(|()| fs::rename(&part, path))
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn record(dir: &Path, version: &str, sha256: &str) -> Result<(), String> {
    let info = serde_json::json!({"version": version, "sha256": sha256});
    write_atomic(&dir.join(INFO), info.to_string().as_bytes())
}

/// Whether the last look at [`LATEST`] is recent enough to use the kept jar as it is.
fn fresh(dir: &Path, now: SystemTime) -> bool {
    fs::metadata(dir.join(INFO))
        .and_then(|m| m.modified())
        .is_ok_and(|at| now.duration_since(at).map_or(true, |age| age < REFRESH))
}

/// Asks [`LATEST`] and downloads the build it names unless that is the jar already kept.
fn update(
    dir: &Path,
    fetch: Fetch,
    kept: Option<&Kept>,
    log: &dyn Fn(String),
) -> Result<PathBuf, String> {
    let artifact = parse_latest(&fetch(LATEST)?)?;
    if let Some(kept) = kept.filter(|k| k.sha256 == artifact.sha256) {
        record(dir, &artifact.version, &artifact.sha256)?;
        return Ok(kept.jar.clone());
    }
    log(trf(
        "Downloading authlib-injector {version}, which Ely.by sign-in in Java Edition needs",
        &[("version", &artifact.version)],
    ));
    let body = fetch(&artifact.url)?;
    if sha256_hex(&body) != artifact.sha256 {
        return Err(
            tr("authlib-injector's download did not match the checksum it publishes").into(),
        );
    }
    fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let jar = dir.join(jar_name(&artifact.sha256));
    write_atomic(&jar, &body)?;
    record(dir, &artifact.version, &artifact.sha256)?;
    // The builds before this one are no use any more; one a game has open stays where it is.
    for old in fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = old.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("authlib-injector-") && name.ends_with(".jar") && old.path() != jar {
            let _ = fs::remove_file(old.path());
        }
    }
    Ok(jar)
}

/// The path of a jar that matches its checksum, ready to be the agent. A jar kept in `dir` that
/// still matches is used as it is for a week (offline too); after that, and the first time, the
/// newest build is looked up, downloaded and checked. When that fails, a kept jar still serves;
/// with none there is nothing to start the game with, and the error says why.
pub fn ensure(
    dir: &Path,
    fetch: Fetch,
    now: SystemTime,
    log: &dyn Fn(String),
) -> Result<PathBuf, String> {
    let kept = kept(dir);
    if let Some(kept) = kept.as_ref().filter(|_| fresh(dir, now)) {
        return Ok(kept.jar.clone());
    }
    match update(dir, fetch, kept.as_ref(), log) {
        Ok(jar) => Ok(jar),
        Err(error) => match kept {
            Some(kept) => {
                log(trf(
                    "Could not update authlib-injector ({error}); using version {version}, already downloaded",
                    &[("error", &error), ("version", &kept.version)],
                ));
                Ok(kept.jar)
            }
            None => Err(trf(
                "Ely.by sign-in in Java Edition needs authlib-injector, which could not be downloaded: {error}",
                &[("error", &error)],
            )),
        },
    }
}

#[cfg(test)]
#[path = "authlib_injector/tests.rs"]
mod tests;
