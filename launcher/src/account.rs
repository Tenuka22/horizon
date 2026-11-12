//! Ely.by account sign-in (docs/elyby-account.md): the Ely.by username and password, the two-factor
//! code when the account asks for one, and the session the game plays with. Secrets (the access
//! token and the client token) live in the OS keychain, or in a file only the player can read where
//! there is none; accounts.json keeps only names and UUIDs. The game gets the session in a file it
//! reads once (`--session-file`), never on its command line. Tokens never reach logs, errors or
//! output.
//!
//! Play needs a signed-in account ([`launch`]), and the game plays as the selected one. An Ely.by
//! account doesn't have to own Minecraft: the game files come from Mojang's public download servers
//! (`mojang.rs`). The game binary itself, its harnesses and the servers don't go through the
//! launcher's gate.
pub mod elyby;
pub mod session;

use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The keychain service name every secret is stored under.
pub const SERVICE: &str = "dev.playhorizon.launcher";
/// What every Play says while no account is signed in.
pub const NEEDS_ACCOUNT: &str = "Sign in with an Ely.by account to play";
/// The access token is renewed when it has less than this left.
const RENEW_BEFORE: u64 = 3600;

/// Where the requests go: the real services, or a mock in tests.
#[derive(Clone, Debug)]
pub struct Endpoints {
    /// authserver.ely.by (`HORIZON_ELY_AUTH` overrides it).
    pub ely: String,
    /// skinsystem.ely.by (`HORIZON_ELY_SKINS` overrides it).
    pub skins: String,
}

impl Endpoints {
    pub fn live() -> Self {
        Self {
            ely: elyby::base(),
            skins: elyby::skins_base(),
        }
    }
}

/// A request to `url`: the launcher's user agent, a 20 s timeout, and the player's proxy when one
/// is set (`proxy`), since a sign-in has to get out the way downloads do.
fn request_to(method: &str, url: &str) -> ureq::Request {
    crate::proxy::agent(
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(20))
            .user_agent(&format!("horizon-launcher/{}", crate::backend::VERSION)),
        url,
    )
    .build()
    .request(method, url)
}

/// One request's result: the status and the JSON body (empty when there is none). Transport
/// errors name the service, never the URL's query or the request.
fn send(request: ureq::Request, body: Option<Body>, service: &str) -> Result<(u16, Value), String> {
    let result = match body {
        None => request.call(),
        Some(Body::Json(value)) => request.set("Accept", "application/json").send_json(value),
    };
    let response = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(ureq::Error::Transport(_)) => {
            return Err(format!(
                "Could not reach {service}. Check your internet connection."
            ));
        }
    };
    let status = response.status();
    let text = response.into_string().unwrap_or_default();
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

enum Body {
    Json(Value),
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// An Ely.by profile, as the launcher shows and remembers it (no secrets).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Account {
    /// The profile UUID without dashes.
    pub uuid: String,
    pub name: String,
    /// The skin's URL (Ely.by's skin system) and model ("classic" or "slim").
    #[serde(default)]
    pub skin: Option<(String, String)>,
    #[serde(default)]
    pub cape: Option<String>,
}

/// What the keychain keeps for one account: Ely.by's two tokens.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Secrets {
    /// The client token the access token was issued to: `/auth/refresh` needs both.
    refresh_token: String,
    #[serde(default)]
    sign_in_required: bool,
    /// The Ely.by access token: the session the game and the servers are given.
    minecraft_token: String,
    minecraft_expires_at: u64,
}
impl std::fmt::Debug for Secrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secrets(<hidden>)")
    }
}

/// What asking Ely.by again found for one account.
enum Check {
    /// The session holds: the profile as it is now (the name may have changed) and its tokens.
    Ready(Account, Secrets),
    /// No answer (no network, a sign-in that can't be renewed): the last tokens stand.
    NoAnswer(String, Secrets),
}

/// Checks `account`'s session, with the access token renewed through Ely.by first when it has less
/// than an hour left. The secrets come back renewed whatever the answer.
fn check(ep: &Endpoints, account: &Account, secrets: Secrets) -> Check {
    check_with(ep, account, secrets, false, &|_| {})
}

fn check_with(
    ep: &Endpoints,
    account: &Account,
    mut secrets: Secrets,
    force: bool,
    progress: &dyn Fn(session::Phase),
) -> Check {
    if secrets.sign_in_required {
        return Check::NoAnswer(elyby::EXPIRED.into(), secrets);
    }
    if !force && secrets.minecraft_expires_at > now() + RENEW_BEFORE {
        return Check::Ready(account.clone(), secrets);
    }
    progress(session::Phase::Elyby);
    match elyby::refresh(&ep.ely, &secrets.minecraft_token, &secrets.refresh_token) {
        Ok(t) if t.uuid != account.uuid => Check::NoAnswer(
            elyby::OTHER_PROFILE.into(),
            Secrets {
                sign_in_required: true,
                ..secrets
            },
        ),
        Ok(t) => {
            let renewed = elyby::with_textures(
                &ep.skins,
                Account {
                    name: t.name,
                    ..account.clone()
                },
            );
            Check::Ready(
                renewed,
                Secrets {
                    minecraft_token: t.access,
                    minecraft_expires_at: now() + elyby::LIFETIME,
                    ..secrets
                },
            )
        }
        Err(e) => {
            secrets.sign_in_required = e == elyby::EXPIRED;
            Check::NoAnswer(e, secrets)
        }
    }
}

// ----- The keychain --------------------------------------------------------------------------------

/// Where secrets are kept: the OS keychain, a file of the launcher's own when there is none, or
/// memory in tests.
pub trait SecretStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<String>, String>;
    fn set(&self, key: &str, secret: &str) -> Result<(), String>;
    fn delete(&self, key: &str) -> Result<(), String>;
    /// False for memory: secrets are gone when the launcher closes.
    fn persistent(&self) -> bool;
    /// A shared lock for read/renew/write operations; memory stores need only the process lock.
    fn lock_file(&self) -> Option<PathBuf> {
        None
    }
    /// The in-process half of that lock. The system keychain is one per computer, so every
    /// handle to it shares the process-wide lock; a memory store's secrets are its own, and so
    /// is its lock: two of them (one per unit test) never wait for each other.
    fn operations(&self) -> &Mutex<()> {
        session::process_lock()
    }
}

#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, String>>, Mutex<()>);
impl SecretStore for MemoryStore {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        Ok(self.0.lock().unwrap().get(key).cloned())
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        self.0.lock().unwrap().insert(key.into(), secret.into());
        Ok(())
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
    fn persistent(&self) -> bool {
        false
    }
    fn operations(&self) -> &Mutex<()> {
        &self.1
    }
}

/// How long a call to the system keyring may take before the launcher gives up on it (#12179). An
/// unlock prompt the player is typing a password into counts as an answer still coming, so this is
/// longer than a working keyring ever takes, short enough that Play says what is wrong.
pub const KEYCHAIN_WAIT: Duration = Duration::from_secs(20);

/// What the launcher says when the system keyring didn't answer within `wait`.
pub fn keyring_no_answer(wait: Duration) -> String {
    crate::i18n::trf(
        "Your system keyring didn't answer within {seconds} seconds. Make sure it is running and unlocked, then restart the launcher.",
        &[("seconds", &wait.as_secs().max(1).to_string())],
    )
}

/// One call to a [`Bounded`] keyring: its answer, or that the caller gave up on it.
struct Call<T> {
    answer: Option<T>,
    gave_up: bool,
}

