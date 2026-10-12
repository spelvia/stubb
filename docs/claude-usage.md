# Claude Code account usage

`Claude::account_usage()` and `Provider::account_usage()` read subscription
quotas using a short-lived stream-json process: `initialize`, then `get_usage`,
then close. They never send user input or consume a model turn. This native
experimental request was verified with Claude Code 2.1.289. Older CLIs may
return an RPC error; unsupported providers return `Error::Unsupported`.

The result includes the subscription plan and five-hour, weekly and reported
model-specific quota windows. `used_percent` uses the native 0–100 unit (unlike
some streaming rate-limit events, whose utilization uses fractions). Missing
percentages and reset times remain unknown. `raw` retains the native response,
including extra usage/spend and breakdowns, for applications that need them.
The library does not read credentials, call private HTTP endpoints, change
settings or infer quotas from token counts. Hosts should cache this operation,
for example once per minute, and select which raw fields they expose.

Run `cargo run --example usage -- /path/to/claude` to verify an installation.
