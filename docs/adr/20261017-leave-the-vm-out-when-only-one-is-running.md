# Leave the VM out when only one is running

Status: Accepted

## Context

Most sessions have one VM running, and every command after `run` still had to
repeat its generated name. `exec` takes the VM and then a free-form command, so
the command line cannot say on its own whether the first word is a VM or the
start of the command.

## Decision

A command that acts on a VM may leave the VM out while exactly one is running.
For `exec` and `launch`, the first word is the VM if it names one; otherwise the
whole line is the command.

## Alternatives considered

- A separator, `vitro exec -- cargo test`. clap drops the separator before
  vitro sees it, and requiring it after a named VM would break every existing
  `vitro exec <vm> <cmd>` line.
- An environment variable naming the current VM. It outlives the VM it names
  and goes stale silently, which is worse than typing the name.

## Consequences

- A misspelt VM name with one VM running is run as a command in that VM, and
  fails there instead of on the host.
- A first word that fits several VMs is an error, not a command.