/// A keyring whose calls give up after `wait` (#12179): a Secret Service that never answers (a
/// stuck KWallet or GNOME Keyring daemon) otherwise holds the account lock forever, and Play stays
/// on "Starting…" with nothing said. Each call runs on a thread of its own; one given up on keeps
/// running there, and while it does, later calls fail at once instead of piling up behind it.
pub struct Bounded {
    keyring: std::sync::Arc<dyn SecretStore>,
    wait: Duration,
    /// Calls given up on that haven't come back yet.
    stuck: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Bounded {
    pub fn new(keyring: impl SecretStore + 'static, wait: Duration) -> Self {
        Self {
            keyring: std::sync::Arc::new(keyring),
            wait,
            stuck: Default::default(),
        }
    }

    fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn SecretStore) -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        use std::sync::atomic::Ordering::SeqCst;
        let no_answer = || {
            let said = keyring_no_answer(self.wait);
            eprintln!("Horizon: {said}");
            Err(said)
        };
        if self.stuck.load(SeqCst) > 0 {
            return no_answer();
        }
        let call = std::sync::Arc::new((
            Mutex::new(Call::<Result<T, String>> {
                answer: None,
                gave_up: false,
            }),
            std::sync::Condvar::new(),
        ));
        let (keyring, stuck, theirs) = (self.keyring.clone(), self.stuck.clone(), call.clone());
        let spawned = std::thread::Builder::new()
            .name("keyring".into())
            .spawn(move || {
                let answer = f(keyring.as_ref());
                let (slot, done) = &*theirs;
                let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
                if slot.gave_up {
                    stuck.fetch_sub(1, SeqCst);
                } else {
                    slot.answer = Some(answer);
                    done.notify_one();
                }
            });
        if let Err(e) = spawned {
            return Err(e.to_string());
        }
        let (slot, done) = &*call;
        let slot = slot.lock().unwrap_or_else(|e| e.into_inner());
        let (mut slot, _) = done
            .wait_timeout_while(slot, self.wait, |call| call.answer.is_none())
            .unwrap_or_else(|e| e.into_inner());
        if let Some(answer) = slot.answer.take() {
            return answer;
        }
        // Under the call's lock: the thread, when it ends, sees this and counts itself back.
        slot.gave_up = true;
        self.stuck.fetch_add(1, SeqCst);
        drop(slot);
        no_answer()
    }
}

impl SecretStore for Bounded {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        let key = key.to_string();
        self.call(move |keyring| keyring.get(&key))
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        let (key, secret) = (key.to_string(), secret.to_string());
        self.call(move |keyring| keyring.set(&key, &secret))
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        let key = key.to_string();
        self.call(move |keyring| keyring.delete(&key))
    }
    fn persistent(&self) -> bool {
        self.keyring.persistent()
    }
    fn lock_file(&self) -> Option<PathBuf> {
        self.keyring.lock_file()
    }
    fn operations(&self) -> &Mutex<()> {
        self.keyring.operations()
    }
}

/// The secrets file in the launcher's data folder, when no keychain answers (#5095): many Linux
/// systems run no Secret Service, and sign-ins would otherwise last one run. One JSON object, key
/// to secret, encrypted with a key of this computer's ([`seal`]), readable and writable by the
/// player only (0600 on Unix), replaced whole on every write. A lock file beside it keeps each read-modify-replace whole across every launcher and CLI
/// process. The keychain takes its secrets back at the first start it answers ([`Layered`]).
pub struct FileStore {
    path: PathBuf,
}

/// The secrets file's name in the launcher's data folder.
pub const SECRETS_FILE: &str = "account-secrets.json";

impl FileStore {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// The launcher's own: [`SECRETS_FILE`] in its data folder.
    pub fn launcher() -> Self {
        Self::new(crate::backend::data_dir().join(SECRETS_FILE))
    }

    fn shown(&self, e: std::io::Error) -> String {
        format!("{}: {e}", self.path.display())
    }

    /// Runs `f` holding the file's lock (`account-secrets.lock`): other processes wait.
    fn locked<T>(&self, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        self.holding("lock", f)
    }

