# Configuration

One file: `~/.config/vitro/config.toml`. `vitro setup` will write a starting
point for it.

Values in `[defaults]` apply to every image, and anything in `[images.<key>]`
overrides them. **An unknown key is an error** naming both the key and the
image it is in, rather than a setting that silently does nothing — a
misspelled `momery` should not look like it worked.

Paths beginning with `~` expand against your home directory.

## `[defaults]`

| Setting | Meaning |
| --- | --- |
| `cpus` | Virtual CPUs given to each guest |
| `memory` | Memory per guest, as `4G`, `512M` and so on |
| `ssh_user` | The account vitro creates and connects as |
| `ssh_key` | Private key to authorise and present. Defaults to `~/.config/vitro/id_ed25519`; vitro expects `<ssh_key>.pub` beside it. A golden image carries whichever key was there when it was built, so changing this does not reach an image already built |
| `boot_timeout` | How long `run` waits for a guest to answer SSH. A guest that answers and refuses the key gives up sooner, since waiting cannot change that |
| `resolution` | Guest display size, as `1280x800` |
| `firmware` | The UEFI code file. Required by machines with no legacy BIOS — every aarch64 guest. An x86-64 guest gets `q35`, which boots from QEMU's BIOS, and needs no `firmware` at all |

## `[images.<key>]`

`<key>` is the name you pass to `build` and `run`. Everything in `[defaults]`
can be repeated here, plus:

| Setting | Meaning |
| --- | --- |
| `backend` | `qemu` or `lume`. Inferred from the build kind when there is one |
| `guest` | `unix` or `windows`. Inferred from the build kind; set it only for a ready-made golden image with no build section to infer from |
| `golden` | The golden image. **Leave it out** and `run` starts whatever `build` or `promote` last made for this image. A path pins one instead; for lume it is a VM name and is required |
| `extra_args` | Passed to QEMU verbatim, for whatever vitro does not model |

`guest` decides how a VM is shut down and how `exec` quotes its arguments, so a
`guest` that contradicts the build is refused rather than obeyed: the two
disagreeing means one of them stops the guest the wrong way.

## `[images.<key>.build]`

Absent this section, the image is a golden one somebody else produced and
`build` has nothing to do.

| Setting | Meaning |
| --- | --- |
| `kind` | `cloud-image`, `unattended-install` or `lume-ipsw` |
| `source` | A URL, a path, or `latest` (lume only). Omit it for an unattended Windows install and vitro uses whatever is in its media directory |
| `sha256` | Checked after downloading. **Without it a cached file is reused unverified** |
| `disk_size` | How large the guest's disk is made |
| `provision` | A script run inside the guest after its first boot |
| `unattend` | Your own `autounattend.xml`, used whole instead of vitro's |
| `virtio_iso` | Path or URL for the virtio drivers. Offered as a download when unset |
| `unattended` | lume's own first-boot preset, such as `tahoe` |
| `build_timeout` | Ceiling for the whole build. Windows installs reboot several times and fetch OpenSSH before they answer |

A caller's own `unattend` file is used whole, so it — not vitro — decides the
account's password. vitro therefore stops reporting one: printing a password
that is not set would be worse than printing nothing.

## `[tools]`

Explicit paths for the programs vitro runs. Anything left out is looked up on
`PATH`, and `vitro doctor` reports which of the two each one came from.

```toml
[tools]
qemu | qemu_img | ssh | scp | ssh_keygen | lume
```

Naming a path that does not exist is an error that says which setting pointed
at it, rather than a lookup that quietly falls back to `PATH`.

## Where things end up

The same layout on every platform, XDG variables honoured where they are set to
an absolute path:

| | |
| --- | --- |
| `~/.config/vitro` | `config.toml`, the SSH key, the `ssh_config` include file |
| `~/.local/share/vitro` | Golden images |
| `~/.local/state/vitro` | Per-VM records, overlays, logs; build working directories |
| `~/.local/share/vitro/media` | Installation media you supply — the files vitro is not allowed to fetch |
| `~/.cache/vitro` | Installation media vitro downloaded. Safe to delete |

`VITRO_LOG` takes a `tracing` filter if you want the internals to talk.
