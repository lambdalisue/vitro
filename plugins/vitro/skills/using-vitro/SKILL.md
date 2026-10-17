---
name: using-vitro
description: Run commands, builds and tests inside disposable Windows, macOS or Linux VMs with the vitro CLI. Use when a question can only be answered on another operating system ("does this build on Windows?", "do the tests pass on macOS?"), when something should be done to a throwaway machine rather than the host, or when the user mentions vitro, a golden image, or a guest VM.
allowed-tools: Bash(vitro *)
---

# Driving guests with vitro

vitro builds a golden image of an operating system once, then starts
throwaway VMs from it in seconds. A VM is a copy-on-write overlay: destroying
it costs nothing, and nothing done inside it reaches the host.

## Start by asking vitro

```console
$ vitro status --json
```

It lists running VMs, the images that can be run or built, and a `next` array
of commands worth running now. Prefer it to guessing image names. When a
command fails for a reason outside vitro (QEMU missing, no key, no firmware),
`vitro doctor` names the fix.

## The one-shot form — use this by default

To answer one question on a fresh machine, start a VM, run the command, and
destroy the VM in one step:

```console
$ vitro run --rm --sync . windows -- cargo test
```

- The exit code is the guest command's own, so failure means the command
  failed in the guest.
- `--sync <dir>` copies the directory in first (minus what `.gitignore`
  excludes) and runs the command inside it.
- `--rm` destroys the VM afterwards, even when the command fails or is
  interrupted.
- stdout carries only the guest command's output; vitro's own messages go to
  stderr.

If the project has a `vitro.toml`, `vitro test` does the same for every image
it lists and prints one verdict per image. `vitro test windows -- --nocapture`
restricts it to one image and appends arguments.

## Keeping a VM across several commands

```console
$ vitro run windows                    # prints the VM's name
$ vitro exec windows cargo build       # the image name reaches its one VM
$ vitro exec --sync . --cwd app windows cargo test
$ vitro cp :app/target/debug/app.exe . # ":" alone means the only running VM
$ vitro destroy windows
```

- A VM can be named by its full name, by its image, or by a unique prefix.
- While only one VM is running, the name can be left out:
  `vitro exec cargo test`.
- `--cwd` and `--env NAME=VALUE` are written in the guest shell's syntax. On a
  Windows guest the command itself runs in PowerShell, so write it in
  PowerShell (`Get-ChildItem`, `$env:PATH`), not in POSIX sh.
- Always destroy what you started. `vitro status` flags a VM that has been up
  for more than a day.

## Things to avoid

- Do not run `vitro build` unless asked: it takes from seconds (Linux) to
  tens of minutes (Windows) and may need an installer the user must download.
- Do not use `destroy --all` unless every VM is yours to destroy.
- Do not pass `--graceful` to `destroy`; the default forced stop is intended.
- GUI commands (`screenshot`, `key`, `click`, `launch`) are for checking
  something on the guest's screen; prefer `exec` for anything a shell can do.