    /// Runs `f` holding the lock file `account-secrets.<name>`, made if need be.
    fn holding<T>(&self, name: &str, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| self.shown(e))?;
        }
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.path.with_extension(name))
            .map_err(|e| self.shown(e))?;
        lock.lock().map_err(|e| self.shown(e))?;
        // Closing the file releases the lock, after `f` whatever it answers.
        f()
    }

    /// Whether the file holds a sign-in: not only sign-out marks, and readable.
    fn holds_secrets(&self) -> bool {
        self.entries()
            .is_ok_and(|secrets| secrets.values().any(|secret| secret != SIGNED_OUT))
    }

    fn read(&self) -> Result<std::collections::BTreeMap<String, String>, String> {
        match std::fs::read(&self.path) {
            // Only where the file breaks: serde's own message can quote what the file holds.
            Ok(bytes) => {
                let (json, old) = self.unseal(&bytes)?.ok_or_else(|| {
                    format!(
                        "{} can't be read on this computer: it was written on another one, or changed. \
                         Remove it and sign in again.",
                        self.path.display()
                    )
                })?;
                let secrets = serde_json::from_slice(&json).map_err(|e| {
                    format!(
                        "{} can't be read ({:?} error at line {}, column {}).",
                        self.path.display(),
                        e.classify(),
                        e.line(),
                        e.column()
                    )
                })?;
                // Sealed under the old key: sealed again under this install's. Every read runs
                // under the lock; a write that fails leaves the old file, read again next time.
                if old {
                    let _ = self.write(&secrets);
                }
                Ok(secrets)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(e) => Err(self.shown(e)),
        }
    }

    /// Replaces the file with `secrets` through a temporary file of this write's own beside it, so
    /// a crash leaves the old file or the new one; removes it when there are none left.
    fn write(&self, secrets: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
        if secrets.is_empty() {
            return match std::fs::remove_file(&self.path) {
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(self.shown(e)),
                _ => Ok(()),
            };
        }
        static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = self
            .path
            .with_extension(format!("json.{}.{n}.tmp", std::process::id()));
        let json = serde_json::to_vec(secrets).map_err(|e| e.to_string())?;
        let sealed = self.seal(&json)?;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let written = options.open(&tmp).and_then(|mut file| {
            std::io::Write::write_all(&mut file, &sealed)?;
            file.sync_all()
        });
        if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &self.path)) {
            let _ = std::fs::remove_file(&tmp);
            return Err(self.shown(e));
        }
        Ok(())
    }

    /// The file's entry for `key`, if the file is there.
    fn entry(&self, key: &str) -> Result<Option<String>, String> {
        if !self.path.exists() {
            return Ok(None);
        }
        self.locked(|| Ok(self.read()?.remove(key)))
    }

    /// Everything the file holds, if it is there.
    fn entries(&self) -> Result<std::collections::BTreeMap<String, String>, String> {
        if !self.path.exists() {
            return Ok(Default::default());
        }
        self.locked(|| self.read())
    }

    fn put(&self, key: &str, value: &str) -> Result<(), String> {
        self.locked(|| {
            let mut secrets = self.read()?;
            secrets.insert(key.into(), value.into());
            self.write(&secrets)
        })
    }

    /// The keychain took `key`: removes its entry if it still holds `before`, what was read before
    /// the keychain call. Whether the file still has an entry for it: one another process wrote
    /// since stays, and is read first.
    fn settle(&self, key: &str, before: Option<&str>) -> Result<bool, String> {
        if !self.path.exists() {
            return Ok(false);
        }
        self.locked(|| {
            let mut secrets = self.read()?;
            if before.is_some() && secrets.get(key).map(String::as_str) == before {
                secrets.remove(key);
                self.write(&secrets)?;
                return Ok(false);
            }
            Ok(secrets.contains_key(key))
        })
    }

    /// The install's own secret beside the file (`account-secrets.key`): 32 random bytes, readable
    /// and writable by the player only, made at the first write when `make`. `None` when it isn't
    /// there and not made.
    fn install_secret(&self, make: bool) -> Result<Option<[u8; 32]>, String> {
        let path = self.path.with_extension("key");
        match std::fs::read(&path) {
            Ok(bytes) => <[u8; 32]>::try_from(bytes).map(Some).map_err(|_| {
                format!(
                    "{} is damaged. Remove it and {} and sign in again.",
                    path.display(),
                    self.path.display()
                )
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && make => {
                let mut secret = [0u8; 32];
                getrandom::fill(&mut secret).map_err(|e| e.to_string())?;
                // Whole or not at all, as the secrets file: a temporary file, then a rename.
                let tmp = path.with_extension(format!("key.{}.tmp", std::process::id()));
                let mut options = std::fs::OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
                let written = options.open(&tmp).and_then(|mut file| {
                    std::io::Write::write_all(&mut file, &secret)?;
                    file.sync_all()
                });
                if let Err(e) = written.and_then(|()| std::fs::rename(&tmp, &path)) {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(format!("{}: {e}", path.display()));
                }
                Ok(Some(secret))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// `json` sealed for the file ([`SEALED`]), under [`file_key`] with this install's secret.
    fn seal(&self, json: &[u8]) -> Result<Vec<u8>, String> {
        use chacha20poly1305::{aead::Aead, KeyInit};
        let key = file_key(&self.install_secret(true)?.expect("made"));
        let mut nonce = [0u8; 12];
        getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
        let sealed = chacha20poly1305::ChaCha20Poly1305::new(&key)
            .encrypt(&nonce.into(), json)
            .map_err(|_| "the secrets couldn't be sealed".to_string())?;
        Ok([SEALED, &nonce, &sealed].concat())
    }

    /// What [`Self::seal`] sealed, and whether it was sealed in format 1 ([`SEALED_V1`]), to seal
    /// again; `None` for a file in another format, changed, sealed on another computer or by
    /// another install, or whose install secret is gone.
    fn unseal(&self, bytes: &[u8]) -> Result<Option<(Vec<u8>, bool)>, String> {
        use chacha20poly1305::{aead::Aead, KeyInit};
        let (key, old, rest) = if let Some(rest) = bytes.strip_prefix(SEALED) {
            let Some(secret) = self.install_secret(false)? else {
                return Ok(None);
            };
            (file_key(&secret), false, rest)
        } else if let Some(rest) = bytes.strip_prefix(SEALED_V1) {
            (file_key_v1(), true, rest)
        } else {
            return Ok(None);
        };
        let Some((nonce, sealed)) = rest.split_at_checked(12) else {
            return Ok(None);
        };
        let nonce: [u8; 12] = nonce.try_into().expect("12 bytes");
        Ok(chacha20poly1305::ChaCha20Poly1305::new(&key)
            .decrypt(&nonce.into(), sealed)
            .ok()
            .map(|json| (json, old)))
    }
}

/// The secrets file's first bytes: format 2, the JSON sealed with ChaCha20-Poly1305 under
/// [`file_key`], then a random 12-byte nonce and the ciphertext with its tag.
const SEALED: &[u8] = b"HZSECv2\n";

/// Format 1, sealed under [`file_key_v1`]: still read, and sealed again in format 2 at once.
const SEALED_V1: &[u8] = b"HZSECv1\n";

/// This computer's identity: `/etc/machine-id` on Linux, `/etc/hostid` on FreeBSD.
fn machine_id() -> Option<Vec<u8>> {
    ["/etc/machine-id", "/var/lib/dbus/machine-id", "/etc/hostid"]
        .iter()
        .find_map(|path| std::fs::read(path).ok())
        .map(|id| id.trim_ascii().to_vec())
}

/// The secrets file's key: a constant of the launcher's, this computer's identity and the install's
/// own random secret (`account-secrets.key`, 0600). The tokens are never on disk in plain text,
/// and the key is never something every local user can work out (the machine id is readable by
/// all, and some systems have none): it takes the install secret, which only the player can read.
/// The file's mode (0600) is still what keeps other users out; the secret means a copy of the file
/// alone, or one left readable by others while the secret wasn't, can't be read either.
fn file_key(install_secret: &[u8; 32]) -> chacha20poly1305::Key {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    hash.update(b"horizon-launcher account-secrets v2");
    let id = machine_id().unwrap_or_default();
    hash.update((id.len() as u64).to_le_bytes());
    hash.update(&id);
    hash.update(install_secret);
    hash.finalize()
}

/// Format 1's key, only to read a file sealed before format 2: the constant and the machine id,
/// which anyone on the computer can read (the bare constant where there is none).
fn file_key_v1() -> chacha20poly1305::Key {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    hash.update(b"horizon-launcher account-secrets v1");
    if let Some(id) = machine_id() {
        hash.update(id);
    }
    hash.finalize()
}

/// A file entry for an account signed out while the keychain was away: the keychain's copy, if any,
/// counts as gone, and goes at the first start it answers.
const SIGNED_OUT: &str = "";

impl SecretStore for FileStore {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        self.locked(|| Ok(self.read()?.remove(key)))
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        self.locked(|| {
            let mut secrets = self.read()?;
            secrets.insert(key.into(), secret.into());
            self.write(&secrets)
        })
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        self.locked(|| {
            let mut secrets = self.read()?;
            if secrets.remove(key).is_none() {
                return Ok(());
            }
            self.write(&secrets)
        })
    }
    fn persistent(&self) -> bool {
        true
    }
}

/// The Secret Service (GNOME Keyring, KWallet) through oo7, as gpui itself uses it.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub struct SystemStore;
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
impl SystemStore {
    fn keyring() -> Result<oo7::Keyring, String> {
        futures_lite::future::block_on(async {
            // `oo7::Keyring::new` takes `GTK_USE_PORTAL=1` (set by many KDE desktops) for a
            // sandbox, and then keeps the secrets in a file under a key the Secret portal gives
            // per app id. Outside a sandbox that id comes from how the launcher was started (the
            // menu or a terminal), so the file opened in one start and not the next (#3249).
            // Flatpak (`/.flatpak-info`) and Snap (`$SNAP`) keep the portal's file.
            let sandboxed =
                Path::new("/.flatpak-info").exists() || std::env::var_os("SNAP").is_some();
            let keyring = if sandboxed {
                oo7::Keyring::new().await.map_err(|e| e.to_string())?
            } else {
                let service = oo7::dbus::Service::new().await.map_err(|e| e.to_string())?;
                oo7::Keyring::DBus(
                    service
                        .default_collection()
                        .await
                        .map_err(|e| e.to_string())?,
                )
            };
            keyring.unlock().await.map_err(|e| e.to_string())?;
            Ok(keyring)
        })
    }
}
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
impl SecretStore for SystemStore {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        let keyring = Self::keyring()?;
        futures_lite::future::block_on(async {
            let attributes = vec![("service", SERVICE), ("account", key)];
            let items = keyring
                .search_items(&attributes)
                .await
                .map_err(|e| e.to_string())?;
            let Some(item) = items.into_iter().next() else {
                return Ok(None);
            };
            item.unlock().await.map_err(|e| e.to_string())?;
            let secret = item.secret().await.map_err(|e| e.to_string())?;
            Ok(String::from_utf8(secret.to_vec()).ok())
        })
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        let keyring = Self::keyring()?;
        futures_lite::future::block_on(async {
            let attributes = vec![("service", SERVICE), ("account", key)];
            keyring
                .create_item(
                    "Horizon launcher: Minecraft account",
                    &attributes,
                    secret.as_bytes(),
                    true,
                )
                .await
                .map_err(|e| e.to_string())
        })
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        let keyring = Self::keyring()?;
        futures_lite::future::block_on(async {
            let attributes = vec![("service", SERVICE), ("account", key)];
            keyring.delete(&attributes).await.map_err(|e| e.to_string())
        })
    }
    fn persistent(&self) -> bool {
        true
    }
    fn lock_file(&self) -> Option<PathBuf> {
        Some(crate::backend::data_dir().join("account-session.lock"))
    }
}

/// Keychain on macOS, Credential Manager on Windows (the keyring crate). Credential Manager takes at
/// most 2560 bytes of UTF-16 per item and a sign-in's secrets are longer, so on Windows a secret is
/// kept in pieces: the first under the account's key, the rest under `key.1`, `key.2`... The
/// Keychain has no such limit, and every item is a consent prompt for a launcher signature it
/// doesn't know yet — pieces meant several prompts after every self-update (#4912) — so on macOS a
/// secret is one item, and pieces an older launcher left fold into it as they are read.
#[cfg(any(target_os = "macos", target_os = "windows"))]
pub struct SystemStore;

/// Bytes of UTF-8 per keychain item: at most twice as many in UTF-16, well under 2560.
#[cfg(any(target_os = "windows", test))]
const PIECE: usize = 1000;

#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn piece_key(key: &str, n: usize) -> String {
    if n == 0 {
        key.to_string()
    } else {
        format!("{key}.{n}")
    }
}

/// `secret` cut into pieces of at most `PIECE` bytes, on character boundaries.
#[cfg(any(target_os = "windows", test))]
fn split_secret(mut secret: &str) -> Vec<&str> {
    let mut pieces = Vec::new();
    while secret.len() > PIECE {
        let mut cut = PIECE;
        while !secret.is_char_boundary(cut) {
            cut -= 1;
        }
        let (head, tail) = secret.split_at(cut);
        pieces.push(head);
        secret = tail;
    }
    pieces.push(secret);
    pieces
}

/// One keychain item per key: what a secret's pieces are kept in.
#[cfg(any(target_os = "macos", target_os = "windows", test))]
trait Items {
    fn read(&self, key: &str) -> Result<Option<String>, String>;
    fn write(&self, key: &str, piece: &str) -> Result<(), String>;
    /// Removes one item; false when there was none.
    fn remove(&self, key: &str) -> Result<bool, String>;
}

#[cfg(any(target_os = "windows", test))]
fn get_pieces(items: &dyn Items, key: &str) -> Result<Option<String>, String> {
    let Some(mut secret) = items.read(key)? else {
        return Ok(None);
    };
    for n in 1.. {
        match items.read(&piece_key(key, n))? {
            Some(piece) => secret.push_str(&piece),
            None => break,
        }
    }
    Ok(Some(secret))
}

#[cfg(any(target_os = "windows", test))]
fn set_pieces(items: &dyn Items, key: &str, secret: &str) -> Result<(), String> {
    let before = get_pieces(items, key).ok().flatten();
    let pieces = split_secret(secret);
    for (n, piece) in pieces.iter().enumerate() {
        if let Err(e) = items.write(&piece_key(key, n), piece) {
            // New pieces before old ones read back as no secret at all (the account would be
            // signed out at the next start): the old secret goes back, which fit before.
            if n > 0 {
                let _ = delete_pieces(items, key);
                if let Some(before) = &before {
                    for (n, piece) in split_secret(before).iter().enumerate() {
                        let _ = items.write(&piece_key(key, n), piece);
                    }
                }
            }
            return Err(e);
        }
    }
    // Pieces left over from a longer secret would be read back as part of this one.
    for n in pieces.len().. {
        match items.remove(&piece_key(key, n)) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                // Some leftovers are already gone, some are still there: the old secret goes back
                // whole, as on a refused write above.
                let _ = delete_pieces(items, key);
                if let Some(before) = &before {
                    for (n, piece) in split_secret(before).iter().enumerate() {
                        let _ = items.write(&piece_key(key, n), piece);
                    }
                }
                return Err(e);
            }
        }
    }
    Ok(())
}

