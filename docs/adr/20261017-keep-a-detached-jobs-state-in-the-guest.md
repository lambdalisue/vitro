# Keep a detached job's state in the guest

Status: Accepted

## Context

A build can outlast the SSH connection that started it, and the session that
started it may not be the one that wants its result. vitro has no process on
the host that lives longer than one command.

## Decision

A detached job's command, output, process and exit code are kept in files in
the guest, and every vitro command learns what became of a job by asking the
guest.

## Alternatives considered

- A vitro process on the host that holds the connection open and records the
  output. It dies with the terminal or the session that started it, which is
  the failure detaching exists to survive, and it is a resident process vitro
  otherwise never needs.
- Recording jobs in the VM's record on the host. The job runs in the guest, so
  only the guest knows whether it is still running; a host copy can only go
  stale.

## Consequences

- Any connection, from any session, can follow a job or collect its exit code.
- A job goes with the VM when it is destroyed.
- Following a job is a poll of the guest, not a stream.
