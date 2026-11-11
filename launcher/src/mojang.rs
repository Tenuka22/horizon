//! The player's own download of the pinned version from Mojang's public servers, as other
//! launchers get it: the version JSON, the client jar, this system's libraries and the assets
//! (the sounds), each checked against its sha1, in the official launcher's layout in the
//! launcher's data folder. No Minecraft account takes part: the files are public, so an
//! Ely.by player gets them as anyone does. sources.rs then finds them there like any
//! launcher's. Nothing of Mojang's is part of Horizon.
//!
//! Each file is checked against the size and sha1 Mojang lists, and written whole. A file
//! that fails is tried again before the download gives up. A download stopped half way
//! (cancelled, no network) keeps the files it finished: the next one fetches only the rest.

use crate::sources::{fetch_checked, fetch_manifest, sha1_file, sha1_hex, write_file, Fetch, Pins};
use serde_json::Value;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Duration;

const OBJECTS: &str = "https://resources.download.minecraft.net";
/// The index of Mojang's Java runtimes, the one the official launcher reads.
const JAVA_RUNTIMES: &str = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";

/// Set to stop the download under way (the window's Cancel); cleared when a preparation starts.
pub static CANCEL: AtomicBool = AtomicBool::new(false);
/// The download under way: files done, of how many (0 when none runs).
static DONE: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

/// How far the download under way is, files done of how many; none when none runs.
pub fn progress() -> Option<(usize, usize)> {
    let total = TOTAL.load(Relaxed);
    (total > 0).then(|| (DONE.load(Relaxed).min(total), total))
}

const CANCELLED: &str = "Download cancelled. Download again to go on where it stopped.";

/// One file to download, with the size Mojang lists for it when it lists one.
struct File {
    url: String,
    sha1: String,
    size: Option<u64>,
    path: PathBuf,
    /// Kept when its size is the listed one rather than its sha1 (the assets: each object is
    /// written whole or not at all, and there are thousands).
    by_size: bool,
}

/// Downloads the pinned version into `dir`, `jobs` files at a time; files already there and
/// whole are kept, and with all of them there nothing is fetched at all. A file that fails is tried
/// again (`TRIES` in all) before the download fails, naming every file that did. The asset
/// list that cannot be fetched is replaced by the one already there; an asset that fails is a
/// line in the log (the game is then partly silent), anything else an error. `cancel` stops it
/// between files.
pub fn download(
    pins: &Pins,
    dir: &Path,
    system: System,
    jobs: usize,
    fetch: Fetch,
    cancel: &AtomicBool,
    log: &dyn Fn(String),
) -> Result<(), String> {
    let v = &pins.version;
    let versions = dir.join("versions").join(v);
    let manifest = versions.join(format!("{v}.json"));
    fetch_manifest(pins, &manifest, fetch, log)?;
    let manifest = read_json(&manifest)?;
    let client = &manifest["downloads"]["client"];
    // A version Horizon doesn't pin takes its client's sha1 from its version file, itself
    // checked against the sha1 Mojang's version list gives (`Pins::for_version`).
    let client_sha1 = match pins.client_sha1.as_str() {
        "" => text(client, "sha1")?,
        pinned => pinned.to_string(),
    };
    let mut game = vec![File {
        url: text(client, "url")?,
        sha1: client_sha1,
        size: client["size"].as_u64(),
        path: versions.join(format!("{v}.jar")),
        by_size: false,
    }];
    let libraries = manifest["libraries"]
        .as_array()
        .ok_or("The version file has no libraries")?;
    for library in libraries.iter().filter(|l| active(l, system)) {
        // Before 1.19 a library's natives for this system are a classifier of its own.
        let native = native_classifier(library, system)
            .map(|key| &library["downloads"]["classifiers"][key.as_str()]);
        for artifact in [Some(&library["downloads"]["artifact"]), native]
            .into_iter()
            .flatten()
            .filter(|a| !a.is_null())
        {
            let path = dir
                .join("libraries")
                .join(relative(&text(artifact, "path")?)?);
            // Libraries listed twice are fetched once: two writers would share a partial file.
            if game.iter().any(|f| f.path == path) {
                continue;
            }
            game.push(File {
                url: text(artifact, "url")?,
                sha1: text(artifact, "sha1")?,
                size: artifact["size"].as_u64(),
                path,
                by_size: false,
            });
        }
    }
    let libraries = game.len() - 1;
    // The logging setup the game is started with (`logging.client.argument`): for 1.7 to 1.18,
    // Mojang's fixed one is what keeps Log4Shell out.
    let logging = &manifest["logging"]["client"]["file"];
    if !logging.is_null() {
        game.push(File {
            url: text(logging, "url")?,
            sha1: text(logging, "sha1")?,
            size: logging["size"].as_u64(),
            path: dir
                .join("assets/log_configs")
                .join(relative(&text(logging, "id")?)?),
            by_size: false,
        });
    }
    log(format!(
        "Downloading Minecraft {v} from Mojang: the game and {libraries} libraries"
    ));
    fetch_all(&game, jobs, fetch, cancel, log).map_err(Stop::message)?;
    let index = &manifest["assetIndex"];
    let index = File {
        url: text(index, "url")?,
        sha1: text(index, "sha1")?,
        size: index["size"].as_u64(),
        path: dir
            .join("assets/indexes")
            .join(relative(&format!("{}.json", text(index, "id")?))?),
        by_size: false,
    };
    match sounds(&index, dir, jobs, fetch, cancel, log) {
        Err(Stop::Cancelled) => Err(CANCELLED.to_string()),
        Err(Stop::Failed(failed)) => {
            log(format!(
                "The sounds could not all be downloaded: {}",
                describe(&failed)
            ));
            Ok(())
        }
        Ok(()) => Ok(()),
    }
}

