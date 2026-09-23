# vitro

Disposable development VMs driven from a CLI.

_In vitro_ — in glass — is work done outside the living organism, precisely so
the organism is not put at risk. That is what this is for: the machine you ruin
is not the one you work on.

```console
$ vitro run windows           # a copy-on-write overlay on a golden image
windows-7f3a2c is up: ssh dev@127.0.0.1 -p 53422
$ vitro exec windows-7f3a2c cargo test
$ echo $?                     # the guest's exit code, not vitro's
101
$ vitro destroy windows-7f3a2c
```

## What it is for

Answering "does this actually work on that operating system?" when you do not
have that operating system to hand.

It exists because answering that used to mean buying a second machine, paying
for a hypervisor licence that unlocks its CLI, or asking somebody to click
through an installer. The work that produced vitro had all three of those in
front of it and took none of them.

Three things it has been used for, in earnest:

- **Settling a question that only Windows can answer.** An investigation had
  been stuck for weeks because nothing could drive a Windows guest without a
  human at its screen.
- **Doing things to a machine you would not do to yours.** Enabling developer
  mode, installing several gigabytes of build tools, registering packages,
  rewriting ACLs under `Program Files`. None of it touches the host, and
  `destroy` ends it.
- **Building software the host cannot build.** A Windows ARM64 binary, compiled
  and run and looked at, from a Mac.

`exec` returning the guest's own exit code is what makes the VM a build target
rather than somewhere to poke around by hand: `vitro exec win cargo test` fails
here exactly when it fails there.

**If a Linux guest is all you need, use something else.** Docker, Lima and
multipass are faster, lighter and far better worn. vitro earns its place on
Windows and macOS guests, which is where the gap was.

## Try it

Two things to install — vitro itself, and QEMU, which brings the guest firmware
with it:

```console
$ cargo install --git https://github.com/lambdalisue/vitro
$ brew install qemu                        # macOS
$ sudo apt install qemu-system qemu-utils  # Debian, Ubuntu
```

Then let vitro do the rest:

```console
$ vitro setup
```

It makes the SSH key, asks which guest you want first, works out where the
guest firmware is if this host's machine needs one, offers to let `scp` and
`rsync` reach VMs by name, and writes a configuration for that host:

```
  key       ~/.config/vitro/id_ed25519 (created)
  config    ~/.config/vitro/config.toml is missing
            which guest first? [Windows/linux/macos/skip]
```

### A Windows guest

Answering Windows — the default — gets you the one instruction vitro cannot
carry out for you:

```
Windows needs an installer ISO, and Microsoft does not allow anyone to
redistribute it — so this is the one part vitro cannot do for you.

  1. Open  https://www.microsoft.com/software-download/windows11arm64
  2. Choose "Windows 11 (multi-edition ISO for Arm-based PCs)", pick a language, and download
  3. Save it into  ~/.local/share/vitro/media

The filename does not matter: vitro identifies the media by the volume label
inside it. That label also says which architecture the ISO is for, and this
host needs aarch64 — the wrong one does not fail, it stalls at the installer's
first screen, so vitro checks before starting anything.
```

That page and that edition name are the ones for an Apple Silicon host, which
is what everything below was run on. **A guest always matches the host's
architecture**, so on x86-64 vitro names Microsoft's ordinary Windows 11
download and the x64 edition instead, and there is no `firmware` setting to
fill in — that machine boots from QEMU's own BIOS. Nothing else differs.

That directory is created for you, so the download can go straight there — and
because vitro looks in it, the whole configuration is this:

```toml
[images.windows]

[images.windows.build]
kind          = "unattended-install"
disk_size     = "80G"
build_timeout = "90m"
```

No `source`, and no `golden` either: with `golden` unset, `run` starts whatever
`build` last made for the image, so there is nothing to edit between the two.

```console
$ vitro build windows
An unattended Windows install needs the virtio drivers, which Windows does not ship.
Download them from https://fedorapeople.org/…/stable-virtio/virtio-win.iso? [y/N] y
built …/golden/windows-20260920-165554.qcow2 in 16m
the guest account's password is q5vsqqfvAmxetuytKbWT-7
```

Sixteen minutes, and nobody touched the installer. From then on a fresh Windows
machine is seconds away:

```console
$ vitro run windows
windows-7f3a2c is up: ssh dev@127.0.0.1 -p 53422
$ vitro exec windows-7f3a2c cargo test
$ vitro destroy windows-7f3a2c
```

