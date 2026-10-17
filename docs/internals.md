# How it works

Not required reading. It is here because most of what follows was learned by
something failing in a way that looked like something else, and those are worth
writing down.

## Two commands, not one

```
build    installation media ──(once, minutes)──> golden image
run      golden image ──(copy-on-write overlay, seconds)──> VM
```

A VM is only disposable if getting another one back is cheap. Splitting the
expensive half off and doing it once is what makes `destroy` something you
reach for without thinking.

`run` creates a qcow2 overlay backed by the golden image, so starting a VM
writes almost nothing and destroying one deletes almost nothing. `promote`
flattens an overlay back into a standalone image with `qemu-img convert` —
never `commit`, which would write the VM's changes into the image it was
supposed to leave alone.

## Waiting for a guest

**A forwarded TCP port accepts before the guest exists.** QEMU's user-mode
networking completes the handshake on a `hostfwd` port whether or not anything
is listening inside, so a readiness check that connects to the port reports
success about fourteen seconds early — and reports a guest that never boots as
ready forever.

vitro waits on an SSH handshake instead, running a command that survives being
quoted. That is less obvious than it sounds: the probe is the single word
`exit`, because `exit 0` arrives at the far end as a command *named* `exit 0`,
and `true` does not exist on a Windows guest at all.

While it waits it also watches the QEMU process. A guest that dies is reported
as dead immediately rather than at the end of the timeout.

## Stopping a guest

Cleanly, and differently per guest: `shutdown /s` on Windows, `shutdown -h` on
a Unix one, with QMP's `system_powerdown` as the fallback and a kill after
that. `promote` depends on this — flattening a disk whose filesystem is still
running produces an image that boots into a repair.

QMP is Unix-socket only. A Windows host would need the named-pipe transport,
which vitro does not implement; there a guest is stopped over SSH instead.

## The monitor, and the screen

`screenshot` asks QEMU rather than the guest, so it works even when the guest
is wedged at a firmware prompt with no network. Two things about QMP are easy
to get wrong: there is no `sendkey` command — that name belongs to the human
monitor, and QMP spells it `input-send-event` with an explicit press and
release per key — and a press and release batched into one event arrives as a
single HID report, which a guest UI ignores.

`click`, `scroll`, `hover` and `drag` place the pointer on QEMU's tablet, whose
axes span the screen end to end rather than counting pixels, so each first
takes a capture to learn the screen's size. QEMU treats a wheel notch as a
button press, and the tablet reports only a vertical wheel: QEMU accepts
`wheel-left` and `wheel-right` and the guest never sees them, and virtio's
tablet drops them the same way. `scroll` therefore offers only `up` and
`down`, and a sideways scroll is `--with shift`, which most toolkits read as
one. Keys held with `--with` come up again even when the pointer action
fails, since a modifier left down would change everything typed afterwards. A drag passes through points between its
ends with the button held, because a press here and a release there with
nothing between reads to many toolkits as a click.

A lume guest has no QEMU monitor. Its screen and keyboard are reached through
the VNC server Virtualization.framework offers, at an address and password
lume reports afresh each time the guest starts. Keys sent that way arrive as a
keyboard's would, so macOS asks for no Accessibility permission — which input
synthesised from inside the guest would need, and which only a click in System
Settings can grant.

## Seed volumes

Both cloud-init and Windows Setup read their configuration off a small volume
attached at boot. vitro writes FAT images itself rather than shelling out, so
there is no dependency on `mkisofs` or `genisoimage`.

Reading in the other direction, the virtio drivers come out of an ISO9660 image
with a minimal reader written for the purpose. It bounds every length against
the file's size before allocating, because a truncated ISO should be an error
rather than an allocation.

## Records, and PIDs that come back

Every VM has a record naming the process that runs it. A PID on its own is a
reusable number, so the record stores the process's start time as well: a
recycled PID will not have the same one, and acting on a stale record means
signalling whatever inherited it.

That is also how a lume guest is tracked. lume detaches and reports no PID, so
vitro finds the process by its command line — matching whole arguments rather
than a substring, because a VM called `mac` would otherwise match the process
running `mac-test`.

## What has actually been run

The bugs that mattered in this project were all found by running it, and none
of them by the test suite:

- A golden image that came up at a logon screen nobody could get past. The
  answer file's `AutoLogon` spends its count on the install's first boot, and
  Winlogon then deletes the password and clears the flag the setup script had
  written — so the permanent form has to remove the count, not just set the
  values.
- A screenshot that photographed a display the guest had switched off. The
  framebuffer is still reported, so the capture succeeds and returns a black
  rectangle indistinguishable from a GUI that never drew.
- A `doctor` that hung. It asked OpenSSH for `--version`, which is not a flag
  it has; on Unix that exits with a usage message, and on Windows it never
  returns.

Hence the table in the README, and hence its bluntness:

| Host | State |
| --- | --- |
| macOS, aarch64 | Exercised end to end, all three guests |
| Windows | Builds, lints, passes its tests. No VM has ever been started |
| Linux | CI runs the same checks. No VM has ever been started |

The Windows unattended install is verified on aarch64. The x86-64 path is
parameterised and reachable and has never been run — the answer file names the
architecture, the driver directories name it, and both are derived from the
host, but nobody has yet watched an x86-64 install finish.
