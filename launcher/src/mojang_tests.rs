//! A download from a mock of Mojang's servers serving small fake files (never Mojang's).
use super::*;
use crate::sources::{resolve, sha1_hex, Resolved};
use std::collections::HashMap;

const CLIENT: &[u8] = b"fake client jar";
const COMMON: &[u8] = b"a library every system uses";
const LINUX: &[u8] = b"a linux native";
const SOUND: &[u8] = b"a fake ogg";
const LANG: &[u8] = b"{}";
const LOGGING: &[u8] = b"<Configuration/>";
/// Never cancelled.
static GO: AtomicBool = AtomicBool::new(false);

/// The fake version file, asset index and files, by URL.
fn mojang() -> (Pins, HashMap<String, Vec<u8>>) {
    let artifact = |name: &str, data: &[u8]| {
        serde_json::json!({
            "path": format!("org/fake/{name}/1/{name}-1.jar"),
            "sha1": sha1_hex(data),
            "url": format!("https://libraries.minecraft.net/org/fake/{name}/1/{name}-1.jar"),
        })
    };
    let index = serde_json::json!({"objects": {
        "minecraft/sounds/a.ogg": {"hash": sha1_hex(SOUND), "size": SOUND.len()},
        "minecraft/sounds/same.ogg": {"hash": sha1_hex(SOUND), "size": SOUND.len()},
        "minecraft/lang/fr_fr.json": {"hash": sha1_hex(LANG), "size": LANG.len()},
    }})
    .to_string();
    let manifest = serde_json::json!({
        "id": "26.3",
        "assetIndex": {"id": "34", "sha1": sha1_hex(index.as_bytes()),
                       "url": "https://piston-meta.mojang.com/v1/packages/i/34.json"},
        "downloads": {"client": {"sha1": sha1_hex(CLIENT), "size": CLIENT.len(), "url": "https://piston-data.mojang.com/client.jar"}},
        "libraries": [
            {"name": "org.fake:common:1", "downloads": {"artifact": artifact("common", COMMON)}},
            {"name": "org.fake:linux:1", "downloads": {"artifact": artifact("linux", LINUX)},
             "rules": [{"action": "allow", "os": {"name": "linux"}}]},
            {"name": "org.fake:not-osx:1", "downloads": {"artifact": artifact("not-osx", COMMON)},
             "rules": [{"action": "allow"}, {"action": "disallow", "os": {"name": "osx"}}]},
            {"name": "org.fake:windows:1", "downloads": {"artifact": artifact("windows", b"w")},
             "rules": [{"action": "allow", "os": {"name": "windows"}}]},
        ],
    })
    .to_string();
    let pins = Pins::read(&serde_json::json!({"minecraft": {
        "version": "26.3",
        "client_sha1": sha1_hex(CLIENT),
        "manifest_sha1": sha1_hex(manifest.as_bytes()),
    }}))
    .unwrap();
    let object = |data: &[u8]| {
        let hash = sha1_hex(data);
        format!("{OBJECTS}/{}/{hash}", &hash[..2])
    };
    let lib =
        |name: &str| format!("https://libraries.minecraft.net/org/fake/{name}/1/{name}-1.jar");
    let files = HashMap::from([
        (
            format!(
                "https://piston-meta.mojang.com/v1/packages/{}/26.3.json",
                sha1_hex(manifest.as_bytes())
            ),
            manifest.into_bytes(),
        ),
        (
            "https://piston-data.mojang.com/client.jar".into(),
            CLIENT.to_vec(),
        ),
        (lib("common"), COMMON.to_vec()),
        (lib("linux"), LINUX.to_vec()),
        (lib("not-osx"), COMMON.to_vec()),
        (lib("windows"), b"w".to_vec()),
        (
            "https://piston-meta.mojang.com/v1/packages/i/34.json".into(),
            index.into_bytes(),
        ),
        (object(SOUND), SOUND.to_vec()),
        (object(LANG), LANG.to_vec()),
    ]);
    (pins, files)
}

struct Server {
    files: HashMap<String, Vec<u8>>,
    asked: Mutex<Vec<String>>,
}

impl Server {
    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        self.asked.lock().unwrap().push(url.to_string());
        self.files
            .get(url)
            .cloned()
            .ok_or_else(|| format!("{url} answered 404"))
    }

    fn asked(&self) -> Vec<String> {
        std::mem::take(&mut *self.asked.lock().unwrap())
    }
}