#[cfg(any(target_os = "macos", target_os = "windows", test))]
fn delete_pieces(items: &dyn Items, key: &str) -> Result<(), String> {
    for n in 0.. {
        if !items.remove(&piece_key(key, n))? {
            break;
        }
    }
    Ok(())
}

/// One item per key, the whole secret in it (macOS). A secret an older launcher split over `key.1`,
/// `key.2`, ... reads whole and folds into the one item, so a read touches a single item — and asks
/// once — from then on. A merged item also reads fine as "piece 0" to launchers that still split.
#[cfg(any(target_os = "macos", test))]
fn get_item(items: &dyn Items, key: &str) -> Result<Option<String>, String> {
    let Some(mut secret) = items.read(key)? else {
        return Ok(None);
    };
    let mut split = false;
    for n in 1.. {
        match items.read(&piece_key(key, n))? {
            Some(piece) => {
                secret.push_str(&piece);
                split = true;
            }
            None => break,
        }
    }
    if split {
        let _ = set_item(items, key, &secret);
    }
    Ok(Some(secret))
}

/// Writes `secret` to `key` and removes `key.1`, `key.2`, ... pieces an older, split secret left:
/// they would read back as part of this one. A removal the keychain refuses puts the old items
/// back: the merged secret beside the pieces left would read as a longer, unparseable one, and
/// pieces removed before the failure can't be had again except from here.
#[cfg(any(target_os = "macos", test))]
fn set_item(items: &dyn Items, key: &str, secret: &str) -> Result<(), String> {
    let mut before = Vec::new();
    if let Some(piece) = items.read(key)? {
        before.push((key.to_string(), piece));
        for n in 1.. {
            match items.read(&piece_key(key, n))? {
                Some(piece) => before.push((piece_key(key, n), piece)),
                None => break,
            }
        }
    }
    items.write(key, secret)?;
    for n in 1.. {
        match items.remove(&piece_key(key, n)) {
            Ok(true) => {}
            Ok(false) => break,
            Err(e) => {
                if before.is_empty() {
                    let _ = items.remove(key);
                } else {
                    for (key, piece) in &before {
                        let _ = items.write(key, piece);
                    }
                }
                return Err(e);
            }
        }
    }
    Ok(())
}

/// Credential Manager's error 8 (`ERROR_NOT_ENOUGH_MEMORY`) on a write: not the computer's memory,
/// the player's credential store is full (an undocumented limit of its total size). The Xbox app
/// and the Minecraft Launcher are known to leave hundreds of items there (#4313, #4366).
#[cfg(any(target_os = "windows", test))]
pub const CREDENTIAL_MANAGER_FULL: &str = "Windows Credential Manager is full, so Horizon can't \
     keep your sign-in (Windows error 8; it isn't your computer's memory). Open Credential Manager \
     from the Start menu, go to Windows Credentials and remove entries you no longer use, then sign \
     in again. The Xbox app and the Minecraft Launcher often leave many XblGrts and Xbl| entries \
     there; removing an entry can sign the app that made it out.";

/// What the launcher says for a Windows error code Credential Manager gave when writing an item,
/// when it says more than the code.
#[cfg(any(target_os = "windows", test))]
fn credential_manager_says(code: u32) -> Option<&'static str> {
    const ERROR_NOT_ENOUGH_MEMORY: u32 = 8;
    (code == ERROR_NOT_ENOUGH_MEMORY).then_some(CREDENTIAL_MANAGER_FULL)
}

/// A running executable removed by an older macOS updater cannot create Keychain credentials.
#[cfg(any(target_os = "macos", test))]
pub const MACOS_LAUNCHER_RESTART: &str = "The launcher was updated while it was running. Restart the launcher, then sign in with Ely.by again.";

#[cfg(any(target_os = "macos", test))]
fn macos_keychain_says(code: i32) -> Option<&'static str> {
    // CSSM's Unix ENOENT: Keychain cannot identify an executable removed by an older updater.
    (code == 100002).then_some(MACOS_LAUNCHER_RESTART)
}

/// The keyring crate's error as the player reads it.
#[cfg(any(target_os = "macos", target_os = "windows"))]
fn write_error(e: keyring::Error) -> String {
    #[cfg(target_os = "macos")]
    if let keyring::Error::PlatformFailure(inner) = &e {
        if let Some(said) = inner
            .downcast_ref::<security_framework::base::Error>()
            .and_then(|error| macos_keychain_says(error.code()))
        {
            return said.to_string();
        }
    }
    #[cfg(target_os = "windows")]
    if let keyring::Error::PlatformFailure(inner) = &e {
        if let Some(said) = inner
            .downcast_ref::<keyring::windows::Error>()
            .and_then(|code| credential_manager_says(code.0))
        {
            return said.to_string();
        }
    }
    e.to_string()
}

/// The OS keychain's items, under [`SERVICE`].
#[cfg(any(target_os = "macos", target_os = "windows"))]
struct KeyringItems;

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl Items for KeyringItems {
    fn read(&self, key: &str) -> Result<Option<String>, String> {
        match keyring::Entry::new(SERVICE, key).and_then(|e| e.get_password()) {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e.to_string()),
        }
    }
    fn write(&self, key: &str, piece: &str) -> Result<(), String> {
        keyring::Entry::new(SERVICE, key)
            .and_then(|e| e.set_password(piece))
            .map_err(write_error)
    }
    fn remove(&self, key: &str) -> Result<bool, String> {
        match keyring::Entry::new(SERVICE, key).and_then(|e| e.delete_credential()) {
            Ok(()) => Ok(true),
            Err(keyring::Error::NoEntry) => Ok(false),
            Err(e) => Err(e.to_string()),
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
impl SecretStore for SystemStore {
    #[cfg(target_os = "macos")]
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        get_item(&KeyringItems, key)
    }
    #[cfg(target_os = "windows")]
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        get_pieces(&KeyringItems, key)
    }
    #[cfg(target_os = "macos")]
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        set_item(&KeyringItems, key, secret)
    }
    #[cfg(target_os = "windows")]
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        set_pieces(&KeyringItems, key, secret)
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        delete_pieces(&KeyringItems, key)
    }
    fn persistent(&self) -> bool {
        true
    }
    fn lock_file(&self) -> Option<PathBuf> {
        Some(crate::backend::data_dir().join("account-session.lock"))
    }
}

pub const NO_KEYCHAIN: &str = "No keychain available: you are signed in until the launcher closes, then you will have to sign in again.";

/// What the keychain answered when [`store`] fell back to memory.
static NO_KEYCHAIN_WHY: OnceLock<String> = OnceLock::new();

/// What the window and the CLI say when sign-ins last only this run: [`NO_KEYCHAIN`] and why.
/// None while the keychain keeps them.
pub fn no_keychain() -> Option<String> {
    if store().persistent() {
        return None;
    }
    Some(match NO_KEYCHAIN_WHY.get() {
        Some(why) => format!("{NO_KEYCHAIN} The keychain said: {why}"),
        None => NO_KEYCHAIN.into(),
    })
}

