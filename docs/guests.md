# Guests

What each kind of guest needs, where to get it, and what vitro does with it.

Linux needs nothing downloaded by hand. Windows needs two files nobody is
allowed to redistribute. macOS needs a second tool and no files at all.

## Linux

A distribution's cloud image works as it is, and `source` is simply its URL.

```toml
[images.linux]
firmware = "…/share/qemu/edk2-aarch64-code.fd"   # aarch64 host; see below

[images.linux.build]
kind      = "cloud-image"
source    = "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-arm64.qcow2"
sha256    = "…"
disk_size = "20G"
```

That example is written for an Apple Silicon host, so it names the `arm64`
image and a firmware file. On an x86-64 host, `source` is the distribution's
`amd64` image instead, and **there is no `firmware` line at all** — vitro gives
x86-64 guests the `q35` machine, which boots from QEMU's own BIOS. `vitro
setup` writes whichever of the two suits the host it runs on.

`build` grows the disk, writes a small cloud-init seed volume carrying the user
and the SSH key, boots the image once so cloud-init applies them, and then
**disables cloud-init before flattening**. Left enabled, every later boot spends
about two minutes hunting for a datasource that `run` deliberately does not
provide.

Without `sha256` the download is still cached, but a cached file is reused
without being checked. vitro says so on the way past rather than pretending
otherwise.

## Windows

```toml
[images.windows]

[images.windows.build]
kind          = "unattended-install"
disk_size     = "80G"
build_timeout = "90m"
```

Neither example sets `golden`, which is not an omission: `build` writes a dated
file — so that rebuilding cannot pull the disk out from under a VM already
running on the old image — and with `golden` unset, `run` starts whichever of
those is newest. Setting it pins one instead, and then every build ends in
editing this file. `vitro ls` shows which image an entry currently resolves to.

### The installer ISO

No `source` above, because vitro keeps a directory for the installation media
it is not allowed to fetch:

```
~/.local/share/vitro/media
```

Drop the ISO in there under any name — vitro identifies it by the volume label
inside the image rather than by its filename, so whatever the browser called it
is fine. `source` still works if you would rather name a file, or a URL for a
mirror or an artifact store, and it takes precedence when it is set.

It has to match the architecture of the host, because vitro runs accelerated
same-architecture guests and nothing else. Arm machines have their own download
page, separate from the usual one — getting this wrong is expensive, since
Setup ignores an answer file whose architecture does not match its media in
exactly the way it ignores a missing one, and the install stops at its first
screen half an hour later.

- Arm: [Download Windows 11 for Arm-based PCs](https://www.microsoft.com/software-download/windows11arm64)
- x86-64: Microsoft's Windows 11 download page, "Download Windows 11 Disk Image (ISO)"

vitro reads the ISO's volume label before starting anything, so media for the
wrong architecture is refused in seconds rather than discovered half an hour
later. Microsoft names the architecture there — `CCCOMA_A64FRE_EN-US_DV9` is
Arm, `CCCOMA_X64FRE_EN-US_DV9` is x86-64 — and an image whose label says
nothing is allowed through rather than guessed at.

`source` takes a path or a URL. vitro will not fetch an ISO from Microsoft for
you: the licence is yours to accept, and that page issues links that expire
rather than a stable address anything could depend on. A URL you already have —
an internal mirror, an artifact store, a volume-licence or evaluation image — is
a different matter, and `build` downloads and caches it exactly as it does a
Linux cloud image.

Both downloads are multi-edition ISOs that use a product key to decide which
edition to install. vitro's answer file supplies the generic Windows 11 Pro key, which
selects that edition and activates nothing: the guest is a valid installation
but an unactivated one, and using Windows still needs a licence. Point
`unattend` at your own answer file to change the edition or to supply a key.

### The virtio drivers

The disk, network card and display vitro gives a guest are virtio devices,
which is the fast path — the alternatives are emulated hardware. **Windows
ships with no virtio drivers at all**, so Setup cannot see the disk it is
supposed to install onto and stops before it starts.

`virtio-win.iso` carries them. vitro lifts out the three it needs — storage,
network and display — and feeds them to Setup through the answer file. Every
file each driver's `.inf` refers to comes along, because picking individual
ones is how this first went wrong: `netkvm.inf` copies `netkvmp.exe` as well,
and `pnputil` reports only "cannot find the file specified" when one is
missing, half an hour into an install.

**You do not have to fetch it yourself.** With `virtio_iso` unset, `build`
offers to download the upstream image and caches it:

```console
$ vitro build windows
An unattended Windows install needs the virtio drivers, which Windows does not ship.
Download them from https://fedorapeople.org/…/stable-virtio/virtio-win.iso? [y/N]
```

Nothing is downloaded without an answer, and nothing is asked when there is
nobody to answer — driven by a script, the build stops and tells you to set
`virtio_iso` instead. That setting takes a path or a URL:

```toml
virtio_iso = "~/Downloads/virtio-win.iso"
```

It is built and signed by Red Hat, who publish it through Fedora's community
hosting; that is the upstream rather than a mirror. The Windows 10 and 11
drivers carry Microsoft attestation signatures. The stable channel is what
vitro defaults to, so a guest rebuilt next month gets the drivers it got this
month.

### What the install does

No interaction at all. vitro writes an answer file and a first-logon script to
a small FAT volume, attaches it alongside the ISO, and taps an arrow key at the
firmware's boot prompt — without which the installer never reaches its first
screen. The answer file bypasses the TPM, Secure Boot and RAM checks that the
QEMU `virt` machine cannot satisfy, installs to the second disk rather than the
seed, and hands over to a script that installs OpenSSH, authorises your key,
sets a permanent automatic logon and turns off the display and sleep timeouts.

The account's password is generated and printed once, at the end of the build.
Nothing else records it.

## macOS

```toml
[images.macos]
backend = "lume"
golden  = "tahoe-base"        # a VM name, not a path

[images.macos.build]
kind       = "lume-ipsw"
source     = "latest"
unattended = "tahoe"
```

vitro delegates entirely to [`lume`](https://github.com/trycua/cua): it asks
lume to install macOS from a restore image and to apply its own unattended
preset, then authorises your key and clones the result into a golden VM. There
is no answer file on vitro's side, and `source = "latest"` asks lume which
restore image is current and fetches it.

`golden` is a VM name here rather than a path, because that is how lume
addresses its VMs. Giving it a path is refused rather than guessed at.

A lume guest gets a longer default boot ceiling than a Linux one: it has to get
an address and then open port 22, and those are not the same moment.
