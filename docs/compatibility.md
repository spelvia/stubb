# Compatibility

Stubb is embedded in long-running host applications that store its event JSON
and send its types over their own wire formats. These rules say what may
change between releases, and how hosts should read data written by another
version of Stubb.

## Versions

Stubb follows [Semantic Versioning](https://semver.org) as Cargo applies it:

- Before 1.0, the minor version is the breaking one. Within a `0.x` minor
  version (`0.1.0`, `0.1.1`, …), releases only add: new modules, functions,
  enum variants, fields of `#[non_exhaustive]` structs, event types and
  JSON fields. Breaking changes wait for the next minor version (`0.2.0`).
- From 1.0, breaking changes wait for the next major version.
- Release candidates (versions such as `0.2.0-rc.1`) may still change before
  their release.
- The minimum supported Rust version (`rust-version`) is raised only in a
  minor (before 1.0) or major release.

On release PRs, CI runs [`cargo-semver-checks`](https://github.com/obi1kenobi/cargo-semver-checks)
against the latest release tag; it starts with the first release. To run it
locally against a tag or commit:

```sh
cargo install cargo-semver-checks --locked   # or: nix shell nixpkgs#cargo-semver-checks
cargo semver-checks -p stubb --baseline-rev <previous release tag>
```

## Types that grow

- **Enums that gain variants are `#[non_exhaustive]`**: events, items, tool
  details, deltas, statuses, requests and decisions, errors, harnesses and
  their auth state, install and registry methods, logo sources, fast mode,
  task and agent kinds, observation events and the permission
  configurations. Match them with a wildcard arm (`_ => …`) that does
  something sensible for a value your code does not know yet.
- **Structs Stubb builds for hosts are `#[non_exhaustive]`** and gain
  fields: probe reports, update packages, plans and outcomes, profiles and
  logos, registry entries and account usage. Read their fields; build them
  only through their constructors, such as `discovery::Report::new` for tests.
- **Structs hosts build stay constructible**: settings, specs, queries,
  inputs, images, launches, provider configs and permissions. Build them
  with `..Default::default()` (or the type's constructor) rather than
  listing every field, so that new optional fields are not a problem in
  practice. Adding a field to them is still a breaking change and waits for
  a new minor version before 1.0.
- **Data a provider implementation returns** (catalog, session and turn
  data, observation records) stays constructible, so hosts can implement
  `Provider` and `session::Driver` themselves, for example as test fakes.
  New fields there also wait for a new minor version before 1.0, and are
  optional in JSON (see below).

## Event format

`stubb::session::EVENT_FORMAT` is the version of the JSON that
`session::Event` and everything inside it (turns, items, requests,
outcomes) serialize to. It is `1`.

Within one format, Stubb only adds: new event types, item and tool kinds,
enum values, and fields. Fields added to existing types are optional, so
older events still read. The format number changes, together with a new
minor (before 1.0) or major version, when a field or type is removed or
renamed, or when a value's meaning or JSON type changes.

Reading is tolerant:

- Unknown fields are ignored.
- Unknown event types read as `Event::Unknown(json)`, and unknown item,
  tool, delta, request, outcome and close-reason kinds as the matching
  `Unknown(json)` variant; all keep the JSON as it was read and serialize
  back to it. A known type whose shape this version cannot read also
  becomes `Unknown`, rather than failing the whole event.
- Unknown status values read as their enum's `Unknown` variant.

Hosts that store events should:

1. Store `EVENT_FORMAT` next to each event (one column per row, or one per
   stream).
2. When reading, interpret events of the formats they know. Keep events
   with a newer, unknown format as they are, without interpreting them,
   and skip or mark them in the interface; a later version of the host
   reads them.
3. Treat `Unknown` events, items and values as "something this version does
   not show": keep them, never fail on them.

## Deprecation

A public item is deprecated (`#[deprecated(since = "…", note = "use …")]`)
in at least one release before it is removed, with its replacement
available in the same release. Removal is a breaking change and follows
the version rules above. Deprecated JSON fields and event types are still
written for as long as their event format lasts.