/// A 64-bit x86 system named `os`, as most of these tests run on.
fn sys(os: &'static str) -> System<'static> {
    System {
        os,
        arch: Arch::X86_64,
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("horizon-mojang-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_download_gets_the_official_layout_with_this_systems_libraries() {
    let (pins, files) = mojang();
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("layout");
    let logged = Mutex::new(Vec::new());
    download(
        &pins,
        &dir,
        sys("linux"),
        3,
        &|url| server.fetch(url),
        &GO,
        &|line| logged.lock().unwrap().push(line),
    )
    .unwrap();
    assert_eq!(
        fs::read(dir.join("versions/26.3/26.3.jar")).unwrap(),
        CLIENT
    );
    let libraries = dir.join("libraries/org/fake");
    assert_eq!(
        fs::read(libraries.join("common/1/common-1.jar")).unwrap(),
        COMMON
    );
    assert_eq!(
        fs::read(libraries.join("linux/1/linux-1.jar")).unwrap(),
        LINUX
    );
    assert!(libraries.join("not-osx/1/not-osx-1.jar").is_file());
    assert!(!libraries.join("windows").exists());
    let hash = sha1_hex(SOUND);
    assert_eq!(
        fs::read(dir.join("assets/objects").join(&hash[..2]).join(&hash)).unwrap(),
        SOUND
    );
    assert!(dir.join("assets/indexes/34.json").is_file());
    let asked = server.asked();
    // The sound two names share is fetched once; nothing is fetched twice.
    assert_eq!(asked.len(), 8, "{asked:#?}");
    assert!(!asked.iter().any(|u| u.contains("windows")));
    assert_eq!(
        logged.lock().unwrap()[1],
        "Downloading Minecraft 26.3 from Mojang: the game and 3 libraries"
    );

    // Found like any launcher's folder, as it is; a second download fetches nothing.
    let found = resolve(
        &dir,
        &pins,
        &[],
        &scratch("unused"),
        &|url| server.fetch(url),
        &|_| {},
    );
    assert_eq!(found, Ok(Resolved::Folder(dir.clone())));
    download(
        &pins,
        &dir,
        sys("linux"),
        3,
        &|url| server.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap();
    assert_eq!(server.asked(), Vec::<String>::new());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn macos_skips_what_its_rules_disallow() {
    let (pins, files) = mojang();
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("osx");
    download(
        &pins,
        &dir,
        sys("osx"),
        2,
        &|url| server.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap();
    let libraries = dir.join("libraries/org/fake");
    assert!(libraries.join("common/1/common-1.jar").is_file());
    assert!(!libraries.join("linux").exists());
    assert!(!libraries.join("not-osx").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_that_is_not_the_pinned_one_stops_the_download() {
    let (pins, mut files) = mojang();
    files.insert(
        "https://piston-data.mojang.com/client.jar".into(),
        b"fake client JAR".to_vec(),
    );
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("bad-jar");
    let error = download(
        &pins,
        &dir,
        sys("linux"),
        2,
        &|url| server.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap_err();
    assert!(
        error.starts_with("https://piston-data.mojang.com/client.jar is not the one Horizon pins"),
        "{error}"
    );
    assert!(!dir.join("versions/26.3/26.3.jar").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn missing_sounds_are_a_line_in_the_log_not_an_error() {
    let (pins, mut files) = mojang();
    let hash = sha1_hex(SOUND);
    files.remove(&format!("{OBJECTS}/{}/{hash}", &hash[..2]));
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("no-sound");
    let logged = Mutex::new(Vec::new());
    download(
        &pins,
        &dir,
        sys("linux"),
        1,
        &|url| server.fetch(url),
        &GO,
        &|line| logged.lock().unwrap().push(line),
    )
    .unwrap();
    assert!(dir.join("versions/26.3/26.3.jar").is_file());
    let last = logged.lock().unwrap().last().cloned().unwrap();
    assert!(
        last.starts_with("The sounds could not all be downloaded:")
            && last.ends_with("answered 404"),
        "{last}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn version_file_paths_stay_in_the_folder() {
    assert!(relative("org/fake/a.jar").is_ok());
    for bad in ["../a.jar", "/etc/a", "a/../../b", "a\\b"] {
        assert!(relative(bad).is_err(), "{bad}");
    }
}

#[test]
fn links_stay_inside_the_runtime() {
    let at = |p: &str| PathBuf::from(p);
    assert!(link_stays_inside(
        &at("legal/java.compiler/LICENSE"),
        "../java.base/LICENSE"
    ));
    assert!(link_stays_inside(&at("lib/a"), "b"));
    assert!(!link_stays_inside(&at("lib/a"), "../../b"));
    assert!(!link_stays_inside(&at("a"), "/etc/passwd"));
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn java_runtime_lays_out_mojangs_runtime_with_its_executables_and_links() {
    const JAVAC: &[u8] = b"#!/bin/sh\necho javac 25.0.1\n";
    const JAVA: &[u8] = b"#!/bin/sh\necho java 25.0.1\n";
    const LICENSE: &[u8] = b"a fake license";
    let manifest = serde_json::json!({"files": {
        "bin": {"type": "directory"},
        "bin/java": {"type": "file", "executable": true, "downloads": {"raw":
            {"sha1": sha1_hex(JAVA), "size": JAVA.len(), "url": "https://piston-data.mojang.com/java"}}},
        "bin/javac": {"type": "file", "executable": true, "downloads": {"raw":
            {"sha1": sha1_hex(JAVAC), "size": JAVAC.len(), "url": "https://piston-data.mojang.com/javac"}}},
        "legal/java.base/LICENSE": {"type": "file", "executable": false, "downloads": {"raw":
            {"sha1": sha1_hex(LICENSE), "size": LICENSE.len(), "url": "https://piston-data.mojang.com/license"}}},
        "legal/java.compiler/LICENSE": {"type": "link", "target": "../java.base/LICENSE"},
    }})
    .to_string();
    let index = serde_json::json!({"linux": {"java-runtime-epsilon": [{
        "manifest": {"sha1": sha1_hex(manifest.as_bytes()), "url": "https://piston-meta.mojang.com/m.json"},
        "version": {"name": "25.0.1"},
    }]}})
    .to_string();
    let server = Server {
        files: HashMap::from([
            (JAVA_RUNTIMES.to_string(), index.into_bytes()),
            (
                "https://piston-meta.mojang.com/m.json".into(),
                manifest.into_bytes(),
            ),
            ("https://piston-data.mojang.com/java".into(), JAVA.to_vec()),
            (
                "https://piston-data.mojang.com/javac".into(),
                JAVAC.to_vec(),
            ),
            (
                "https://piston-data.mojang.com/license".into(),
                LICENSE.to_vec(),
            ),
        ]),
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("java");
    let home = java_runtime(
        "java-runtime-epsilon",
        &dir,
        2,
        &|url| server.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap();
    assert_eq!(
        home,
        dir.join("java-runtime-epsilon/linux/java-runtime-epsilon")
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(home.join("bin/javac"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o111, 0o111);
    assert_eq!(
        fs::read(home.join("legal/java.compiler/LICENSE")).unwrap(),
        LICENSE
    );
    // horizon-prepare finds it where the official launcher would have put it.
    assert_eq!(
        horizon_platform::prepare::java::minecraft_runtimes(&dir),
        [home]
    );
    // A second call keeps the files: only the index and manifest are asked again.
    server.asked();
    java_runtime(
        "java-runtime-epsilon",
        &dir,
        2,
        &|url| server.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap();
    assert_eq!(server.asked().len(), 2);
    assert!(java_runtime(
        "java-runtime-zeta",
        &dir,
        2,
        &|url| server.fetch(url),
        &GO,
        &|_| {}
    )
    .is_err());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_file_of_another_size_than_mojang_lists_is_refused() {
    let (pins, mut files) = mojang();
    // A cut-off body: its size says so before its sha1.
    files.insert(
        "https://piston-data.mojang.com/client.jar".into(),
        CLIENT[..4].to_vec(),
    );
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("size");
    let error = download(
        &pins,
        &dir,
        sys("linux"),
        2,
        &|url| server.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap_err();
    assert!(
        error.ends_with(&format!(
            "is 4 bytes, not the {} Mojang lists",
            CLIENT.len()
        )),
        "{error}"
    );
    assert!(!dir.join("versions/26.3/26.3.jar").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_cancelled_download_goes_on_where_it_stopped() {
    let (pins, files) = mojang();
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("cancel");
    let cancel = AtomicBool::new(false);
    // Cancelled once the client jar has arrived: one job, so nothing after it starts.
    let fetch = |url: &str| {
        let body = server.fetch(url);
        if url.ends_with("client.jar") {
            cancel.store(true, Relaxed);
        }
        body
    };
    let error = download(&pins, &dir, sys("linux"), 1, &fetch, &cancel, &|_| {}).unwrap_err();
    assert_eq!(error, CANCELLED);
    assert_eq!(
        fs::read(dir.join("versions/26.3/26.3.jar")).unwrap(),
        CLIENT
    );
    assert!(!dir.join("libraries").exists());
    // Again: the version file and the jar are kept, only the rest is fetched.
    cancel.store(false, Relaxed);
    server.asked();
    download(
        &pins,
        &dir,
        sys("linux"),
        1,
        &|url| server.fetch(url),
        &cancel,
        &|_| {},
    )
    .unwrap();
    let asked = server.asked();
    assert!(
        !asked
            .iter()
            .any(|u| u.ends_with("client.jar") || u.ends_with("26.3.json")),
        "{asked:#?}"
    );
    assert_eq!(asked.len(), 6, "{asked:#?}");
    fs::remove_dir_all(&dir).unwrap();
}

/// A version Horizon doesn't pin, as Mojang's list names it (its version file's sha1): the
/// client's sha1 comes from that checked file, and an old version's natives for this system
/// (a classifier of their library, `${arch}` filled in) come with the libraries.
#[test]
fn any_listed_version_comes_with_its_natives() {
    let native = |name: &str, data: &[u8]| {
        serde_json::json!({
            "path": format!("org/lwjgl/lwjgl-platform/2.9.4/lwjgl-platform-2.9.4-{name}.jar"),
            "sha1": sha1_hex(data),
            "url": format!("https://libraries.minecraft.net/lwjgl-platform-{name}.jar"),
        })
    };
    let index = serde_json::json!({"objects": {}}).to_string();
    let manifest = serde_json::json!({
        "id": "1.12.2",
        "assetIndex": {"id": "1.12", "sha1": sha1_hex(index.as_bytes()),
                       "url": "https://piston-meta.mojang.com/v1/packages/i/1.12.json"},
        "downloads": {"client": {"sha1": sha1_hex(CLIENT), "size": CLIENT.len(), "url": "https://piston-data.mojang.com/old.jar"}},
        "logging": {"client": {"argument": "-Dlog4j.configurationFile=${path}", "file": {
            "id": "client-1.12.xml", "sha1": sha1_hex(LOGGING), "size": LOGGING.len(),
            "url": "https://piston-data.mojang.com/client-1.12.xml"}}},
        "libraries": [{
            "name": "org.lwjgl.lwjgl:lwjgl-platform:2.9.4",
            "natives": {"osx": "natives-osx", "windows": "natives-windows-${arch}"},
            "downloads": {"classifiers": {
                "natives-osx": native("natives-osx", b"osx natives"),
                "natives-windows-64": native("natives-windows-64", b"win64 natives"),
                "natives-windows-32": native("natives-windows-32", b"win32 natives"),
            }},
        }],
    })
    .to_string();
    let sha1 = sha1_hex(manifest.as_bytes());
    let lib = |name: &str| format!("https://libraries.minecraft.net/lwjgl-platform-{name}.jar");
    let server = Server {
        files: HashMap::from([
            (
                format!("https://piston-meta.mojang.com/v1/packages/{sha1}/1.12.2.json"),
                manifest.into_bytes(),
            ),
            (
                "https://piston-data.mojang.com/old.jar".into(),
                CLIENT.to_vec(),
            ),
            (
                "https://piston-data.mojang.com/client-1.12.xml".into(),
                LOGGING.to_vec(),
            ),
            (lib("natives-osx"), b"osx natives".to_vec()),
            (lib("natives-windows-64"), b"win64 natives".to_vec()),
            (lib("natives-windows-32"), b"win32 natives".to_vec()),
            (
                "https://piston-meta.mojang.com/v1/packages/i/1.12.json".into(),
                index.into_bytes(),
            ),
        ]),
        asked: Mutex::new(Vec::new()),
    };
    let pins = Pins::for_version("1.12.2", &sha1);
    let platform = |system: System| {
        let dir = scratch(&format!("any-{}-{}", system.os, system.arch.name()));
        download(
            &pins,
            &dir,
            system,
            2,
            &|url| server.fetch(url),
            &GO,
            &|_| {},
        )
        .unwrap();
        dir
    };
    let mac = platform(sys("osx"));
    assert_eq!(
        fs::read(mac.join("versions/1.12.2/1.12.2.jar")).unwrap(),
        CLIENT
    );
    let natives = |dir: &Path, name: &str| {
        dir.join(format!(
            "libraries/org/lwjgl/lwjgl-platform/2.9.4/lwjgl-platform-2.9.4-{name}.jar"
        ))
    };
    assert!(natives(&mac, "natives-osx").is_file());
    assert_eq!(
        fs::read(mac.join("assets/log_configs/client-1.12.xml")).unwrap(),
        LOGGING
    );
    assert!(!natives(&mac, "natives-windows-64").exists());
    let windows = platform(sys("windows"));
    assert!(natives(&windows, "natives-windows-64").is_file());
    assert!(!natives(&windows, "natives-windows-32").exists());
    // Linux has no natives in this library: nothing to fetch for it, and no error.
    let linux = platform(sys("linux"));
    // ARM has no plain entry (Mojang's are x86's): the same, and no error.
    let arm = platform(on("linux", Arch::Arm64));
    assert!(!arm.join("libraries").exists());
    fs::remove_dir_all(arm).unwrap();
    assert!(!linux.join("libraries").exists());
    assert!(linux.join("assets/log_configs/client-1.12.xml").is_file());
    for dir in [mac, windows, linux] {
        fs::remove_dir_all(dir).unwrap();
    }

    // A client that isn't the one its checked version file names is refused.
    let wrong = Server {
        files: server
            .files
            .clone()
            .into_iter()
            .map(|(url, body)| match url.ends_with("old.jar") {
                true => (url, b"another jar".to_vec()),
                false => (url, body),
            })
            .collect(),
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("any-wrong");
    let refused = download(
        &pins,
        &dir,
        sys("osx"),
        1,
        &|url| wrong.fetch(url),
        &GO,
        &|_| {},
    );
    assert!(refused.unwrap_err().contains("old.jar"));
    let _ = fs::remove_dir_all(&dir);
}

/// A fetch that fails the first `fails` times each of `urls` is asked for, and counts every ask.
struct Flaky {
    server: Server,
    urls: Vec<String>,
    fails: usize,
    asked: Mutex<HashMap<String, usize>>,
}

impl Flaky {
    fn new(files: HashMap<String, Vec<u8>>, urls: &[String], fails: usize) -> Self {
        Flaky {
            server: Server {
                files,
                asked: Mutex::new(Vec::new()),
            },
            urls: urls.to_vec(),
            fails,
            asked: Mutex::new(HashMap::new()),
        }
    }

    fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let mut asked = self.asked.lock().unwrap();
        let times = asked.entry(url.to_string()).or_default();
        *times += 1;
        if self.urls.iter().any(|u| u == url) && *times <= self.fails {
            return Err(format!("timed out (ask {times})"));
        }
        self.server.fetch(url)
    }

    fn times(&self, url: &str) -> usize {
        self.asked.lock().unwrap().get(url).copied().unwrap_or(0)
    }
}

fn library_url(name: &str) -> String {
    format!("https://libraries.minecraft.net/org/fake/{name}/1/{name}-1.jar")
}

#[test]
fn a_file_that_fails_is_tried_again_and_the_third_try_can_win() {
    let (pins, files) = mojang();
    let linux = library_url("linux");
    let flaky = Flaky::new(
        files,
        &[
            linux.clone(),
            "https://piston-data.mojang.com/client.jar".into(),
        ],
        2,
    );
    let dir = scratch("retry");
    let logged = Mutex::new(Vec::new());
    download(
        &pins,
        &dir,
        sys("linux"),
        2,
        &|url| flaky.fetch(url),
        &GO,
        &|line| logged.lock().unwrap().push(line),
    )
    .unwrap();
    assert_eq!(
        fs::read(dir.join("libraries/org/fake/linux/1/linux-1.jar")).unwrap(),
        LINUX
    );
    assert_eq!(
        fs::read(dir.join("versions/26.3/26.3.jar")).unwrap(),
        CLIENT
    );
    // Two failures, then the file: three asks, and the others once.
    assert_eq!(flaky.times(&linux), 3);
    assert_eq!(flaky.times(&library_url("common")), 1);
    let logged = logged.lock().unwrap();
    assert!(
        logged
            .iter()
            .any(|l| l == "2 files failed; trying them again (try 2 of 4)"),
        "{logged:#?}"
    );
    assert!(
        logged
            .iter()
            .any(|l| l == "2 files failed; trying them again (try 3 of 4)"),
        "{logged:#?}"
    );
    drop(logged);
    fs::remove_dir_all(&dir).unwrap();

    // The fourth try is the last one that counts: three failures still end well.
    let (pins, files) = mojang();
    let flaky = Flaky::new(files, std::slice::from_ref(&linux), 3);
    let dir = scratch("retry-last");
    download(
        &pins,
        &dir,
        sys("linux"),
        1,
        &|url| flaky.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap();
    assert_eq!(flaky.times(&linux), 4);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn every_file_that_keeps_failing_is_named() {
    let (pins, files) = mojang();
    let (common, linux) = (library_url("common"), library_url("linux"));
    let flaky = Flaky::new(files, &[common.clone(), linux.clone()], 4);
    let dir = scratch("failed-all");
    let error = download(
        &pins,
        &dir,
        sys("linux"),
        2,
        &|url| flaky.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap_err();
    // Both are tried four times, and neither hides the other; the rest of the pass still ran.
    assert_eq!(flaky.times(&common), 4);
    assert_eq!(flaky.times(&linux), 4);
    assert!(
        error.starts_with("2 files failed after 4 tries:"),
        "{error}"
    );
    assert!(
        error.contains(&format!("{common} timed out (ask 4)")),
        "{error}"
    );
    assert!(
        error.contains(&format!("{linux} timed out (ask 4)")),
        "{error}"
    );
    assert!(dir
        .join("libraries/org/fake/not-osx/1/not-osx-1.jar")
        .is_file());
    // The game's files are all there is to a failed download: no sounds were tried.
    assert!(!dir.join("assets/indexes").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_long_list_of_failures_is_cut_short() {
    let failed: Vec<String> = (1..=11)
        .map(|n| format!("https://example.test/{n} answered 404"))
        .collect();
    let text = describe(&failed);
    assert!(
        text.starts_with("11 files failed after 4 tries:\n    https://example.test/1 answered 404")
    );
    assert!(text.contains("https://example.test/8 answered 404"));
    assert!(!text.contains("https://example.test/9 "));
    assert!(text.ends_with("\n    … and 3 more"), "{text}");
    // One failure is just its line.
    assert_eq!(
        describe(&failed[..1]),
        "https://example.test/1 answered 404"
    );
}

#[test]
fn a_cancelled_download_is_never_retried() {
    let (pins, files) = mojang();
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("cancel-retry");
    let cancel = AtomicBool::new(false);
    // The jar fails, and Cancel is pressed meanwhile: it is not asked for again.
    let fetch = |url: &str| {
        if url.ends_with("client.jar") {
            cancel.store(true, Relaxed);
            return Err("timed out".to_string());
        }
        server.fetch(url)
    };
    let error = download(&pins, &dir, sys("linux"), 1, &fetch, &cancel, &|_| {}).unwrap_err();
    assert_eq!(error, CANCELLED);
    let asked = server.asked();
    assert!(
        asked.iter().all(|u| !u.ends_with("client.jar")),
        "{asked:#?}"
    );
    assert_eq!(
        asked.len(),
        1,
        "only the version file was fetched: {asked:#?}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_download_goes_on_past_an_asset_that_keeps_failing() {
    let (pins, mut files) = mojang();
    // The sound is gone from the server; the language still comes.
    let hash = sha1_hex(SOUND);
    files.remove(&format!("{OBJECTS}/{}/{hash}", &hash[..2]));
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("tolerant");
    let logged = Mutex::new(Vec::new());
    download(
        &pins,
        &dir,
        sys("linux"),
        1,
        &|url| server.fetch(url),
        &GO,
        &|line| logged.lock().unwrap().push(line),
    )
    .unwrap();
    let lang = sha1_hex(LANG);
    assert!(dir
        .join("assets/objects")
        .join(&lang[..2])
        .join(&lang)
        .is_file());
    let sound = server.asked().iter().filter(|u| u.ends_with(&hash)).count();
    assert_eq!(sound, 4, "tried four times");
    let last = logged.lock().unwrap().last().cloned().unwrap();
    assert!(
        last.starts_with("The sounds could not all be downloaded: ")
            && last.ends_with(&format!("{OBJECTS}/{}/{hash} answered 404", &hash[..2])),
        "{last}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_asset_of_the_listed_size_is_kept_unread_and_another_size_is_fetched_again() {
    let (pins, files) = mojang();
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("by-size");
    let object = |data: &[u8]| {
        let hash = sha1_hex(data);
        (
            hash.clone(),
            dir.join("assets/objects").join(&hash[..2]).join(&hash),
        )
    };
    // The sound is there at its listed size, whatever its bytes; the language is cut short.
    let (sound_hash, sound) = object(SOUND);
    let (lang_hash, lang) = object(LANG);
    fs::create_dir_all(sound.parent().unwrap()).unwrap();
    fs::create_dir_all(lang.parent().unwrap()).unwrap();
    let same_size = vec![b'x'; SOUND.len()];
    fs::write(&sound, &same_size).unwrap();
    fs::write(&lang, b"{").unwrap();
    download(
        &pins,
        &dir,
        sys("linux"),
        2,
        &|url| server.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap();
    assert_eq!(fs::read(&sound).unwrap(), same_size);
    assert_eq!(fs::read(&lang).unwrap(), LANG);
    let asked = server.asked();
    assert!(
        !asked.iter().any(|u| u.ends_with(&sound_hash)),
        "{asked:#?}"
    );
    assert_eq!(asked.iter().filter(|u| u.ends_with(&lang_hash)).count(), 1);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_complete_download_needs_no_network() {
    let (pins, files) = mojang();
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("offline");
    download(
        &pins,
        &dir,
        sys("linux"),
        2,
        &|url| server.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap();
    let logged = Mutex::new(Vec::new());
    let offline = |_: &str| Err::<Vec<u8>, String>("no network".to_string());
    download(&pins, &dir, sys("linux"), 2, &offline, &GO, &|line| {
        logged.lock().unwrap().push(line)
    })
    .unwrap();
    let logged = logged.lock().unwrap();
    assert!(
        logged
            .iter()
            .all(|l| !l.contains("fail") && !l.contains("could not")),
        "{logged:#?}"
    );
    drop(logged);
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_asset_list_that_cannot_be_fetched_is_replaced_by_the_one_already_there() {
    let (pins, files) = mojang();
    let server = Server {
        files: files.clone(),
        asked: Mutex::new(Vec::new()),
    };
    let dir = scratch("old-index");
    download(
        &pins,
        &dir,
        sys("linux"),
        2,
        &|url| server.fetch(url),
        &GO,
        &|_| {},
    )
    .unwrap();
    // The list on disk is not the one Mojang lists any more (another sha1), and Mojang's cannot
    // be fetched: the old one still tells which sounds to look for.
    let lang = sha1_hex(LANG);
    let kept = serde_json::json!({"objects": {
        "minecraft/lang/fr_fr.json": {"hash": lang, "size": LANG.len()},
    }});
    fs::write(dir.join("assets/indexes/34.json"), kept.to_string()).unwrap();
    let mut files = files;
    files.remove("https://piston-meta.mojang.com/v1/packages/i/34.json");
    let server = Server {
        files,
        asked: Mutex::new(Vec::new()),
    };
    let logged = Mutex::new(Vec::new());
    download(
        &pins,
        &dir,
        sys("linux"),
        2,
        &|url| server.fetch(url),
        &GO,
        &|line| logged.lock().unwrap().push(line),
    )
    .unwrap();
    let logged = logged.lock().unwrap();
    assert!(
        logged.iter().any(|l| l.starts_with(
            "The asset list could not be downloaded, so the one already here is used: "
        ) && l.ends_with("34.json answered 404")),
        "{logged:#?}"
    );
    assert!(
        logged
            .iter()
            .any(|l| l == "Downloading the sounds (1 files)"),
        "{logged:#?}"
    );
    assert!(
        !logged.iter().any(|l| l.starts_with("The sounds could not")),
        "{logged:#?}"
    );
    drop(logged);
    fs::remove_dir_all(&dir).unwrap();

    // With no list at all, the sounds are what is left out: a line, not an error.
    let dir = scratch("no-index");
    let logged = Mutex::new(Vec::new());
    download(
        &pins,
        &dir,
        sys("linux"),
        2,
        &|url| server.fetch(url),
        &GO,
        &|line| logged.lock().unwrap().push(line),
    )
    .unwrap();
    let last = logged.lock().unwrap().last().cloned().unwrap();
    assert!(
        last.starts_with("The sounds could not all be downloaded: "),
        "{last}"
    );
    assert!(dir.join("versions/26.3/26.3.jar").is_file());
    fs::remove_dir_all(&dir).unwrap();
}

fn on(os: &'static str, arch: Arch) -> System<'static> {
    System { os, arch }
}

/// Names of the libraries a version file's `libraries` leaves active on `system`.
fn active_on<'a>(libraries: &'a Value, system: System<'_>) -> Vec<&'a str> {
    libraries
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| active(l, system))
        .map(|l| l["name"].as_str().unwrap())
        .collect()
}

#[test]
fn rules_choose_libraries_by_operating_system_and_processor() {
    let libraries = serde_json::json!([
        {"name": "plain"},
        {"name": "linux-any", "rules": [{"action": "allow", "os": {"name": "linux"}}]},
        {"name": "linux-arm64", "rules": [{"action": "allow", "os": {"name": "linux", "arch": "arm64"}}]},
        {"name": "linux-aarch64", "rules": [{"action": "allow", "os": {"name": "linux", "arch": "aarch64"}}]},
        {"name": "windows-x86", "rules": [{"action": "allow", "os": {"name": "windows", "arch": "x86"}}]},
        {"name": "osx-not-arm64", "rules": [
            {"action": "allow", "os": {"name": "osx"}},
            {"action": "disallow", "os": {"name": "osx", "arch": "arm64"}},
        ]},
        {"name": "amd64", "rules": [{"action": "allow", "os": {"arch": "amd64"}}]},
        {"name": "no-rules-at-all", "rules": []},
    ]);
    // Written out by hand, not derived from the helpers under test.
    let cases = [
        (
            on("linux", Arch::X86_64),
            vec!["plain", "linux-any", "amd64", "no-rules-at-all"],
        ),
        (
            on("linux", Arch::Arm64),
            vec![
                "plain",
                "linux-any",
                "linux-arm64",
                "linux-aarch64",
                "no-rules-at-all",
            ],
        ),
        (
            on("linux", Arch::X86),
            vec!["plain", "linux-any", "no-rules-at-all"],
        ),
        (
            on("windows", Arch::X86),
            vec!["plain", "windows-x86", "no-rules-at-all"],
        ),
        (
            on("windows", Arch::X86_64),
            vec!["plain", "amd64", "no-rules-at-all"],
        ),
        (
            on("osx", Arch::X86_64),
            vec!["plain", "osx-not-arm64", "amd64", "no-rules-at-all"],
        ),
        (on("osx", Arch::Arm64), vec!["plain", "no-rules-at-all"]),
    ];
    for (system, expected) in cases {
        assert_eq!(active_on(&libraries, system), expected, "{system:?}");
    }
}

#[test]
fn natives_follow_the_processor_where_the_version_names_one() {
    let library = serde_json::json!({
        "name": "org.fake:platform:1",
        "natives": {
            "linux": "natives-linux",
            "linux-arm64": "natives-linux-arm64",
            "windows": "natives-windows-${arch}",
            "osx": "natives-osx",
        },
    });
    let classifier = |system: System| native_classifier(&library, system);
    assert_eq!(
        classifier(on("linux", Arch::X86_64)).as_deref(),
        Some("natives-linux")
    );
    assert_eq!(
        classifier(on("linux", Arch::X86)).as_deref(),
        Some("natives-linux")
    );
    assert_eq!(
        classifier(on("linux", Arch::Arm64)).as_deref(),
        Some("natives-linux-arm64")
    );
    assert_eq!(
        classifier(on("windows", Arch::X86)).as_deref(),
        Some("natives-windows-32")
    );
    assert_eq!(
        classifier(on("windows", Arch::X86_64)).as_deref(),
        Some("natives-windows-64")
    );
    // Mojang's plain entries are x86's: an ARM system without an entry of its own has none.
    assert_eq!(classifier(on("windows", Arch::Arm64)), None);
    assert_eq!(classifier(on("osx", Arch::Arm64)), None);
    assert_eq!(
        classifier(on("osx", Arch::X86_64)).as_deref(),
        Some("natives-osx")
    );
    // No natives map, no classifier.
    assert_eq!(
        native_classifier(
            &serde_json::json!({"name": "a:b:1"}),
            on("linux", Arch::X86_64)
        ),
        None
    );
}

#[test]
fn a_library_with_natives_for_other_systems_keeps_its_jar() {
    // As Mojang's text2speech for 1.12.2: a jar for every system, natives for two of them.
    let library = serde_json::json!({
        "name": "com.mojang:text2speech:1.10.3",
        "natives": {"linux": "natives-linux", "windows": "natives-windows"},
        "downloads": {"artifact": {"path": "t.jar"}},
    });
    let mac = on("osx", Arch::X86_64);
    assert!(active(&library, mac));
    assert_eq!(native_classifier(&library, mac), None);
}

/// Since 1.19 a system's natives are libraries of their own (`natives-linux-arm64`), chosen by
/// the rules' operating system and processor: a download takes only the ones that apply.
#[test]
fn a_download_takes_the_natives_of_this_systems_processor() {
    let native = |name: &str, data: &[u8]| {
        serde_json::json!({
            "path": format!("org/fake/{name}/1/{name}-1.jar"),
            "sha1": sha1_hex(data),
            "url": library_url(name),
        })
    };
    let index = serde_json::json!({"objects": {}}).to_string();
    let manifest = serde_json::json!({
        "id": "1.21",
        "assetIndex": {"id": "17", "sha1": sha1_hex(index.as_bytes()),
                       "url": "https://piston-meta.mojang.com/v1/packages/i/17.json"},
        "downloads": {"client": {"sha1": sha1_hex(CLIENT), "size": CLIENT.len(), "url": "https://piston-data.mojang.com/c.jar"}},
        "libraries": [
            {"name": "org.fake:core:1", "downloads": {"artifact": native("core", COMMON)}},
            {"name": "org.fake:natives-linux:1", "downloads": {"artifact": native("natives-linux", b"x64")},
             "rules": [{"action": "allow", "os": {"name": "linux"}}, {"action": "disallow", "os": {"name": "linux", "arch": "arm64"}}]},
            {"name": "org.fake:natives-linux-arm64:1", "downloads": {"artifact": native("natives-linux-arm64", b"arm")},
             "rules": [{"action": "allow", "os": {"name": "linux", "arch": "arm64"}}]},
            {"name": "org.fake:natives-windows-x86:1", "downloads": {"artifact": native("natives-windows-x86", b"x86")},
             "rules": [{"action": "allow", "os": {"name": "windows", "arch": "x86"}}]},
        ],
    })
    .to_string();
    let sha1 = sha1_hex(manifest.as_bytes());
    let server = Server {
        files: HashMap::from([
            (
                format!("https://piston-meta.mojang.com/v1/packages/{sha1}/1.21.json"),
                manifest.into_bytes(),
            ),
            (
                "https://piston-data.mojang.com/c.jar".into(),
                CLIENT.to_vec(),
            ),
            (library_url("core"), COMMON.to_vec()),
            (library_url("natives-linux"), b"x64".to_vec()),
            (library_url("natives-linux-arm64"), b"arm".to_vec()),
            (library_url("natives-windows-x86"), b"x86".to_vec()),
            (
                "https://piston-meta.mojang.com/v1/packages/i/17.json".into(),
                index.into_bytes(),
            ),
        ]),
        asked: Mutex::new(Vec::new()),
    };
    let pins = Pins::for_version("1.21", &sha1);
    let fetched = |system: System| {
        let dir = scratch(&format!("arch-{}-{}", system.os, system.arch.name()));
        download(
            &pins,
            &dir,
            system,
            2,
            &|url| server.fetch(url),
            &GO,
            &|_| {},
        )
        .unwrap();
        let mut names: Vec<String> = fs::read_dir(dir.join("libraries/org/fake"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        fs::remove_dir_all(&dir).unwrap();
        names
    };
    assert_eq!(
        fetched(on("linux", Arch::X86_64)),
        ["core", "natives-linux"]
    );
    assert_eq!(
        fetched(on("linux", Arch::Arm64)),
        ["core", "natives-linux-arm64"]
    );
    assert_eq!(
        fetched(on("windows", Arch::X86)),
        ["core", "natives-windows-x86"]
    );
    assert_eq!(fetched(on("windows", Arch::X86_64)), ["core"]);
}
