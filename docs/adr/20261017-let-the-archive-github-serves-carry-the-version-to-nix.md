# Let the archive GitHub serves carry the version to Nix

Status: Accepted

## Context

A release starts from publishing a GitHub Release, and the tag is the only
record of the version: Cargo.toml holds 0.0.0 and the release workflow stamps
the tag before each cargo build. A flake is not built by that workflow. Fetched
as `github:lambdalisue/vitro/vX.Y.Z`, it sees neither the tag nor `.git`, so
its build reported a commit instead of the release.

## Decision

`VERSION` holds a `git describe` placeholder marked `export-subst`. GitHub fills
it in when it serves an archive, and the flake takes the version from it,
falling back to the commit when the placeholder is left as it is.

## Alternatives considered

- Commit the version before tagging. The release would no longer start from
  publishing a GitHub Release alone.
- Have the release workflow commit the stamped version and move the tag onto
  it. A tag that moves after publishing cannot be trusted, and immutable
  releases forbid it.
- Attach a stamped source archive to the release. A Nix user would have to
  fetch a release asset URL instead of the tag.

## Consequences

- A build from a checkout reports `0.0.0-<commit>`, since nothing fills in the
  placeholder there.
- The content of an untagged commit's archive changes when that commit is
  later tagged, so a lock file that pinned it fails its hash check on the next
  fetch.
- Nix caches only hit a build made from the archive, so CI builds from
  `github:` references rather than from its checkout.
