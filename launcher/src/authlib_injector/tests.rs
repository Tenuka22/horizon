//! The download API's answer, the checksum gate, and the kept jar, with the network made up by a
//! closure that records what was asked.
use super::*;
use std::cell::RefCell;

/// The real answer of `https://authlib-injector.yushi.moe/artifact/latest.json` (build 56).
const REAL_LATEST: &str = r#"{
  "build_number": 56,
  "version": "1.2.8",
  "release_time": "2026-07-11T17:14:13Z",
  "download_url": "https://authlib-injector.yushi.moe/artifact/56/authlib-injector-1.2.8.jar",
  "checksums": {
    "sha256": "9c7f4343e6c82034958ffb48c14a2cb0c85928be7283103ce17da00c6d5a7b10"
  }
}"#;

/// FIPS 180-2's SHA-256 test vectors for "abc" and the empty message.
const ABC: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

const DAY: Duration = Duration::from_secs(24 * 3600);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("horizon-authlib-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn latest(version: &str, url: &str, sha256: &str) -> Vec<u8> {
    serde_json::json!({
        "build_number": 1, "version": version, "download_url": url,
        "checksums": {"sha256": sha256}
    })
    .to_string()
    .into_bytes()
}

/// A made-up download API: `latest.json`, and a jar at one URL. `asked` lists the requests.
struct Api {
    latest: Vec<u8>,
    jar: Vec<u8>,
    asked: RefCell<Vec<String>>,
}

impl Api {
    fn new(version: &str, sha256: &str, jar: &[u8]) -> Api {
        Api {
            latest: latest(
                version,
                &format!("https://example.test/{version}.jar"),
                sha256,
            ),
            jar: jar.to_vec(),
            asked: RefCell::new(Vec::new()),
        }
    }

    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        self.asked.borrow_mut().push(url.to_string());
        if url == LATEST {
            Ok(self.latest.clone())
        } else if url.starts_with("https://example.test/") {
            Ok(self.jar.clone())
        } else {
            Err(format!("{url} answered 404"))
        }
    }

    fn asked(&self) -> Vec<String> {
        self.asked.borrow().clone()
    }
}

fn offline(url: &str) -> Result<Vec<u8>, String> {
    Err(format!("No connection: {url}"))
}

/// `ensure` against `api`, with the log lines it wrote.
fn run(dir: &Path, api: &Api, now: SystemTime) -> (Result<PathBuf, String>, Vec<String>) {
    let lines = RefCell::new(Vec::new());
    let jar = ensure(dir, &|u| api.fetch(u), now, &|l| lines.borrow_mut().push(l));
    (jar, lines.into_inner())
}

/// Makes the last look at the API `age` ago.
fn age_info(dir: &Path, age: Duration) {
    fs::File::options()
        .write(true)
        .open(dir.join(INFO))
        .unwrap()
        .set_modified(SystemTime::now() - age)
        .unwrap();
}

#[test]
fn the_real_latest_json_is_read() {
    assert_eq!(
        parse_latest(REAL_LATEST.as_bytes()).unwrap(),
        Artifact {
            version: "1.2.8".into(),
            url: "https://authlib-injector.yushi.moe/artifact/56/authlib-injector-1.2.8.jar".into(),
            sha256: "9c7f4343e6c82034958ffb48c14a2cb0c85928be7283103ce17da00c6d5a7b10".into(),
        }
    );
}

#[test]
fn an_answer_that_is_not_https_or_has_no_checksum_is_refused() {
    let with = |url: &str, sha: &str| latest("1.2.8", url, sha);
    assert!(parse_latest(&with("http://example.test/a.jar", ABC)).is_err());
    assert!(parse_latest(&with("https://example.test/a.jar", "abc123")).is_err());
    assert!(parse_latest(&with("https://example.test/a.jar", &"z".repeat(64))).is_err());
    assert!(parse_latest(&with("", ABC)).is_err());
    assert!(parse_latest(b"{}").is_err());
    assert!(parse_latest(b"<html>").is_err());
    // The checksum may come in capitals; it is kept in lowercase.
    let upper = parse_latest(&with("https://example.test/a.jar", &ABC.to_uppercase())).unwrap();
    assert_eq!(upper.sha256, ABC);
}

#[test]
fn the_agent_argument_names_the_jar_and_ely_by() {
    assert_eq!(
        javaagent(Path::new("/data/authlib-injector.jar")),
        "-javaagent:/data/authlib-injector.jar=ely.by"
    );
}

#[test]
fn sha256_matches_the_published_vectors() {
    assert_eq!(sha256_hex(b"abc"), ABC);
    assert_eq!(sha256_hex(b""), EMPTY);
}

