# Running a command in a guest

`vitro exec` runs one command in a VM and exits with that command's exit code,
so a command fails on the host exactly when it fails in the guest.

## The command

The words after the VM reach the guest as the same words. On a Unix guest each
word is quoted for the shell sshd starts, so its boundaries survive the trip.
On a Windows guest the words are joined and handed to PowerShell as written,
so they are written in PowerShell's language.

## Where it runs and what it sees

`--cwd <dir>` runs the command in that guest directory; a relative one is taken
from the guest account's home. A directory that does not exist stops the
command before it runs.

`--env NAME=VALUE` sets a variable for the command only, and may be repeated. A
name that is not letters, digits and `_` (not starting with a digit) is refused
before any VM is contacted.

Both are written in the guest shell's own syntax, so the same `vitro exec` line
means the same thing on every guest.
