# Troubleshooting

`vitro doctor` first. It checks every prerequisite `run` and `build` depend on,
reports what it found rather than only what is broken — knowing which
`qemu-system-*` was picked up, and from where, settles most questions on its
own — and prints a `→` line with the exact fix for anything that is wrong.

The rest of this page is the failures that are not failures.

## `run` says the image is missing, and `build` just made one

A built image carries the date it was built:

```
built …/golden/linux-20260920-160437.qcow2 in 16s
```

so it is never the file a pinned `golden` names. Leave `golden` out and `run`
starts whichever build is newest, which is why the starter configuration does
not set it. With a path there instead, that path is what `run` uses, and it has
to be repointed after every build — `build` prints where to point it, and says
nothing when there is nothing to do.

The dating itself is deliberate: it is what lets the image a VM is already
running on stay where it is until the new one has proved itself.

## The guest is up but `run` waits, then says it never answered

vitro authorises one key — `~/.config/vitro/id_ed25519` — and a golden image
carries whichever key was there when it was built. Present a different one and
sshd answers and turns it down, which from outside looks exactly like a guest
that has not finished booting.

```
dev@127.0.0.1 is up but will not accept …/id_ed25519;
the golden image was probably built with a different key
```

vitro gives a refusal a minute to sort itself out — during a Windows build,
sshd starts a moment before the key is installed — and then says so rather than
spending the whole `boot_timeout` on a state that cannot change. Either point
`ssh_key` at the key the image was built with, or rebuild the image.

## The screenshot is black

**A black screenshot does not mean the program failed.** A guest that has
switched its display off still reports a framebuffer, so the capture succeeds
and returns a black rectangle while the application runs perfectly behind it.

`screenshot` says so when the picture it took is a single colour. Wake the
guest and take another. The golden images vitro builds for Windows disable the
timeouts that cause this; a golden image from somewhere else has to do it
itself.

## `launch` starts nothing, or the window never appears

`launch` needs somebody logged in at a desktop. A process started by sshd on
Windows lands in session 0, which has no desktop; one started without `DISPLAY`
on Linux has no screen to draw on. vitro's Windows images set a permanent
automatic logon for this reason. On Linux the golden image has to provide the
desktop — an X server and a window manager are necessary but not sufficient,
and a kernel without the virtio-gpu DRM driver produces a black screen no
matter what else is right.

## A Windows install sits at its first screen

Almost always the ISO's architecture does not match the host's. Setup ignores
an answer file whose architecture does not match its media in exactly the way
it ignores a missing one, so the install proceeds interactively — which, with
nobody watching, looks like a hang. See [guests.md](guests.md) for which
download page to use.

`--keep-failed` keeps the build's working directory so the serial log and the
disk survive, and `screenshot` works against a build that is still running.

## `vitro exec` runs the wrong thing

Arguments are passed through as separate words. A pipeline or a redirect
written as one string becomes a command *named* that string:

```console
$ vitro exec vm 'ls | wc -l'        # a command called "ls | wc -l"
$ vitro exec vm sh -c 'ls | wc -l'  # what you meant
```

A Windows guest receives the command verbatim in PowerShell instead, so
operators work directly there and the quoting is yours to get right.

## Something else is using the state directory

Two vitro processes will not write the same state directory at once, and the
second says so. If nothing else is running, a previous process died holding the
lock; it is released when that process goes, so there is nothing to clean up by
hand.

## A VM is listed that no longer exists

`ls --prune` removes records whose process is gone. A record is trusted only as
far as the process it names still exists *and* still has the start time it had
when the record was written, so a recycled PID does not resurrect a dead VM.