/// The assets: the list `index` names, which stays usable from an earlier download when it can
/// no longer be fetched, then each sound and other object it lists.
fn sounds(
    index: &File,
    dir: &Path,
    jobs: usize,
    fetch: Fetch,
    cancel: &AtomicBool,
    log: &dyn Fn(String),
) -> Result<(), Stop> {
    match fetch_all(std::slice::from_ref(index), 1, fetch, cancel, log) {
        Err(Stop::Failed(failed)) if index.path.is_file() => log(format!(
            "The asset list could not be downloaded, so the one already here is used: {}",
            describe(&failed)
        )),
        other => other?,
    }
    let list = read_json(&index.path).map_err(|e| Stop::Failed(vec![e]))?;
    let objects = objects(&list, dir).map_err(|e| Stop::Failed(vec![e]))?;
    log(format!("Downloading the sounds ({} files)", objects.len()));
    fetch_all(&objects, jobs, fetch, cancel, log)
}

/// Mojang's name for this system in its Java runtime index.
pub(crate) fn java_platform() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "mac-os-arm64",
        ("macos", "x86_64") => "mac-os",
        ("linux", "x86_64") => "linux",
        ("linux", "x86") => "linux-i386",
        ("windows", "x86_64") => "windows-x64",
        ("windows", "aarch64") => "windows-arm64",
        ("windows", "x86") => "windows-x86",
        _ => return None,
    })
}

/// Downloads Mojang's Java runtime `component` (the version JSON's `javaVersion.component`, e.g.
/// `java-runtime-epsilon`) into the game directory's `runtime` folder, as the official launcher
/// lays it out: `<component>/<platform>/<component>/`. Each file is checked against its sha1;
/// files already there and whole are kept. These also include javac for older release preparers;
/// current releases can use an already installed runtime without it.
pub fn java_runtime(
    component: &str,
    runtime: &Path,
    jobs: usize,
    fetch: Fetch,
    cancel: &AtomicBool,
    log: &dyn Fn(String),
) -> Result<PathBuf, String> {
    let platform = java_platform().ok_or("Mojang has no Java for this system")?;
    let index: Value = serde_json::from_slice(&fetch(JAVA_RUNTIMES)?)
        .map_err(|e| format!("Mojang's Java index: {e}"))?;
    let entry = &index[platform][component][0];
    if entry.is_null() {
        return Err(format!("Mojang has no {component} for {platform}"));
    }
    let manifest = &entry["manifest"];
    let manifest: Value = serde_json::from_slice(&fetch_checked(
        fetch,
        &text(manifest, "url")?,
        &text(manifest, "sha1")?,
    )?)
    .map_err(|e| format!("Mojang's {component} manifest: {e}"))?;
    let files = manifest["files"]
        .as_object()
        .ok_or_else(|| format!("Mojang's {component} manifest lists no files"))?;
    let home = runtime.join(component).join(platform).join(component);
    let mut downloads = Vec::new();
    let mut executables = Vec::new();
    let mut links = Vec::new();
    for (name, file) in files {
        let path = home.join(relative(name)?);
        match file["type"].as_str() {
            Some("file") => {
                let raw = &file["downloads"]["raw"];
                if file["executable"] == true {
                    executables.push(path.clone());
                }
                downloads.push(File {
                    url: text(raw, "url")?,
                    sha1: text(raw, "sha1")?,
                    size: raw["size"].as_u64(),
                    path,
                    by_size: false,
                });
            }
            Some("link") => links.push((path, text(file, "target")?)),
            _ => {}
        }
    }
    let name = entry["version"]["name"].as_str().unwrap_or(component);
    log(format!(
        "Downloading Minecraft's Java {name} from Mojang ({} files)",
        downloads.len()
    ));
    fetch_all(&downloads, jobs, fetch, cancel, log).map_err(Stop::message)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for path in &executables {
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
        for (path, target) in &links {
            if !link_stays_inside(name_in(&home, path), target) {
                return Err(format!(
                    "Unsafe link {target:?} in Mojang's {component} manifest"
                ));
            }
            let _ = fs::remove_file(path);
            fs::create_dir_all(path.parent().unwrap())
                .and_then(|_| std::os::unix::fs::symlink(target, path))
                .map_err(|e| format!("{}: {e}", path.display()))?;
        }
    }
    #[cfg(not(unix))]
    let _ = (executables, links);
    Ok(home)
}

