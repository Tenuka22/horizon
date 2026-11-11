# Ely.by accounts

The launcher signs players in with an [Ely.by](https://ely.by) account and nothing else: the Ely.by
username and password, and the two-factor code when the account has one. Microsoft accounts are no
longer supported. Play needs a signed-in Ely.by account (the selected one plays, on LAN and
offline-mode servers too); there are no offline names. An Ely.by account does not need to own
Minecraft: the launcher checks no ownership, and downloads the game files for it from Mojang's public
CDN ([Game files](#game-files)). An online-mode server can verify the account.

Accounts that older launchers signed in with Microsoft are dropped at the first start of this one:
their `accounts.json` entries and their keychain items are removed, and the launcher says so once.

## Address

Ely.by sign-in and session calls go to `https://authserver.ely.by` only (the constant `ELY_AUTH` in
`crates/horizon-addresses`; `HORIZON_ELY_AUTH` overrides it, for mock tests). Endpoints:

- `POST /auth/authenticate`: sign-in. With two-factor on, the same request is sent again with the password as `<password>:<code>`.
- `POST /auth/refresh`: renews the access token once it is within the renewal window (`RENEW_BEFORE`).
- `POST /auth/invalidate`: sign-out.
- `POST /session/join`: the game joins an online-mode server, as Mojang's `join`.
- `GET /session/hasJoined`: the server verifies a joining player.

Errors are Ely.by's `{"error", "errorMessage"}`, mapped as the Mojang session errors are.

Settings → Accounts signs in. `horizon-launcher account sign-in` does the same from the command line:
it asks for the username, the password and the two-factor code. The MCP has no sign-in tool: a password
must not pass through an agent. `account list`, `account sign-out`, `account select` and
`account refresh` work as before, and the MCP's `account_list` and `account_refresh` stay
([launcher-commands.md](launcher-commands.md)).

## Game files

The version file, the client jar, the libraries, the assets and the Java runtime are downloaded at run
time from Mojang's public CDN for the signed-in Ely.by account, into `minecraft/` in the launcher's
data folder, in the official launcher's layout. The download works as PolyMC's does: every file is
checked against the sha1 and size Mojang lists, written atomically, fetched in parallel, and the files
that failed are tried again for up to 3 rounds. An asset that is present with the listed size is
skipped; libraries are chosen by their rules for the operating system and CPU architecture. A
cancelled download goes on where it stopped, and what is present works offline. CI, agents, harnesses
and the repo never download or keep Mojang's files.

## Tokens and storage

The client token and the access token are the secrets. They go to the OS keychain, one item per
account: Keychain on macOS, Credential Manager on Windows, the Secret Service (GNOME Keyring, KWallet)
on Linux. Service name `dev.playhorizon.launcher`, account = the profile UUID. The access token is
treated as valid for 24 hours. Tokens never appear in logs, errors, `horizon-launcher` CLI output, MCP
results or crash reports (requests are logged by endpoint name only), and leave the secret store only
in the game's session file.

- Credential Manager takes at most 2560 bytes per item, which the secrets of a sign-in can exceed: on
  Windows a longer secret is split over the items `<uuid>`, `<uuid>.1`, `<uuid>.2`... The Keychain
  has no such limit, and every item is a consent prompt for a launcher signature it doesn't know yet,
  so on macOS a secret is one item (#4912); pieces older launchers left fold into it on the first
  read, and launchers that still split read a merged item as its first piece. A write or a clean-up
  delete the store refuses puts the old secret back: on Windows new pieces in front of old ones would
  read back as no secret, and the account would be signed out at the next start; on macOS a merged
  item beside leftover pieces would read as a longer secret that doesn't parse.
- **Windows: "Windows Credential Manager is full" (Windows error 8, #4313, #4366).** Credential
  Manager refuses every new item with `ERROR_NOT_ENOUGH_MEMORY` once the player's store reaches an
  undocumented total size, whatever the item's size; it is not the computer's memory. The Xbox app
  and the Minecraft Launcher are known to leave hundreds of `XblGrts…` and `Xbl|…` items there. The
  launcher says so and nothing is saved; the player opens Credential Manager (Start menu) → Windows
  Credentials, removes entries they no longer use (`cmdkey /list` in a terminal lists them), and
  signs in again. Removing an entry can sign the app that made it out. The launcher never deletes
  other apps' items.
- `accounts.json` in the launcher's data folder keeps only public data: UUID, name, which account is
  selected. Never a token.
- Linux and FreeBSD with no keychain answering (no Secret Service, a locked keychain, #5095): the
  secrets the keychain refuses go to `account-secrets.json` in the launcher's data folder instead,
  readable and writable by the player only (0600 on Unix) and never in plain text: the file is
  encrypted (ChaCha20-Poly1305) with a key derived from the computer's identity (`/etc/machine-id`,
  `/etc/hostid`) and a random secret of the install's own, `account-secrets.key` beside it (32
  bytes, 0600, made at the first write), so a copy taken off the computer (a backup, a synced home
  folder) can't be read elsewhere, and nobody can work the key out from what every user can read.
  On the computer itself the files' mode is what keeps other users out. A file sealed by an older
  launcher (machine identity only) is read once and sealed again under the new key. A file that
  doesn't open (another computer's, or changed) is never written over: remove it and sign in again.
  The launcher never picks one store for a run: every read takes the file's copy first (it was
  written while the keychain was away, so it is the newer one) and the keychain's otherwise, and
  every write goes to the keychain and to the file only when it refuses. Each change to the file
  holds `account-secrets.lock` across its read and replace, and each write goes through a temporary
  file of its own, so launcher and CLI processes running at once lose nothing. The lock is never held
  while the keychain is asked (it can wait on an unlock prompt): a file entry the keychain took is
  removed only if it still holds what was read before, so another process's newer write stays and
  is read first. Writes to the keychain hold `account-secrets.keychain.lock` instead, with what they
  write read under it, so a hand-over never puts back a token another process renewed or signed out
  meanwhile; only keychain writes wait on a prompt, as they would on the keychain itself. Once the
  keychain refuses or doesn't answer (a dismissed prompt), the run stops asking it and keeps new
  sign-ins in the file, until it finds that another process handed the keychain a secret it saw in
  the file (one it put there, read there, or found there at its start). Every start hands the
  keychain what the file kept, one secret at a time up to the first it refuses; the rest stays in
  the file, which goes once it is empty. Signing out while the keychain is away leaves an empty entry
  for the account, which hides the keychain's copy until it can be deleted. A broken file is an error
  naming only its line and column, never what it holds. Only sign-ins go there: the Tailscale access
  token (Internet servers) administers the player's whole tailnet, so without a keychain it stays in
  memory for the run (`account::keychain`). MCP's `account_list` says `secrets_file: true` while the
  file holds a sign-in (sign-out marks alone, or a file that can't be read, don't count).
- No keychain on macOS or Windows, or `HORIZON_NO_KEYCHAIN` (the launcher's own tests): secrets stay
  in memory for the run, and the launcher says "No keychain available: you are signed in until the
  launcher closes".

## Account status and renewal

Settings shows each account's current authentication status. “Ready for online play” means its saved
access token is not near expiry; the service can revoke a token without the launcher knowing until a
later request. “Needs refresh” means its access token has an hour or less left. On launcher startup
these due sessions are renewed in the background, and Play renews a due token before every launch.
Fresh tokens are not renewed on a startup status check.

**Refresh** explicitly renews an account even when its access token is fresh. Ely.by rejecting the
saved token marks the account **Sign in again** until a successful sign-in replaces it. A network
outage offers retry rather than discarding the account. Unreadable keychain data is never overwritten
by refresh. Several accounts can be signed in; one is selected. "Sign out" invalidates the token at
Ely.by and deletes the account's secrets from the keychain.

`horizon-launcher account refresh NAME_OR_UUID` exposes the same operation (MCP: `account_refresh`).
`account list --json` includes public authentication status and token expiry, never tokens.
`account refresh --json` returns a status object, including when renewal needs attention; callers
inspect `state` rather than treating transport success as an online session. Inspection, refresh and
sign-out run off the window's drawing thread. Credential operations are serialized in-process and
between launchers using the same launcher data folder.

The reusable `account::session` API has no GPUI dependency. A running game asks the launcher for a
renewed session through the CLI: the launcher passes its own path to the game as
`HORIZON_LAUNCHER_EXE`, and `account session NAME_OR_UUID --json` renews that account and returns a
new read-once session file (`{"session_file"}`), or the public status (`{"authentication"}`) when the
account needs a new sign-in or a retry. `account session` is not an MCP tool: its file holds a token,
and only the game reads it. Renewing in the launcher does not change the token a running game already
holds until the game asks.

Account operations report the actual stage (waiting for account access, reading the keychain,
renewing the sign-in, saving). Progress is public metadata and does not expose credentials through the
UI or MCP.

## Skins and capes

Skins and heads come from Ely.by's skin system, `http://skinsystem.ely.by/textures/<name>`
([docs.ely.by/en/skins-system.html](https://docs.ely.by/en/skins-system.html)); capes are Ely.by's.
The launcher shows the head in Settings, the selected-account chip and the account switcher; images are
fetched on workers and cached under `profile-heads/` in launcher data. An unavailable image uses the
UUID's default skin from installed resources, or a neutral placeholder when those resources are absent.
The cache contains public images, never authentication tokens.

## What does not apply

Friends and secure-chat keys are Mojang-service features; an Ely.by account has none, so they return
nothing for it:

- secure chat keys (no certificates): chat on a server that enforces secure profiles is refused with
  "Chat disabled due to missing profile public key", as for any client without a chat session;
- friends.

## Handing the session to the game

Vanilla's launcher passes `--accessToken <token>` on the command line, where every user of the
machine can read it in the process list. Horizon keeps it out of argv:

- Horizon's own client (Rust) is the primary game. The launcher writes `session-<random>.json` (mode
  0600 on Unix, created new, in the game home's `sessions/` folder) with
  `{"name", "uuid", "access_token", "user_type": "elyby"}` and starts the game with
  `--session-file PATH`. The game accepts `user_type` `elyby` and refuses any other value; `elyby` sets
  `AuthSession::provider` to Ely.
- The game reads the file at start and deletes it at once; the token lives only in the game's memory.
  A file older than two minutes is refused (and deleted), so a stale file can't be replayed.
- Without a session file the game is offline (`--name`, offline UUID).
- `HORIZON_SESSION_FILE` is the same as `--session-file` (development and harnesses).
- `--session-stdin` takes the same JSON on standard input instead, so nothing touches the disk (for
  other launchers: [the Launch API](api/launch.md)).
- Java Edition instances play with the Ely.by account through authlib-injector:
  `-javaagent:authlib-injector.jar=ely.by`. The launcher downloads the jar and verifies its SHA-256.
- While the game's Splitscreen setting is on (`splitscreen:true` in `horizon-extras.txt`), the other
  signed-in accounts get a session file each too, for splitscreen players to sign in with, listed in
  `HORIZON_ACCOUNT_SESSIONS` (`[{"name", "uuid", "file"}]`). Each token is renewed first when due;
  after ten seconds no further renewal starts (an in-flight request finishes). Accounts whose sessions
  cannot be provided are listed with `skipped` instead of `file`, with no token. They remain in the
  picker marked “(Offline)”, with a “Sign in again in the launcher” tooltip. The game logs unavailable
  accounts and still starts. These names can play in P1's world or on local offline-mode servers;
  external and online-mode servers require a valid session from the launcher.

## Online-mode login (game)

Vanilla's `ClientHandshakePacketListenerImpl.handleHello`, `Crypt` and authlib's `joinServer`:

1. `ClientboundHelloPacket` (server id, X.509 RSA public key, challenge, should_authenticate).
2. Generate a random 16-byte AES key (the shared secret).
3. Server hash: SHA-1 over `serverId` (ISO-8859-1), the shared secret, the encoded public key;
   `new BigInteger(digest).toString(16)` (signed two's complement hex, e.g. "Notch" →
   `4ed1f46bbe04bc756bcb17c0c7ce3e4632f06a48`, "jeb_" → `-7c9d5b0044c130109a5d7b5fb5c317c02b4e28c1`).
4. If `should_authenticate`: `POST /session/join` on Ely.by
   `{"accessToken", "selectedProfile" (undashed UUID), "serverId": hash}`. Errors map like authlib's
   `MinecraftClientHttpException`: `ForbiddenOperationException` or 401 → "Failed to log in: Invalid
   session (Try restarting your game and the launcher)", `InsufficientPrivilegesException` →
   "Multiplayer is disabled…", `multiplayer.access.banned` / `FORCED_USERNAME_CHANGE` → "You are
   banned from playing online", 5xx or no network → "The authentication servers are currently not
   reachable…". Without a session the game says "This server is in online mode. Sign in with your
   Ely.by account in the Horizon launcher to join it." (Vanilla goes on after a failed join on
   LAN-discovered servers, which then refuse the player anyway; Horizon stops at once.)
5. Send `ServerboundKeyPacket` (the secret and the challenge, each RSA PKCS#1 v1.5 encrypted with
   the server key), then AES/CFB8 (key = IV = the secret) both ways.

The connecting screen follows vanilla's states: "Connecting to the server..." until the encryption
request, "Logging in..." while joining the session service, "Encrypting...", then "Joining world...".
Code: `horizon-protocol` (`rsa.rs`, `crypto::server_hash`), `horizon-client` (`auth.rs`, `net.rs`).

Horizon's dedicated server verifies players when `online-mode=true`. A LAN or Play with Friends world
verifies its guests when the host is signed in, as vanilla's `IntegratedServer` does
(`OnlineMode::lan`); started without a session (the services couldn't be reached at Play) it stays
offline-mode. The host's own player joins through memory and is never asked to authenticate.

## Servers: `auth-provider`

An online-mode server (`online-mode=true`) chooses which accounts it verifies in `server.properties`:

- `auth-provider = mojang` (default): Mojang's session service.
- `auth-provider = elyby`: Ely.by's `hasJoined`.

Any other value is a configuration error. A server that verifies Ely.by accounts needs
`auth-provider = elyby`.

The names an operator gives to `op`, `whitelist` and `ban` (and `horizon-server init --player`/`--op`) are
resolved by the same provider: Ely.by's `/api/users/profiles/minecraft/{name}` at `HORIZON_ELY_AUTH`, so a name
only Ely.by knows works. A player given by id (the management protocol) is found at `/session/profile/{id}`.

A LAN host verifies its guests with its own provider, so guests must sign in with the same provider as the host.

## Harnesses

`tools/test_online_mode_parity.py` (dev only, needs no account): a local mock of the session services
(discovery document, `join`, `hasJoined`/`verify`, no public keys) and a vanilla 26.3 dedicated server
in online mode started with `-Dminecraft.api.discovery.host=<mock>` (authlib 10.0.77's only override:
it replaces the whole discovery URL; the old `minecraft.api.session.host` style properties are gone).
`HORIZON_API_DISCOVERY` is the counterpart for the game's Mojang provider. Horizon's headless client
(`HORIZON_NET_REPORT`) joins with a session file holding a mock token: the server verifies the join
through the mock with the hash it computed itself (so the RSA wrapping, the hash and AES/CFB8 are all
checked against vanilla's code), the player spawns, chats and runs `/list`. Negative cases: a token
the mock rejects (the client must show vanilla's "Invalid session" message) and a client that skips
the join (the server must say "Failed to verify username!"). `HORIZON_ELY_AUTH` points the Ely.by
calls at a mock the same way.

Rust unit tests: server hash vectors (Notch, jeb_, simon), RSA wrapping round trip with a test key,
X.509 key parsing, AES/CFB8 vectors (already in `horizon-protocol`), the session file (permissions,
age, deletion), the error mapping. Launcher: the whole token chain against a local mock of the Ely.by
endpoints, and the keychain abstraction with an in-memory backend.
