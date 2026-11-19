# Releasing Stubb

## Prepare each release

1. Open a release PR with the Cargo version, updated lockfile and any install
   examples that specify the previous version. Use versions such as
   `0.1.0-rc.1`, `0.1.0-rc.2`, `0.1.0`, and `0.1.1`.
2. Add `docs/releases/<version>.md`. These files are Stubb's changelog. Describe
   user-visible changes, compatibility, and any migration steps. For a stable
   release, summarize changes since the previous stable release. For a release
   candidate, describe changes since the previous candidate.
3. Merge the PR and wait for the push build on main to pass. Copy the tested
   commit SHA and create an annotated tag on it. For example:

   ```sh
   git fetch origin main
   git tag -a v0.1.0-rc.1 <tested-main-sha> -m 'Stubb 0.1.0-rc.1'
   git push origin refs/tags/v0.1.0-rc.1
   ```

4. Follow the Release workflow. It checks the tag, notes, and main CI result,
   publishes the package, then creates the GitHub Release from those notes.
   Versions with a prerelease suffix get the GitHub prerelease flag.
5. Verify the published version on crates.io and its GitHub Release. Consumers
   such as Ahab can then update their dependency and lockfile.

## Configure the publisher

Create a GitHub environment named `release` in
`spelvia/stubb` and restrict deployment to release tags. Limit who can create or
modify release tags using a repository ruleset.

For the first version only, create a short-lived crates.io token scoped to
`publish-new` and the exact crate name `stubb`. Store it in the release
environment as `CRATES_IO_FIRST_PUBLISH_TOKEN`.

After RC1 appears on crates.io, register a GitHub trusted publisher in the
crate settings with owner `spelvia`, repository `stubb`, workflow `release.yml`,
and environment `release`. Revoke the first-publish token and delete the
environment secret. Subsequent tags obtain a temporary token through OIDC.

## Recover a release

If verification fails while main CI is running, wait for it to pass and rerun
failed jobs. If the upload fails, check the exact version on crates.io before
retrying. If only the GitHub Release job fails, rerun failed jobs so the
successful upload is skipped. If the registry accepted the upload but the job
reported failure, verify the published archive against the intended commit and
finish the GitHub Release using the existing tag and its checked-in notes.
Ship corrections to published code under a new version.
