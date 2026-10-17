# The project file

A project says which guests it is tested on, and how, in a `vitro.toml` kept
in the repository. `vitro test` reads it.

## Finding it

`vitro test` uses the nearest `vitro.toml` in the current directory or any
directory above it, so it works from anywhere inside the project. Paths in the
file are relative to the directory holding it.

## Format

```toml
# The images to test on when `vitro test` names none, in order. Each is an
# [images.*] key from the machine's own config.toml.
images = ["windows", "macos"]

# The command, one word per element, as `vitro exec` takes it. Every image
# runs it unless its own section below names another.
command = ["cargo", "test", "--workspace"]

# The directory sent to the guest before the command, which then runs inside
# it. `true`, or leaving it out, sends the directory holding vitro.toml; a path
# sends that directory instead; `false` sends nothing.
sync = true

# What each VM gets instead of its image's settings.
cpus = 8
memory = "16G"

# Variables set for the command.
[env]
RUST_BACKTRACE = "1"

# What differs for one image, by its key: its own command, which replaces the
# shared one, and variables laid over the shared ones.
[image.windows]
command = ["cargo", "test", "--workspace", "--features", "tsf"]
env = { RUST_LOG = "debug" }
```

| Key | Type | Default |
| --- | --- | --- |
| `images` | array of strings | none; images must then be named on the command line |
| `command` | non-empty array of strings | none; then every image needs its own |
| `sync` | boolean or path | `true` |
| `cpus` | integer, at least 1 | the image's |
| `memory` | size such as `16G` or `512M` | the image's |
| `env` | table of strings | none |
| `image.<key>.command` | non-empty array of strings | the shared `command` |
| `image.<key>.env` | table of strings | none |

Every image in `images` has to end up with a command, from its own section or
the shared one. A key vitro does not know is an error, as is a variable name
that is not letters, digits and `_`, not starting with a digit.

## What `vitro test` does

`vitro test [<image>…] [-- <args>…]` runs on the images named, or on the
project's `images` when none is. Every image is checked against the
configuration, and for a command, before any VM starts.

For each image in turn, it does what `vitro run --rm` does with the project's
settings: start a fresh VM, send the project, run the command with `<args>`
appended, and destroy the VM. A failure on one image does not stop the
others. It ends with one line per image saying whether the command passed,
failed with which exit code, or never ran because the VM did not start or the
project could not be sent.

With one image it exits with that command's exit code. With several it exits
with the first failure's code, `1` for a command that never ran, or `0` when
every image passed.
