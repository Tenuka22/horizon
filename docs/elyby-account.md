# Ely.by accounts

Besides Microsoft ([microsoft-account.md](microsoft-account.md)), the launcher signs players in with an
Ely.by account: the Ely.by username and password, and the two-factor code when the account has one.
An Ely.by account plays online, and an online-mode server can verify it.

## Address

Ely.by sign-in and session calls go to `https://authserver.ely.by` only (the constant `ELY_AUTH` in
`crates/horizon-addresses`; `HORIZON_ELY_AUTH` overrides it, for mock tests). Endpoints:

- `POST /auth/authenticate`: sign-in. With two-factor on, the same request is sent again with the password as `<password>:<code>`.
- `POST /auth/refresh`: renews the access token once it is within the renewal window (`RENEW_BEFORE`, as for Microsoft's token).
- `POST /auth/invalidate`: sign-out.
- `POST /session/join`: the game joins an online-mode server, as Mojang's `join`.
- `GET /session/hasJoined`: the server verifies a joining player.

Errors are Ely.by's `{"error", "errorMessage"}`, mapped as the Mojang session errors are.

## Tokens

The client token and the access token go to the OS keychain, in the same per-account item as Microsoft's
secrets (see [Token storage](microsoft-account.md#token-storage)). The access token is treated as valid for
24 hours. Tokens never appear in logs or errors, and leave the secret store only in the game's session
file, as Microsoft's do.

## What does not apply

Ely.by accounts have no Microsoft services behind them, so these return nothing for them:

- secure chat keys (no certificates), and friends;
- the Minecraft profile lookup: no skin or cape from Ely.by is shown;
- the ownership check: an Ely.by account is treated as owning Minecraft: Java Edition.

## The game

The launcher hands the session to the game in the session file as usual, with `"user_type": "elyby"` and
`"xuid": null`. The game accepts `user_type` `msa` (Microsoft) and `elyby` (Ely.by) and refuses any other
value; `elyby` sets `AuthSession::provider` to Ely.

## Servers: `auth-provider`

An online-mode server (`online-mode=true`) chooses which accounts it verifies in `server.properties`:

- `auth-provider = mojang` (default): Mojang's session service.
- `auth-provider = elyby`: Ely.by's `hasJoined`.

Any other value is a configuration error.

A LAN host verifies its guests with its own provider, so guests must sign in with the same provider as the host.