/// What a keychain this run stopped asking answers.
const KEYCHAIN_AWAY: &str =
    "The keychain didn't answer earlier: sign in again to keep this account.";

/// The keychain, with the secrets file for what it can't keep (#5095). Each call asks both, never a
/// choice made once: a launcher that wrote to the file while the keychain was away still finds its
/// secrets after another process handed them to the keychain. Reads take the file's copy first (it
/// is the newer one), writes go to the keychain and to the file only when it refuses.
///
/// The file's lock is never held across a keychain call, which can wait on an unlock prompt: other
/// processes keep reading and writing the file meanwhile. Every write to the keychain holds a lock of
/// its own instead (`account-secrets.keychain.lock`), with what it writes read under it, so a hand-over
/// never puts back a secret another process renewed or signed out since; only keychain writes wait
/// on a prompt, as they would on the keychain itself. A file entry is only removed if it still holds
/// what was read before the keychain call, so another process's newer write stays. And once the
/// keychain refuses or doesn't answer, this run stops asking it (no prompt after a dismissed
/// prompt), until another process hands it a secret this run saw in the file.
pub struct Layered {
    keychain: Box<dyn SecretStore>,
    file: FileStore,
    away: std::sync::atomic::AtomicBool,
    /// What this run saw in the file and didn't hand over itself: put there, read there, or there
    /// when it started. Gone from the file later, the keychain answers again (another process
    /// handed it).
    parked: Mutex<std::collections::HashSet<String>>,
}

impl Layered {
    /// Starts by handing the keychain what the file kept while it was away, one secret at a time,
    /// up to the first it refuses; sign-outs it missed are done there too. Each is read again under
    /// the keychain's lock: one another process renewed or signed out meanwhile isn't handed over.
    pub fn new(keychain: Box<dyn SecretStore>, file: FileStore) -> Self {
        let layered = Self {
            keychain,
            file,
            away: Default::default(),
            parked: Default::default(),
        };
        let keys: Vec<String> = layered
            .file
            .entries()
            .unwrap_or_default()
            .into_keys()
            .collect();
        // Seen before the hand-over starts: whatever another process hands over meanwhile, this
        // run asks the keychain for it again.
        layered.parked.lock().unwrap().extend(keys.iter().cloned());
        for key in keys {
            let handed = layered.keychain_writes(|| {
                let Some(secret) = layered.file.entry(&key)? else {
                    return Ok(());
                };
                layered.ask(|keychain| match secret.as_str() {
                    SIGNED_OUT => keychain.delete(&key),
                    _ => keychain.set(&key, &secret),
                })?;
                layered.unpark(&key, Some(secret))
            });
            if handed.is_err() {
                break;
            }
        }
        layered
    }

    /// Runs `f`, a write to the keychain and what it reads first, holding the keychain's lock.
    fn keychain_writes<T>(&self, f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
        self.file.holding("keychain.lock", f)
    }

    fn is_away(&self) -> bool {
        self.away.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Asks the keychain unless it was away; an error means it is away for the rest of the run.
    fn ask<T>(
        &self,
        call: impl FnOnce(&dyn SecretStore) -> Result<T, String>,
    ) -> Result<T, String> {
        if self.is_away() {
            return Err(KEYCHAIN_AWAY.into());
        }
        let answer = call(self.keychain.as_ref());
        if answer.is_err() {
            self.away.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        answer
    }

    /// Puts `value` in the file for `key`, the keychain being away.
    fn park(&self, key: &str, value: &str) -> Result<(), String> {
        self.file.put(key, value)?;
        self.parked.lock().unwrap().insert(key.into());
        Ok(())
    }

    /// The keychain took `key`: the file's copy read before goes, unless another process wrote since.
    /// Still in the file, it stays counted as seen there, so its hand-over by another process is
    /// noticed.
    fn unpark(&self, key: &str, before: Option<String>) -> Result<(), String> {
        let still_there = self.file.settle(key, before.as_deref())?;
        let mut parked = self.parked.lock().unwrap();
        if still_there {
            parked.insert(key.into());
        } else {
            parked.remove(key);
        }
        Ok(())
    }
}

impl SecretStore for Layered {
    fn get(&self, key: &str) -> Result<Option<String>, String> {
        match self.file.entry(key)? {
            Some(secret) if secret == SIGNED_OUT => return Ok(None),
            Some(secret) => {
                self.parked.lock().unwrap().insert(key.into());
                return Ok(Some(secret));
            }
            None => {}
        }
        // Seen in the file and gone from it: another process handed it to the keychain, which
        // answers again.
        if self.is_away() && self.parked.lock().unwrap().remove(key) {
            self.away.store(false, std::sync::atomic::Ordering::Relaxed);
        }
        self.ask(|keychain| keychain.get(key))
    }
    fn set(&self, key: &str, secret: &str) -> Result<(), String> {
        if self.is_away() {
            return self.park(key, secret);
        }
        self.keychain_writes(|| {
            let before = self.file.entry(key)?;
            match self.ask(|keychain| keychain.set(key, secret)) {
                Ok(()) => self.unpark(key, before),
                Err(_) => self.park(key, secret),
            }
        })
    }
    fn delete(&self, key: &str) -> Result<(), String> {
        // The keychain may hold a copy: the mark hides it until it can go.
        if self.is_away() {
            return self.park(key, SIGNED_OUT);
        }
        self.keychain_writes(|| {
            let before = self.file.entry(key)?;
            match self.ask(|keychain| keychain.delete(key)) {
                Ok(()) => self.unpark(key, before),
                Err(_) => self.park(key, SIGNED_OUT),
            }
        })
    }
    fn persistent(&self) -> bool {
        true
    }
}

/// The OS keychain if it answers, else why not.
fn keychain_or_why() -> Result<Box<dyn SecretStore>, String> {
    // Unit tests never reach the player's keychain.
    #[cfg(all(
        not(test),
        any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "macos",
            target_os = "windows"
        )
    ))]
    {
        if std::env::var_os("HORIZON_NO_KEYCHAIN").is_some() {
            return Err("HORIZON_NO_KEYCHAIN is set.".into());
        }
        let keychain = Bounded::new(SystemStore, KEYCHAIN_WAIT);
        keychain.get("probe").map(|_| Box::new(keychain) as _)
    }
    #[cfg(not(all(
        not(test),
        any(
            target_os = "linux",
            target_os = "freebsd",
            target_os = "macos",
            target_os = "windows"
        )
    )))]
    Err("This system has no keychain.".into())
}

/// Ely.by sign-ins. Linux and FreeBSD: the Secret Service with the secrets file beside it
/// ([`Layered`], #5095). macOS and Windows: the keychain if it answers. Memory with
/// `HORIZON_NO_KEYCHAIN` (the launcher's own tests) or no keychain on macOS and Windows (said in the
/// UI with [`no_keychain`]).
pub fn store() -> &'static dyn SecretStore {
    static STORE: OnceLock<Box<dyn SecretStore>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            #[cfg(all(not(test), any(target_os = "linux", target_os = "freebsd")))]
            if std::env::var_os("HORIZON_NO_KEYCHAIN").is_none() {
                return Box::new(Layered::new(
                    Box::new(Bounded::new(SystemStore, KEYCHAIN_WAIT)),
                    FileStore::launcher(),
                ));
            }
            keychain_or_why().unwrap_or_else(|why| {
                let _ = NO_KEYCHAIN_WHY.set(why);
                Box::<MemoryStore>::default()
            })
        })
        .as_ref()
}

/// Secrets never written to a file of the launcher's own, the Tailscale access token (it
/// administers the player's whole tailnet): the keychain if it answers, else memory for this run.
/// [`store`] itself on macOS and Windows, where that is what it is.
pub fn keychain() -> &'static dyn SecretStore {
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        static KEYCHAIN: OnceLock<Box<dyn SecretStore>> = OnceLock::new();
        KEYCHAIN
            .get_or_init(|| keychain_or_why().unwrap_or_else(|_| Box::<MemoryStore>::default()))
            .as_ref()
    }
    #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
    store()
}

/// Whether sign-ins sit in the secrets file now, the keychain having refused them (#5095): sign-out
/// marks alone, or a file that can't be read, don't count.
pub fn in_secrets_file() -> bool {
    cfg!(any(target_os = "linux", target_os = "freebsd"))
        && store().persistent()
        && FileStore::launcher().holds_secrets()
}

// ----- Accounts ------------------------------------------------------------------------------------

