# Copying files

`vitro cp` copies files and directories between the host and one VM, over the
same connection `exec` uses.

## Operands

The last operand is the destination; every other one is a source. A guest path
is written `<vm>:<path>`, where the VM is named as anywhere else, or `:<path>`
for the one running VM. A relative guest path starts in the guest account's
home, and an empty one is the home itself.

Everything else is a host path. A colon after a path separator, or after a
drive letter on a Windows host, belongs to the host path.

## What is copied

Either every source is on the host and the destination is in a VM, or every
source is in one VM and the destination is on the host. A copy between VMs, or
with no VM in it, is refused before anything is contacted.

Directories are copied with everything in them. Existing files at the
destination are overwritten; nothing there is deleted.

`cp` exits with the code `scp` returned.
