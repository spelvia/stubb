# Codex account usage

`Codex::account_usage()` and `Provider::account_usage()` read subscription
quotas with the app-server's read-only `account/rateLimits/read`, over the
provider's shared app-server connection. They never start a thread, send
input or consume a model turn; when the app-server is already running for a
session or a catalog, reading usage starts no process.

The result lists every reported limit (`rateLimitsByLimitId`, or the single
`rateLimits` of older versions), Codex's own first, each with its primary and
secondary windows. Labels follow the window length ("5-hour limit", "Weekly
limit"), prefixed with the limit's name for limits other than Codex's.
`used_percent` keeps the native 0–100 unit; `resets_at` converts Codex's Unix
seconds to RFC 3339 UTC. Missing values remain unknown, and `raw` keeps the
native response, including credits. Accounts not signed in with ChatGPT return
an RPC error. Hosts should cache the result, for example for a few minutes.

Run `cargo run --example usage -- codex /path/to/codex` to verify an
installation.
