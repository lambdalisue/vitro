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

On a Windows guest the command sees `PATH` as the machine and user settings
hold it at that moment, so a tool installed earlier in the VM's life is found
without restarting the guest. An explicit `--env PATH=…` still wins.

## Running a script from the host

`--script <file>` runs a script kept on the host instead of a command: it is
copied into the guest, run with the words after the VM as its arguments, and
removed afterwards, and `exec` exits with the script's exit code. The VM can be
left out as usual, and a lone word that names a VM is taken as the VM.

A Unix guest runs any file, with `sh` when it has no `#!` line. A Windows guest
chooses by extension — `.ps1` runs under PowerShell with no execution policy in
the way, `.cmd` and `.bat` under cmd — and refuses any other file before
anything is copied.

## Sending a directory first

`--sync <dir>` copies a host directory into the guest before the command runs,
as a directory of the same name in the guest account's home, and the command
runs inside it unless `--cwd` says otherwise.

What `.gitignore`, `.ignore` and `.git/info/exclude` leave out is not sent,
whether or not the directory is a Git checkout, and neither is `.git`. Hidden
files are sent. A symbolic link to a file or directory inside the directory
arrives as a copy of what it points at, and meets `.gitignore` as what it
points at; a link to anything outside the directory, into `.git`, to nothing,
or one that would contain itself, is not sent, and vitro says so. Files already in the guest directory are overwritten; files
removed on the host are not removed from the guest.

If the copy fails, the command does not run.

## In a VM started for it

`vitro run <image> -- <command>` starts a VM and runs the command in it as
`exec` would, taking the same `--sync`, `--cwd` and `--env`, and exits with the
command's exit code. Its standard output carries only the command's output.
`--script <file>` runs a host script there instead, with the words after `--`
as its arguments; a script the guest could not run is refused before the VM
starts.

With `--rm` the VM is destroyed once the command is done, whether it passed,
failed, never started because the copy failed, or was interrupted with Ctrl-C.
`--rm` without a command opens a shell instead and destroys the VM when it
closes. Without `--rm` the VM is left running.