macOS guests work the same way through
[`lume`](https://github.com/trycua/cua), with no file to download at all.
[docs/guests.md](docs/guests.md) has both in full.

### Checking the plumbing first

Before spending sixteen minutes on an install, it is worth knowing that QEMU,
the firmware, the key and the networking are all in order. Answering `linux` at
the guest prompt writes a cloud-image configuration instead — **not because
Linux is what vitro is for**, but because it is the one guest that needs nothing
downloaded by hand, so it answers that question in seconds rather than minutes:

```console
$ vitro build linux
built …/golden/linux-20260920-160437.qcow2 in 16s
$ vitro run linux && vitro exec linux-7f3a2c uname -a
```

`vitro` with no arguments prints what is running and what to do next. When
something will not start, `vitro doctor` checks every prerequisite and prints a
`→` line with the exact fix.

## The model

```
build    installation media ──(once, minutes)──> golden image
run      golden image ──(copy-on-write overlay, seconds)──> VM
destroy  the overlay goes; the golden image is untouched
```

A VM is only disposable if getting another one back is cheap, which is why
those are two commands rather than one. `promote` runs the other way: it turns
a VM you have set up into a new golden image, so the hour spent installing a
toolchain is spent once.

## Commands

| Command                  | What it does                                         |
| ------------------------ | ---------------------------------------------------- |
| `setup`                  | Get from nothing to a guest you can build            |
| `build <image>`          | Build a golden image from installation media         |
| `run <image>`            | Start a VM from a golden image                       |
| `exec <vm> <cmd>…`       | Run a command in a VM and pass its exit code through |
| `ssh <vm>`               | Open an interactive shell                            |
| `destroy <vm>` / `--all` | Stop a VM and delete its overlay                     |
| `promote <vm>`           | Turn a running VM into a new golden image            |
| `ls`                     | List VMs and golden images (`--json`, `--prune`)     |
| `status [<vm>]`          | What is running, and what to do next (`--json`)      |
| `keygen`                 | Create the SSH key vitro authorises guests with      |
| `doctor`                 | Check everything `run` and `build` depend on         |
| `ssh-config [<vm>]`      | Print an `ssh_config` block (`--write`)              |
| `forward <vm> <port>…`   | Forward guest ports to the host                      |
| `port <vm>`              | Print the SSH port and nothing else                  |
| `inspect <vm>`           | Print the VM's record as JSON                        |
| `screenshot <vm>`        | Save a picture of the guest's screen                 |
| `launch <vm> <cmd>…`     | Start a program on the guest's desktop               |

A VM can be named by any unambiguous prefix. An ambiguous one lists the
candidates rather than picking for you.

Exit codes are `0` success, `1` failure, `2` a bad command line — except
`exec`, which returns whatever the guest's command returned.

## Reaching a guest from other tools

The forwarded SSH port changes on every `run`, which is fine for `vitro exec`
and useless for `scp`, `rsync`, a debugger or an editor's remote connection.

```console
$ vitro ssh-config --write
wrote ~/.config/vitro/ssh_config
```

Add `Include ~/.config/vitro/ssh_config` near the top of `~/.ssh/config` once,
and every VM is reachable as `vitro-<name>` with no port number anywhere:

```console
$ scp ./payload vitro-linux-7f3a2c:
$ rsync -a ./src/ vitro-linux-7f3a2c:src/
```

## Documentation

| | |
| --- | --- |
| [docs/guests.md](docs/guests.md) | What each guest needs, and where to get it |
| [docs/configuration.md](docs/configuration.md) | Every setting, and what it changes |
| [docs/internals.md](docs/internals.md) | How a build and a run actually work |
| [docs/troubleshooting.md](docs/troubleshooting.md) | Things that look like failures and are not |

## What has actually been run

Worth being exact about, because the interesting bugs in this kind of tool are
the ones no test suite sees.

| Host                | State                                                              |
| ------------------- | ------------------------------------------------------------------ |
| macOS, aarch64      | **Exercised end to end.** Linux, Windows and macOS guests, built, run, promoted and destroyed |
| Windows             | Builds, lints and passes its tests. **No VM has ever been started** |
| Linux               | CI runs the same checks. **No VM has ever been started**           |

The Windows unattended install is verified on an aarch64 host. The x86-64 path
is written and reachable and has never been run. See
[docs/internals.md](docs/internals.md) for what that means in practice.

## Development

```console
$ nix develop
$ just check      # fmt, clippy, tests — what CI runs
```

CI runs those checks on Linux, macOS and Windows. Linux matters most: it is the
host this is least often developed on, and the one where a portability mistake
would otherwise go unnoticed until somebody tried it.

## License

MIT. See [LICENSE](LICENSE).