// Used by the unix link step below; the path-traversal check itself is tested on every
// platform, so these stay compiled and are allowed to look unused elsewhere.
#[cfg_attr(not(unix), allow(dead_code))]
fn name_in<'a>(home: &Path, path: &'a Path) -> &'a Path {
    path.strip_prefix(home).unwrap_or(path)
}

/// Whether a link at `link` (relative to the runtime's home) pointing at `target` stays in the
/// home.
#[cfg_attr(not(unix), allow(dead_code))]
fn link_stays_inside(link: &Path, target: &str) -> bool {
    let mut depth = link.components().count().saturating_sub(1);
    for c in Path::new(target).components() {
        match c {
            Component::Normal(_) => depth += 1,
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => depth -= 1,
            _ => return false,
        }
    }
    true
}

fn read_json(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))
}

fn text(v: &Value, key: &str) -> Result<String, String> {
    v[key]
        .as_str()
        .map(String::from)
        .ok_or_else(|| format!("The version file has no {key} in {v}"))
}

/// A path from the version file, refused if it could leave the folder.
pub(crate) fn relative(path: &str) -> Result<PathBuf, String> {
    let safe = !path.contains('\\')
        && Path::new(path)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
    safe.then(|| PathBuf::from(path))
        .ok_or_else(|| format!("Unsafe path {path:?} in the version file"))
}

/// A processor family, as a rule's `os.arch` names it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    /// 32-bit x86 (`x86`).
    X86,
    /// 64-bit x86 (`x86_64`, `amd64`).
    X86_64,
    /// 64-bit ARM (`arm64`, `aarch64`).
    Arm64,
}

impl Arch {
    /// This build's own.
    pub fn current() -> Arch {
        if cfg!(target_arch = "aarch64") {
            Arch::Arm64
        } else if cfg!(target_arch = "x86") {
            Arch::X86
        } else {
            Arch::X86_64
        }
    }

    /// The word a classifier ends with for this architecture (`natives-linux-arm64`).
    fn name(self) -> &'static str {
        match self {
            Arch::X86 => "x86",
            Arch::X86_64 => "x86_64",
            Arch::Arm64 => "arm64",
        }
    }

    /// Whether a rule's `os.arch` names this one.
    fn named(self, arch: &str) -> bool {
        match arch {
            "x86" => self == Arch::X86,
            "x86_64" | "amd64" => self == Arch::X86_64,
            "arm64" | "aarch64" => self == Arch::Arm64,
            _ => false,
        }
    }

    /// `${arch}` in an old classifier (`natives-windows-${arch}`): the word size.
    fn bits(self) -> &'static str {
        match self {
            Arch::X86 => "32",
            Arch::X86_64 | Arch::Arm64 => "64",
        }
    }
}