#[test]
fn the_first_launch_downloads_checks_and_keeps_the_jar() {
    let dir = scratch("first");
    let api = Api::new("1.2.8", ABC, b"abc");
    let (jar, lines) = run(&dir, &api, SystemTime::now());
    let jar = jar.unwrap();
    assert_eq!(jar, dir.join(format!("authlib-injector-{ABC}.jar")));
    assert_eq!(fs::read(&jar).unwrap(), b"abc");
    assert_eq!(api.asked(), [LATEST, "https://example.test/1.2.8.jar"]);
    assert_eq!(lines.len(), 1);
    assert!(lines[0].contains("1.2.8"), "{lines:?}");
    let info: serde_json::Value =
        serde_json::from_slice(&fs::read(dir.join(INFO)).unwrap()).unwrap();
    assert_eq!(info, serde_json::json!({"version": "1.2.8", "sha256": ABC}));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_jar_that_does_not_match_its_checksum_is_refused() {
    let dir = scratch("mismatch");
    // The API promises the hash of "abc" and serves something else.
    let api = Api::new("1.2.8", ABC, b"abd");
    let error = run(&dir, &api, SystemTime::now()).0.unwrap_err();
    assert!(error.contains("checksum"), "{error}");
    assert!(
        !dir.exists() || fs::read_dir(&dir).unwrap().next().is_none(),
        "nothing is kept"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_unreachable_api_with_nothing_kept_is_an_error_that_says_why() {
    let dir = scratch("offline-first");
    let error = ensure(&dir, &offline, SystemTime::now(), &|_| {}).unwrap_err();
    assert!(error.contains("No connection"), "{error}");
    assert!(error.contains("authlib-injector"), "{error}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_kept_jar_is_reused_offline_and_without_asking_for_a_week() {
    let dir = scratch("reuse");
    let now = SystemTime::now();
    let api = Api::new("1.2.8", ABC, b"abc");
    let first = run(&dir, &api, now).0.unwrap();
    let asked = api.asked().len();
    // No network at all, a day later: not even a log line.
    let again = ensure(&dir, &offline, now + DAY, &|_| panic!("nothing to say")).unwrap();
    assert_eq!(again, first);
    assert_eq!(api.asked().len(), asked);
    // After the week the API is asked; with the network down the kept jar still serves.
    let lines = RefCell::new(Vec::new());
    let stale = ensure(&dir, &offline, now + 8 * DAY, &|l| {
        lines.borrow_mut().push(l)
    })
    .unwrap();
    assert_eq!(stale, first);
    let lines = lines.into_inner();
    assert_eq!(lines.len(), 1);
    assert!(
        lines[0].contains("1.2.8") && lines[0].contains("No connection"),
        "{lines:?}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_kept_jar_that_changed_on_disk_is_not_started() {
    let dir = scratch("tampered");
    let now = SystemTime::now();
    let api = Api::new("1.2.8", ABC, b"abc");
    let jar = run(&dir, &api, now).0.unwrap();
    fs::write(&jar, b"abd").unwrap();
    // Offline there is nothing right to start the game with.
    assert!(ensure(&dir, &offline, now, &|_| {}).is_err());
    // Online the jar is fetched again and checked.
    let (fixed, lines) = run(&dir, &api, now);
    assert_eq!(fs::read(fixed.unwrap()).unwrap(), b"abc");
    assert_eq!(lines.len(), 1);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_newer_build_replaces_the_old_one_and_the_same_build_is_not_fetched_again() {
    let dir = scratch("update");
    let old = run(&dir, &Api::new("1.2.7", ABC, b"abc"), SystemTime::now())
        .0
        .unwrap();
    // The same build is still the newest: only latest.json is asked, and the week starts over.
    age_info(&dir, 8 * DAY);
    assert!(!fresh(&dir, SystemTime::now()));
    let same = Api::new("1.2.7", ABC, b"abc");
    assert_eq!(run(&dir, &same, SystemTime::now()).0.unwrap(), old);
    assert_eq!(same.asked(), [LATEST]);
    assert!(fresh(&dir, SystemTime::now()));
    // A new build: downloaded, checked, and the old jar goes.
    age_info(&dir, 8 * DAY);
    let newer = Api::new("1.2.8", EMPTY, b"");
    let new = run(&dir, &newer, SystemTime::now()).0.unwrap();
    assert_eq!(new, dir.join(format!("authlib-injector-{EMPTY}.jar")));
    assert!(!old.exists(), "the old build is removed");
    assert_eq!(kept(&dir).unwrap().version, "1.2.8");
    let _ = fs::remove_dir_all(&dir);
}
