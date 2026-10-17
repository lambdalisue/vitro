# Naming a VM

Every command that acts on a VM accepts what the user remembers about it, not
only the name `run` generated.

## What a name may be

- The VM's full name.
- The image it was started from, when exactly one VM came from that image.
- Any prefix of a name that fits exactly one VM.

Whatever fits more than one VM is refused with the candidates listed; vitro
never picks one, since the same names are arguments to `destroy`.

## Leaving the name out

Commands shown as `[<vm>]` in `--help` act on the one running VM when no name
is given. With none running, or several, they refuse and say which.

`exec` and `launch` take a command after the VM. The first word is the VM when
it names one, and otherwise the whole line is the command for the one running
VM. A first word that fits several VMs is refused rather than run as a command.
