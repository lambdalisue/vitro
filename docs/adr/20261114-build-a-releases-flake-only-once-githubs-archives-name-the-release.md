# Build a release's flake only once GitHub's archives name the release

Status: Accepted

## Context

The flake learns its version from the archive GitHub serves
(20261017-let-the-archive-github-serves-carry-the-version-to-nix.md). Nix
resolves a tag to its commit and fetches that commit's archive. GitHub keeps
serving an archive it made of the commit before the tag existed, with VERSION
describing the commit rather than the release, for a while after tagging. CI
has always made one by then: main takes changes only through pull requests,
and both build the commit's archive. A release's Nix build that fetched it
built, and cached, the commit's description instead of the release.

## Decision

The release workflow builds the flake only once every archive of the tagged
commit that Nix may fetch names the release.

## Alternatives considered

- Tag a commit no archive has been made of. Every commit on main has been
  through a pull request's CI and main's, so there is none to tag.
- Retry the build until it reports the release. A failed build has already
  substituted, or pushed, the cached build of the stale archive; waiting on
  the archive itself avoids building from it at all.

## Consequences

- The Nix cache gets the release's build some time after the release is
  published, up to the time GitHub takes to stop serving the old archive.
- Until then, a Nix install of the tag can still build the commit's
  description, and a lock file written then fails its hash check once GitHub
  serves the new archive.