/// The system a game runs on, as version files name it; every file and rule choice depends
/// on it, so what downloads, what is on the classpath and what unpacks agree.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct System<'a> {
    /// `osx`, `linux` or `windows`.
    pub os: &'a str,
    /// The processor of the Java that runs the game (not always the computer's own).
    pub arch: Arch,
}

impl System<'static> {
    /// This computer.
    pub fn current() -> Self {
        System {
            os: crate::java_launch::os_name(),
            arch: Arch::current(),
        }
    }
}

/// Whether a rule list lets `system` in: the last rule that matches decides; no rules allow.
/// A rule matches by its `os.name`, `os.arch` and `features`; the optional features (demo,
/// custom resolution, quick play) are on only when named in `on`.
pub(crate) fn rules_allow(rules: &Value, system: System, on: &[&str]) -> bool {
    let Some(rules) = rules.as_array().filter(|rules| !rules.is_empty()) else {
        return true;
    };
    let mut allow = false;
    for rule in rules {
        let os = &rule["os"];
        let matches = os["name"].as_str().is_none_or(|name| name == system.os)
            && os["arch"]
                .as_str()
                .is_none_or(|arch| system.arch.named(arch))
            && rule["features"].as_object().is_none_or(|features| {
                features
                    .iter()
                    .all(|(name, v)| v.as_bool() == Some(on.contains(&name.as_str())))
            });
        if matches {
            allow = rule["action"] == "allow";
        }
    }
    allow
}

/// Whether a library's rules let `system` use it. A library with natives may still have none for
/// it (`native_classifier`) and keeps its jar: Mojang's own versions list macOS-less
/// `text2speech` natives next to a jar every system's game loads.
pub(crate) fn active(library: &Value, system: System) -> bool {
    rules_allow(&library["rules"], system, &[])
}

/// The classifier holding a library's natives for `system`, from its `natives` map, as PolyMC
/// reads one: the entry for this operating system and processor (`osx-arm64`) first, then
/// the plain operating system's (`osx`, `linux`, `windows`), which only an x86 system takes,
/// since Mojang wrote those for it. `${arch}` is this system's word size, as old versions name
/// Windows' natives. Newer versions list each system's natives as libraries of their own
/// (`natives-linux-arm64`), chosen by `active`'s rules.
pub(crate) fn native_classifier(library: &Value, system: System) -> Option<String> {
    let natives = library["natives"].as_object()?;
    let precise = format!("{}-{}", system.os, system.arch.name());
    let plain = (system.arch != Arch::Arm64).then_some(system.os);
    let key = natives
        .get(&precise)
        .or_else(|| natives.get(plain?))?
        .as_str()?;
    Some(key.replace("${arch}", system.arch.bits()))
}

/// The asset index's objects, once each.
fn objects(index: &Value, dir: &Path) -> Result<Vec<File>, String> {
    let objects = index["objects"]
        .as_object()
        .ok_or("The asset index has no objects")?;
    let sizes: std::collections::HashMap<&str, u64> = objects
        .values()
        .filter_map(|o| Some((o["hash"].as_str()?, o["size"].as_u64()?)))
        .collect();
    let mut hashes: Vec<&str> = objects
        .values()
        .filter_map(|o| o["hash"].as_str())
        .collect();
    hashes.sort_unstable();
    hashes.dedup();
    hashes
        .into_iter()
        .map(|hash| {
            if hash.len() != 40 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("Bad asset hash {hash:?} in the asset index"));
            }
            let path = format!("{}/{hash}", &hash[..2]);
            Ok(File {
                url: format!("{OBJECTS}/{path}"),
                sha1: hash.to_string(),
                size: sizes.get(hash).copied(),
                path: dir.join("assets/objects").join(path),
                by_size: true,
            })
        })
        .collect()
}

/// One file from Mojang, refused unless it has the size and sha1 Mojang lists.
fn fetch_file(fetch: Fetch, file: &File) -> Result<Vec<u8>, String> {
    let body = fetch(&file.url)?;
    if let Some(size) = file.size.filter(|&size| size != body.len() as u64) {
        return Err(format!(
            "is {} bytes, not the {size} Mojang lists",
            body.len()
        ));
    }
    match sha1_hex(&body) {
        actual if actual == file.sha1 => Ok(body),
        actual => Err(format!("is not the one Horizon pins (sha1 {actual})")),
    }
}

/// The most times a file is tried: the first pass and three more, as PolyMC's `NetJob` does.
const TRIES: usize = 4;

