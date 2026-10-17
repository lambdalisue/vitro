# Send a project as an archive of what .gitignore keeps

Status: Accepted

## Context

Running a project's tests in a guest needs the project in the guest. A Rust
project's tree is mostly build output and `.git`, which the guest does not need
and which can run to gigabytes. Guests are Windows, macOS and Linux, and vitro
reaches every one of them over SSH and nothing else.

## Decision

A project is sent as one tar archive holding what `.gitignore` keeps, copied
over SSH and unpacked by the guest's own `tar`.

## Alternatives considered

- A shared folder (virtiofs or 9p). Windows needs extra drivers for either, the
  Lume backend offers neither on the same terms, and a guest writing build
  output into the host's tree is exactly what a disposable VM is meant to keep
  away from the host.
- A recursive copy of the directory. It cannot leave anything out, so every run
  sends the build output too.
- `rsync`. It is not on a Windows guest.

## Consequences

- Every supported guest already ships a `tar` that reads the archive; nothing
  is installed in the guest.
- Files deleted on the host stay in the guest until the VM is destroyed.
- The whole kept tree is sent every time, not only what changed.
