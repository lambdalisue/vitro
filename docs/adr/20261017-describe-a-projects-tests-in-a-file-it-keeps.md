# Describe a project's tests in a file it keeps

Status: Accepted

## Context

The images a project is tested on, the command that tests it and what that
command needs belong to the project, not to the machine. The machine-wide
configuration says which images exist on this host, and is not shared.

## Decision

A project describes how it is tested in a `vitro.toml` kept in its own
repository, naming images by their keys in the machine's configuration.

## Alternatives considered

- A section per project in the machine-wide configuration. Everyone who checks
  the project out would have to write it again, and it would drift from the
  project it describes.
- A script in the repository that calls `vitro run`. It works, but every
  project writes the same script, and vitro cannot check its image names before
  the first VM starts.

## Consequences

- An image key is a contract between a project and the machines that test it:
  a project naming `windows` needs a machine that defines `windows`.
- How an image is built stays per machine; the project only says which to use.