/// The version of accounts.json this launcher writes. Files without it are from launchers that also
/// signed in with Microsoft (`Accounts::load_from`).
const ACCOUNTS_VERSION: u64 = 2;

/// A Microsoft account an older launcher kept: no longer supported, remembered only until its
/// keychain item is deleted ([`Accounts::drop_microsoft`]).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Microsoft {
    pub uuid: String,
    pub name: String,
}

/// accounts.json: the signed-in accounts and the one that plays. Never a token.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Accounts {
    pub accounts: Vec<Account>,
    /// The UUID of the account the game starts with; none: the first one.
    pub selected: Option<String>,
    /// Offline names an older launcher kept (`offline`), read only so that the next save also
    /// deletes their skins and capes (`skins/<name>.png`, `skins/<name>.cape.png`). Its
    /// `offline_selected` and `looks` are dropped on read.
    #[serde(default, rename = "offline", skip_serializing)]
    old_offline: Vec<String>,
    /// Microsoft accounts an older launcher signed in: dropped from `accounts` on read, their
    /// sign-ins still in the keychain until [`Accounts::drop_microsoft`] deletes them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    microsoft: Vec<Microsoft>,
}

/// The secrets an account keeps in `store`, if it still has them there.
pub(crate) fn secrets_in(store: &dyn SecretStore, uuid: &str) -> Option<Secrets> {
    store
        .get(uuid)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
}

impl Accounts {
    pub(crate) fn path() -> PathBuf {
        crate::backend::data_dir().join("accounts.json")
    }
    pub fn load() -> Self {
        Self::load_from(&Self::path())
    }
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::path())
    }
    pub(crate) fn load_from(path: &Path) -> Self {
        let Some(mut file) = std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return Self::default();
        };
        // Before version 2, an account's `provider` told Microsoft's from Ely.by's (none: Microsoft).
        let old = file.get("version").and_then(Value::as_u64).unwrap_or(0) < ACCOUNTS_VERSION;
        let mut dropped = Vec::new();
        if let Some(list) = old
            .then(|| file.get_mut("accounts").and_then(Value::as_array_mut))
            .flatten()
        {
            let (ely, microsoft): (Vec<Value>, Vec<Value>) = std::mem::take(list)
                .into_iter()
                .partition(|a| a.get("provider").and_then(Value::as_str) == Some("ely"));
            *list = ely;
            dropped = microsoft
                .iter()
                .filter_map(|a| {
                    Some(Microsoft {
                        uuid: str_field(a, "uuid")?,
                        name: str_field(a, "name")?,
                    })
                })
                .collect();
        }
        let mut accounts: Self = serde_json::from_value(file).unwrap_or_default();
        for gone in dropped {
            if !accounts.microsoft.contains(&gone) {
                accounts.microsoft.push(gone);
            }
        }
        if accounts.selected().is_none() {
            accounts.selected = None;
        }
        accounts
    }
    pub(crate) fn save_to(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut file = serde_json::to_value(self).map_err(|e| e.to_string())?;
        file["version"] = json!(ACCOUNTS_VERSION);
        let text = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
        std::fs::write(path, text + "\n").map_err(|e| e.to_string())?;
        // An older launcher's offline names: their skins and capes go too, missing files are fine.
        if let Some(skins) = path.parent().map(|dir| dir.join("skins")) {
            for name in &self.old_offline {
                let key = name.to_ascii_lowercase();
                for file in [format!("{key}.png"), format!("{key}.cape.png")] {
                    let _ = std::fs::remove_file(skins.join(file));
                }
            }
            let _ = std::fs::remove_dir(&skins);
        }
        Ok(())
    }
    pub fn selected(&self) -> Option<&Account> {
        let uuid = self.selected.as_ref()?;
        self.accounts.iter().find(|a| &a.uuid == uuid)
    }
    /// Find an account by name (any case) or UUID (with or without dashes).
    pub fn find(&self, who: &str) -> Option<&Account> {
        let undashed = who.replace('-', "").to_lowercase();
        self.accounts
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(who) || a.uuid == undashed)
    }
    /// Keep a new or renewed account, its secrets in the store, and select it.
    pub fn add(
        &mut self,
        store: &dyn SecretStore,
        account: Account,
        secrets: &Secrets,
    ) -> Result<(), String> {
        let _operation = session::lock(store)?;
        self.keep_sign_in(store, account, secrets)
    }

    fn keep_sign_in(
        &mut self,
        store: &dyn SecretStore,
        account: Account,
        secrets: &Secrets,
    ) -> Result<(), String> {
        store.set(
            &account.uuid,
            &serde_json::to_string(secrets).map_err(|e| e.to_string())?,
        )?;
        self.selected = Some(account.uuid.clone());
        match self.accounts.iter_mut().find(|a| a.uuid == account.uuid) {
            Some(a) => *a = account,
            None => self.accounts.push(account),
        }
        Ok(())
    }
    /// Whether an account here still has its sign-in in the keychain (no request): what Play needs.
    pub fn has_session(&self, store: &dyn SecretStore) -> bool {
        // The selected account first: its item answering ends the search after one account's
        // item: every account's item read can ask for the keychain (macOS, #4912).
        self.selected()
            .into_iter()
            .chain(self.accounts.iter().filter(|a| Some(*a) != self.selected()))
            .any(|a| secrets_in(store, &a.uuid).is_some())
    }

    /// The Microsoft accounts an older launcher kept, until [`Accounts::drop_microsoft`] runs.
    pub fn microsoft(&self) -> &[Microsoft] {
        &self.microsoft
    }

    /// Microsoft sign-in is gone: deletes the keychain items of the Microsoft accounts older
    /// launchers kept and forgets them (the caller saves). Their names come back, for the one
    /// notice that tells the player to sign in with Ely.by.
    pub fn drop_microsoft(&mut self, store: &dyn SecretStore) -> Result<Vec<String>, String> {
        if self.microsoft.is_empty() {
            return Ok(Vec::new());
        }
        let _operation = session::lock(store)?;
        for old in &self.microsoft {
            store.delete(&old.uuid)?;
        }
        Ok(self.microsoft.drain(..).map(|old| old.name).collect())
    }

    /// Plays as the signed-in account `uuid`.
    pub fn play_as(&mut self, uuid: &str) -> Result<(), String> {
        if !self.accounts.iter().any(|a| a.uuid == uuid) {
            return Err("That account isn't signed in.".into());
        }
        self.selected = Some(uuid.to_string());
        Ok(())
    }

    /// A renewed profile (its name or skin changed) in place of the old one; who plays stays.
    fn update(&mut self, account: Account) {
        if let Some(a) = self.accounts.iter_mut().find(|a| a.uuid == account.uuid) {
            *a = account;
        }
    }

    /// Sign out: the secrets leave the keychain, the account the list. The session ends at Ely.by
    /// too (best effort).
    pub fn remove(&mut self, store: &dyn SecretStore, uuid: &str) -> Result<(), String> {
        let _operation = session::lock(store)?;
        if let Some(s) = secrets_in(store, uuid) {
            elyby::invalidate(&elyby::base(), &s.minecraft_token, &s.refresh_token);
        }
        store.delete(uuid)?;
        self.accounts.retain(|a| a.uuid != uuid);
        if self.selected.as_deref() == Some(uuid) {
            self.selected = None;
        }
        Ok(())
    }
}

/// No account is signed in (or its sign-in can't be read): the game must not start.
#[derive(Debug, Default, PartialEq)]
pub struct NoAccount {
    /// Why a sign-in couldn't be read, when the keyring didn't give it (#12179), or why the
    /// account that plays can't: what is said instead of the refusal, which would tell a
    /// signed-in player to sign in.
    pub keyring: Option<String>,
}
impl std::fmt::Display for NoAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.keyring {
            Some(why) => write!(f, "{why}"),
            None => write!(f, "{NEEDS_ACCOUNT}."),
        }
    }
}

impl NoAccount {
    /// The same refusal for the window, which has Settings rather than a command to name.
    pub fn in_window(&self) -> String {
        use crate::i18n::tr;
        if let Some(why) = &self.keyring {
            return why.clone();
        }
        format!(
            "{}. {}",
            tr(NEEDS_ACCOUNT),
            tr("Open Settings and sign in under Account.")
        )
    }
}

/// The gate's pass for one launch.
#[derive(Debug, Default)]
pub struct Launch {
    /// The selected account and its renewed session; none when its sign-in couldn't be renewed
    /// now (`offline` says why).
    pub session: Option<(Account, Secrets)>,
    /// Why the selected account plays without its session this time.
    pub offline: Option<String>,
    /// A renewed sign-in the keychain refused to keep: it plays now, and may have to sign in
    /// again next time.
    pub unsaved: Option<String>,
}