/// How many failed files a message names before it says `… and N more`.
const NAMED: usize = 8;

/// Why `fetch_all` stopped.
enum Stop {
    /// `cancel` was set: nothing is retried.
    Cancelled,
    /// What still failed after every try, one `url why` line per file.
    Failed(Vec<String>),
}

impl Stop {
    fn message(self) -> String {
        match self {
            Stop::Cancelled => CANCELLED.to_string(),
            Stop::Failed(failed) => describe(&failed),
        }
    }
}

/// The failed files as one message: the only one's line, or the first few of them.
fn describe(failed: &[String]) -> String {
    let [only] = failed else {
        let mut text = format!("{} files failed after {TRIES} tries:", failed.len());
        for line in failed.iter().take(NAMED) {
            text += &format!("\n    {line}");
        }
        if failed.len() > NAMED {
            text += &format!("\n    … and {} more", failed.len() - NAMED);
        }
        return text;
    };
    only.clone()
}

/// Whether `file` is there and whole: an asset (`by_size`) by its size, as every write is
/// atomic, anything else by its sha1.
fn present(file: &File) -> bool {
    match file.size.filter(|_| file.by_size) {
        Some(size) => fs::metadata(&file.path).is_ok_and(|m| m.is_file() && m.len() == size),
        None => sha1_file(&file.path).as_ref() == Some(&file.sha1),
    }
}

/// Downloads the files not already there, `jobs` at a time, with a line every second. Files that
/// fail are tried again, up to `TRIES` times in all, before the download fails with every one
/// of them; `cancel` stops it between files, at once and for good.
fn fetch_all(
    files: &[File],
    jobs: usize,
    fetch: Fetch,
    cancel: &AtomicBool,
    log: &dyn Fn(String),
) -> Result<(), Stop> {
    let done = AtomicUsize::new(0);
    DONE.store(0, Relaxed);
    TOTAL.store(files.len(), Relaxed);
    let mut pending: Vec<&File> = files.iter().collect();
    let mut round = 1;
    let result = loop {
        let (failed, stopped) = pass(&pending, jobs, fetch, cancel, log, &done);
        if stopped {
            break Err(Stop::Cancelled);
        }
        if failed.is_empty() {
            break Ok(());
        }
        if round == TRIES {
            break Err(Stop::Failed(failed.into_iter().map(|(_, e)| e).collect()));
        }
        round += 1;
        log(format!(
            "{} files failed; trying them again (try {round} of {TRIES})",
            failed.len()
        ));
        pending = failed.iter().map(|(i, _)| pending[*i]).collect();
    };
    TOTAL.store(0, Relaxed);
    result
}

/// One pass over `files`: the ones that failed, as their place in `files` and why, in order, and
/// whether `cancel` stopped it before the end.
fn pass(
    files: &[&File],
    jobs: usize,
    fetch: Fetch,
    cancel: &AtomicBool,
    log: &dyn Fn(String),
    done: &AtomicUsize,
) -> (Vec<(usize, String)>, bool) {
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let failed = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..jobs.clamp(1, files.len().max(1)))
            .map(|_| {
                scope.spawn(|| loop {
                    let i = next.fetch_add(1, Relaxed);
                    let Some(&file) = files.get(i) else {
                        break;
                    };
                    if cancel.load(Relaxed) {
                        stopped.store(true, Relaxed);
                        break;
                    }
                    if !present(file) {
                        let got = fetch_file(fetch, file)
                            .map_err(|e| format!("{} {e}", file.url))
                            .and_then(|body| write_file(&file.path, &body));
                        if let Err(e) = got {
                            failed.lock().unwrap().push((i, e));
                            continue;
                        }
                    }
                    DONE.store(done.fetch_add(1, Relaxed) + 1, Relaxed);
                })
            })
            .collect();
        let mut waited = 0;
        while !workers.iter().all(|w| w.is_finished()) {
            std::thread::sleep(Duration::from_millis(100));
            waited += 1;
            if waited % 10 == 0 {
                log(format!(
                    "{} of {} files",
                    done.load(Relaxed),
                    TOTAL.load(Relaxed)
                ));
            }
        }
    });
    let mut failed = failed.into_inner().unwrap();
    failed.sort_unstable_by_key(|(i, _)| *i);
    (failed, stopped.into_inner())
}

#[cfg(test)]
#[path = "mojang_tests.rs"]
mod tests;
