# Codex permission configuration

Pass `session::Settings::permissions` when starting or resuming a session:

```rust
use stubb::codex::{ApprovalPolicy, Permissions, Sandbox};
use stubb::{permission, session};

let settings = session::Settings {
    permissions: Some(permission::Configuration::Codex(Permissions {
        approval_policy: Some(ApprovalPolicy::OnRequest),
        sandbox: Some(Sandbox::ReadOnly),
        ..Default::default()
    })),
    ..Default::default()
};
```

Run `cargo run --example permissions` to inspect requested and native-confirmed
configuration without sending a prompt. The same settings type is accepted by
`Provider::resume_session`.

## Approval and sandbox controls

Approval policies are `Untrusted`, `OnRequest`, `Never`, or `Granular`. Granular
policies preserve Codex's individual prompt categories. These settings control
when native requests are raised; answering requests belongs to the interactive
request API. Stubb currently declines those requests.

Sandbox selections are `ReadOnly`, `WorkspaceWrite`, and `DangerFullAccess`.
Workspace write can specify additional absolute writable roots, network access,
and exclusion of `/tmp` or the directory selected by `TMPDIR`. The working
directory remains a native workspace root. Approval policy and sandbox isolation
are independent: `Never` does not disable the sandbox.

A named native permission `profile` can be selected instead of `sandbox`.
Selecting both is invalid. Profiles must already exist in Codex configuration;
Stubb does not create them. Native admin policy can reject configuration.

## Defaults and reporting

Omitted permission fields preserve Codex defaults at start or stored values on
resume, according to native behavior. Explicit `false` and an empty writable-root
list are forwarded as overrides. Permission overrides are merged with effort
configuration; model and speed selection remain independent.

`Session::info().permissions` separates the request from native-reported effective
configuration. It describes configuration at attachment time, rather than live
updates or a guarantee that the platform's sandbox dependencies are installed.
Unknown policy shapes remain unknown in typed fields and are retained in `native`,
alongside reported active-profile provenance. Legacy `approval_policy` and
`sandbox` descriptions remain available for display.

Codex permission settings passed to Claude or Pi return `Unsupported` before
native session I/O. Relative or non-UTF-8 writable roots and profile/sandbox
conflicts return `InvalidSetting`; native rejection retains its RPC error.

## Validation

Unit tests cover wire mappings, inherited defaults, explicit overrides, granular
policies, validation, provider boundaries, and unknown native reports. Real CLI
checks are opt-in and use the separate keel-tester project. Its
`backend/scripts/check-codex-permissions.py` checks configuration without inference;
`--run-tools` adds low-effort Luna turns and filesystem/network probes in a temporary
directory. Live tests consume reported events rather than trusting model replies.

Live checks passed against Codex CLI 0.159.0 using `gpt-5.6-luna`, low effort,
with fast mode disabled. They verified allowed workspace/additional-root writes,
blocked outside writes and network access, successful explicitly enabled loopback
access, and resume persistence/overrides. Read-only resume was checked against the
native configuration report; the model skipped the attempted write, so that turn
provided no evidence of read-only enforcement. Configuration-only checks also
covered granular policies, invalid settings, missing native profiles, and
cross-provider rejection.

Native sandbox behavior depends on the operating system and installed isolation
tools. The Linux tool probes ran with Bubblewrap available to the tester backend.

References: [Codex app-server](https://learn.chatgpt.com/docs/app-server) and
[configuration reference](https://learn.chatgpt.com/docs/config-file/config-reference).