/// The last [`Launch::unsaved`], for the window or the terminal to show once.
static UNSAVED: Mutex<Option<String>> = Mutex::new(None);

/// What the last launch couldn't save in the keychain, once.
pub fn take_unsaved() -> Option<String> {
    UNSAVED.lock().ok()?.take()
}

/// The gate before the game starts: an account must be signed in, and the game plays as the
/// selected one (else the first). Its session is renewed through Ely.by when it is due. With no
/// answer from Ely.by (no network) the game starts without a session, offline; a sign-in that is
/// gone from the keychain, or can't be read, refuses. The caller saves `accounts` (renewed names
/// and skins).
pub fn launch(
    ep: &Endpoints,
    store: &dyn SecretStore,
    accounts: &mut Accounts,
) -> Result<Launch, NoAccount> {
    if accounts.selected().is_none() {
        accounts.selected = accounts.accounts.first().map(|a| a.uuid.clone());
    }
    let Some(account) = accounts.selected().cloned() else {
        return Err(NoAccount::default());
    };
    let operation = session::lock(store);
    let secrets = match store.get(&account.uuid) {
        Ok(text) => text.and_then(|s| serde_json::from_str::<Secrets>(&s).ok()),
        Err(why) => {
            // Not "signed out": the keyring didn't say (#12179).
            let said = crate::i18n::trf(
                "Can't read {name}'s sign-in: {why}",
                &[("name", &account.name), ("why", &why)],
            );
            eprintln!("Horizon: {said}");
            return Err(NoAccount {
                keyring: Some(said),
            });
        }
    };
    let Some(secrets) = secrets else {
        return Err(NoAccount {
            keyring: Some(format!(
                "{} is no longer signed in on this computer. Sign in again.",
                account.name
            )),
        });
    };
    let before = serde_json::to_string(&secrets).ok();
    let checked = match &operation {
        Ok(_) => check(ep, &account, secrets),
        Err(e) => Check::NoAnswer(e.clone(), secrets),
    };
    let mut pass = Launch::default();
    let secrets = match checked {
        Check::Ready(renewed, secrets) => {
            accounts.update(renewed.clone());
            pass.session = Some((renewed, secrets.clone()));
            secrets
        }
        Check::NoAnswer(e, secrets) => {
            pass.offline = Some(e);
            secrets
        }
    };
    let after = serde_json::to_string(&secrets).ok();
    if after != before {
        if let Some(text) = after {
            // The game still starts; the player hears why the next launch may ask again.
            if let Err(e) = store.set(&account.uuid, &text) {
                let said = format!(
                    "{}'s renewed sign-in couldn't be saved in the keychain ({e}): you may \
                     have to sign in again next time.",
                    account.name
                );
                if let Ok(mut last) = UNSAVED.lock() {
                    *last = Some(said.clone());
                }
                pass.unsaved = Some(said);
            }
        }
    }
    Ok(pass)
}

