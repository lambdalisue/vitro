# Concept

This document describes what vitro is for and the ideas every design decision
rests on. It is rewritten in place as the concept evolves; decisions built on
it are recorded as ADRs.

## What it is

A command line that builds a golden image of an operating system once, then
starts throwaway VMs from it in seconds, runs commands in them, and destroys
them. It answers "does this actually work on that operating system?" without a
second machine, a paid hypervisor or a human at the guest's screen.

## Who it is for

Developers who need a Windows or macOS guest — to settle a question only that
system can answer, to do things to a machine they would not do to their own,
or to build and run software the host cannot build — and who want to drive it
from a script or an agent rather than by hand.

## Principles

- **Disposable first.** Getting another VM back has to be cheap, so the
  expensive work (installing from media) is done once into a golden image and
  every VM is a copy-on-write overlay on it.
- **A VM is a build target.** `exec` returns the guest's own exit code, so a
  command fails on the host exactly when it fails in the guest.
- **No human in the loop.** Installation, readiness and shutdown are driven to
  completion unattended; a step that needs a person is a gap, not a feature.
- **Claim only what has run.** What has been exercised end to end is stated
  separately from what merely compiles, and a host or guest is not called
  supported until a VM has actually been started on it.

## What it does not do

- Compete on Linux guests. Docker, Lima and multipass are faster, lighter and
  better worn; vitro earns its place on Windows and macOS guests.
- Run guests of another architecture. It runs same-architecture guests and
  nothing else.
- Manage long-lived machines. A VM worth keeping is promoted to an image, not
  nursed.