/// Write the session for the game, readable by this user only, in a fresh file it deletes
/// when it reads it.
pub fn write_session_file(
    dir: &Path,
    account: &Account,
    secrets: &Secrets,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    // The folder too is this user's only (the files already are).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    // Leftovers of games that never started (refused by the game after two minutes anyway).
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let old = e
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|m| m.elapsed().is_ok_and(|age| age > Duration::from_secs(120)));
            if old {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    let mut name = [0u8; 12];
    getrandom::fill(&mut name).map_err(|e| e.to_string())?;
    let path = dir.join(format!(
        "session-{}.json",
        name.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options.open(&path).map_err(|e| e.to_string())?;
    let body = json!({
        "name": account.name,
        "uuid": account.uuid,
        "access_token": secrets.minecraft_token,
        "user_type": "elyby",
        "xuid": Value::Null,
    });
    std::io::Write::write_all(&mut file, body.to_string().as_bytes()).map_err(|e| e.to_string())?;
    Ok(path)
}

/// How long a game takes a session file: it refuses one older than this (the game's
/// `MAX_SESSION_FILE_AGE`), so the launcher deletes it then if it is still there.
const SESSION_FILE_LIFE: Duration = Duration::from_secs(120);

/// The session files this launcher wrote for games it starts, until they are gone: a game
/// deletes each as it reads it; the rest go when the game would refuse them anyway
/// ([`SESSION_FILE_LIFE`], a game that never started or crashed first) or when the launch failed
/// ([`discard_unread_sessions`]). Nothing with a token stays on disk.
static UNREAD: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

/// [`write_session_file`] for a game this launcher is about to start (see [`UNREAD`]).
fn write_launch_session_file(
    dir: &Path,
    account: &Account,
    secrets: &Secrets,
) -> Result<PathBuf, String> {
    let path = write_session_file(dir, account, secrets)?;
    let Ok(mut unread) = UNREAD.lock() else {
        return Ok(path);
    };
    if unread.is_empty() {
        // One reaper while files wait; it ends with the list (under its lock, so a file added
        // as it ends starts the next).
        let _ = std::thread::Builder::new()
            .name("Session files".into())
            .spawn(|| loop {
                std::thread::sleep(Duration::from_secs(5));
                let Ok(mut unread) = UNREAD.lock() else {
                    return;
                };
                reap(&mut unread, SESSION_FILE_LIFE);
                if unread.is_empty() {
                    return;
                }
            });
    }
    unread.push(path.clone());
    Ok(path)
}

/// Deletes the files of `unread` older than `life` (the game would refuse them) and forgets
/// those gone (read).
fn reap(unread: &mut Vec<PathBuf>, life: Duration) {
    unread.retain(|path| {
        let waiting = std::fs::metadata(path)
            .and_then(|m| m.modified())
            .is_ok_and(|m| m.elapsed().is_ok_and(|age| age <= life));
        if !waiting {
            let _ = std::fs::remove_file(path);
        }
        waiting
    });
}

/// Deletes the session files written for a launch whose game didn't start, or has ended: any it
/// didn't read is left over (a game deletes those it reads).
pub fn discard_unread_sessions() {
    let files = UNREAD
        .lock()
        .map(|mut unread| std::mem::take(&mut *unread))
        .unwrap_or_default();
    for path in files {
        let _ = std::fs::remove_file(path);
    }
}

/// Called under the account-operation lock, after the final profile is known: keeps the account,
/// its secrets, and selects it.
fn accept_sign_in(
    accounts: &mut Accounts,
    store: &dyn SecretStore,
    account: Account,
    secrets: &Secrets,
) -> Result<(), String> {
    accounts.keep_sign_in(store, account, secrets)
}

/// Signs in to Ely.by with the player's username and password (and the two-factor code, when the
/// account asks for one), and keeps the account.
pub fn elyby_sign_in(
    username: &str,
    password: &str,
    totp: Option<&str>,
) -> Result<Account, String> {
    let mut accounts = Accounts::load();
    let account = elyby_sign_in_with(
        &Endpoints::live(),
        store(),
        &mut accounts,
        username,
        password,
        totp,
    )?;
    accounts.save()?;
    Ok(account)
}

fn elyby_sign_in_with(
    ep: &Endpoints,
    store: &dyn SecretStore,
    accounts: &mut Accounts,
    username: &str,
    password: &str,
    totp: Option<&str>,
) -> Result<Account, String> {
    let client = elyby::client_token()?;
    let tokens = elyby::authenticate(&ep.ely, username, password, totp, &client)?;
    let secrets = elyby::secrets(&tokens);
    let account = elyby::with_textures(
        &ep.skins,
        Account {
            uuid: tokens.uuid,
            name: tokens.name,
            skin: None,
            cape: None,
        },
    );
    let _operation = session::lock(store)?;
    accept_sign_in(accounts, store, account.clone(), &secrets)?;
    Ok(account)
}

/// What the game about to start gets from the accounts.
#[derive(Debug, Default)]
pub struct GameSession {
    /// The playing account's session (`--session-file`); none: offline.
    pub file: Option<PathBuf>,
    /// Why the account plays offline this time, if it does.
    pub offline: Option<String>,
    /// The other accounts' sessions, for splitscreen players (`account_sessions`).
    pub env: Vec<(&'static str, String)>,
}

/// The gate for the game about to start, then the selected account's session in a file for it,
/// and the other accounts' for its splitscreen players.
pub fn session_for_game(game_home: &Path) -> Result<GameSession, NoAccount> {
    let mut accounts = Accounts::load();
    let pass = launch(&Endpoints::live(), store(), &mut accounts)?;
    let env = split_sessions(
        game_home,
        &mut accounts,
        pass.session.as_ref().map(|(a, _)| a.uuid.as_str()),
    );
    if let Err(e) = accounts.save() {
        eprintln!("Horizon: the accounts are not saved: {e}");
    }
    Ok(session_file(game_home, pass, env))
}

/// [`session_for_game`] for one launch as `who` (`play --account`, a signed-in account): who
/// plays changes for this launch only, so several games can play as several accounts at once
/// (#4952). What Ely.by renewed (tokens, names, skins) is still kept; which account
/// `account select` picked is not touched.
pub fn session_for_game_as(game_home: &Path, who: &str) -> Result<GameSession, String> {
    let mut accounts = Accounts::load();
    let pass = launch_as(&Endpoints::live(), store(), &mut accounts, who);
    if let Err(e) = accounts.save() {
        eprintln!("Horizon: the accounts are not saved: {e}");
    }
    let pass = pass?;
    let env = split_sessions(
        game_home,
        &mut accounts,
        pass.session.as_ref().map(|(a, _)| a.uuid.as_str()),
    );
    if let Err(e) = accounts.save() {
        eprintln!("Horizon: the accounts are not saved: {e}");
    }
    Ok(session_file(game_home, pass, env))
}

/// The gate as `who`, on `accounts`, which keeps its choice of player.
fn launch_as(
    ep: &Endpoints,
    store: &dyn SecretStore,
    accounts: &mut Accounts,
    who: &str,
) -> Result<Launch, String> {
    let chosen = accounts.selected.clone();
    let pass = accounts
        .play_as(who)
        .and_then(|()| launch(ep, store, accounts).map_err(|e| e.to_string()));
    accounts.selected = chosen;
    pass
}

/// The game's session files for a launch that passed the gate.
fn session_file(game_home: &Path, pass: Launch, env: Vec<(&'static str, String)>) -> GameSession {
    let mut out = GameSession {
        env,
        offline: pass.offline,
        ..GameSession::default()
    };
    if let Some((account, secrets)) = pass.session {
        match write_launch_session_file(&game_home.join("sessions"), &account, &secrets) {
            Ok(file) => out.file = Some(file),
            Err(e) => out.offline = Some(e),
        }
    }
    out
}

fn split_sessions(
    game_home: &Path,
    accounts: &mut Accounts,
    playing: Option<&str>,
) -> Vec<(&'static str, String)> {
    if splitscreen_on(game_home) {
        // The launch may override the saved player.
        let selected = std::mem::replace(&mut accounts.selected, playing.map(str::to_owned));
        let env = account_sessions(
            &Endpoints::live(),
            store(),
            &game_home.join("sessions"),
            accounts,
        );
        accounts.selected = selected;
        env
    } else {
        Vec::new()
    }
}

/// Whether the game's Splitscreen setting is on (`horizon-extras.txt`, `splitscreen:true`): only
/// then do the other accounts' sessions go to it.
fn splitscreen_on(game_home: &Path) -> bool {
    std::fs::read_to_string(game_home.join("horizon-extras.txt"))
        .is_ok_and(|text| text.lines().any(|l| l.trim() == "splitscreen:true"))
}

/// How long Play spends renewing the other accounts' sessions, in all: past it, the rest are
/// offered only with a token that still holds (no request starts; one under way finishes).
const OTHERS_WAIT: Duration = Duration::from_secs(10);

/// The signed-in accounts but the one that plays, each its session in a file of its own, for
/// the game's splitscreen players to sign in with (`HORIZON_ACCOUNT_SESSIONS`: name, uuid and
/// file each). A token not due goes as it is; otherwise it is renewed (`check`), and an account
/// that can't be renewed or whose token names another profile is left out: the game starts all
/// the same. Nothing with one account.
fn account_sessions(
    ep: &Endpoints,
    store: &dyn SecretStore,
    dir: &Path,
    accounts: &mut Accounts,
) -> Vec<(&'static str, String)> {
    account_sessions_until(ep, store, dir, accounts, Instant::now() + OTHERS_WAIT)
}

fn account_sessions_until(
    ep: &Endpoints,
    store: &dyn SecretStore,
    dir: &Path,
    accounts: &mut Accounts,
    deadline: Instant,
) -> Vec<(&'static str, String)> {
    let others: Vec<Account> = accounts
        .accounts
        .iter()
        .filter(|a| Some(&a.uuid) != accounts.selected.as_ref())
        .cloned()
        .collect();
    let mut list = Vec::new();
    for account in others {
        let skipped =
            |reason: &str| json!({ "name": account.name, "uuid": account.uuid, "skipped": reason });
        // Under the account lock, as every renewal is: a token Ely.by reissues here is not lost
        // to another launcher renewing the same account at once. Still busy at the deadline: no
        // request, the token while it holds.
        let operation = session::lock_within(
            store,
            Some(deadline.saturating_duration_since(Instant::now())),
        );
        let Some(secrets) = secrets_in(store, &account.uuid) else {
            list.push(skipped("Sign in again in the launcher."));
            continue;
        };
        let fallback = skipped("Sign in again in the launcher.");
        let before = serde_json::to_string(&secrets).ok();
        let uuid = account.uuid.clone();
        // An account Ely.by sent back to sign-in (a refused refresh, a token now for another
        // profile) is never handed over, whatever its token's time says.
        let holds = |s: &Secrets, margin: u64| {
            !s.sign_in_required && s.minecraft_expires_at > now() + margin
        };
        let known = holds(&secrets, RENEW_BEFORE);
        let late = operation.is_err() || Instant::now() >= deadline;
        let (account, secrets) = match known {
            true => (Some(account), secrets),
            // Out of time: its token while it holds, else Sign in again.
            false if late => (holds(&secrets, 60).then_some(account), secrets),
            false => match check(ep, &account, secrets) {
                Check::Ready(renewed, secrets) => {
                    accounts.update(renewed.clone());
                    (Some(renewed), secrets)
                }
                Check::NoAnswer(e, secrets) => {
                    eprintln!("{}'s session for splitscreen: {e}", account.name);
                    // Its token may still hold (no network): offered only while it does. A token
                    // for another profile is marked sign-in required, so it never is.
                    (holds(&secrets, 60).then_some(account), secrets)
                }
            },
        };
        // A renewed token, kept for the next launch.
        let after = serde_json::to_string(&secrets).ok();
        if let (true, Some(text)) = (after != before, after) {
            let _ = store.set(&uuid, &text);
        }
        drop(operation);
        let Some(account) = account else {
            list.push(fallback);
            continue;
        };
        match write_launch_session_file(dir, &account, &secrets) {
            Ok(file) => {
                list.push(json!({ "name": account.name, "uuid": account.uuid, "file": file }))
            }
            Err(_) => list.push(fallback),
        }
    }
    if list.is_empty() {
        return Vec::new();
    }
    vec![(
        "HORIZON_ACCOUNT_SESSIONS",
        serde_json::Value::from(list).to_string(),
    )]
}

/// Who plays the real Minecraft: Java Edition (a Java instance), past the gate itself
/// ([`launch`]): the selected account. With no answer from Ely.by, it plays with its last token.
/// Mojang's game takes the token on its command line, as Mojang's launcher gives it; that command
/// is never logged.
pub fn java_player() -> Result<crate::java_launch::Player, String> {
    let mut accounts = Accounts::load();
    let player = java_player_with(&Endpoints::live(), store(), &mut accounts);
    if let Err(e) = accounts.save() {
        eprintln!("Horizon: the accounts are not saved: {e}");
    }
    player
}

fn java_player_with(
    ep: &Endpoints,
    store: &dyn SecretStore,
    accounts: &mut Accounts,
) -> Result<crate::java_launch::Player, String> {
    let pass = launch(ep, store, accounts).map_err(|e| e.to_string())?;
    let (account, secrets) = match pass.session {
        Some(session) => session,
        // No answer from Ely.by: the token it has, unless that sign-in was sent back.
        None => accounts
            .selected()
            .and_then(|a| {
                let secrets = secrets_in(store, &a.uuid).filter(|s| !s.sign_in_required)?;
                Some((a.clone(), secrets))
            })
            .ok_or_else(|| {
                pass.offline
                    .unwrap_or_else(|| NoAccount::default().to_string())
            })?,
    };
    Ok(crate::java_launch::Player {
        name: account.name,
        uuid: account.uuid,
        token: secrets.minecraft_token,
    })
}

#[cfg(test)]
mod tests;
