# vitro

Disposable development VMs driven from a CLI.

## How it is built

- **Test first.** Red → green → refactor. Write the failing test before the
  code that makes it pass.
- **Each layer holds one thing.** Put a fact in the one layer that owns it,
  and nowhere else.

  | Layer | Holds |
  | --- | --- |
  | `docs/concept.md` | what this is and for whom |
  | `docs/adr/` | one decision and why it was made |
  | `docs/spec/` | the behaviour users and other components can rely on |
  | Tests | the details that pin the behaviour down: rules, boundaries, tables |
  | Code | how it is done |
  | Commits | why a change was made |
  | Comments | why it is not done another way |

  Before writing, fixing or reviewing a record, read `docs/adr/README.md`;
  before writing, fixing or reviewing a spec, read `docs/spec/README.md`.
  Follow their writing order.
- **Dependencies are current.** Before adding or bumping one, look up its
  latest release in the registry. Do not copy a version from memory or from
  another repository.
- **Claim only what has run.** A host or guest that has only been compiled for
  is not described as working. README's host table says which is which; keep
  it true.

## Quality gate

Run `just ci` before reporting a change as done. CI runs the same recipe on
Linux, macOS and Windows.

## Layout

| Path | What lives there |
| --- | --- |
| `src/` | the single `vitro` crate: library modules and the binary |
| `src/commands/` | one module per subcommand |
| `tests/cli/` | end-to-end tests that drive the binary against fake QEMU and SSH binaries |
| `docs/` | concept, ADRs, specs, and the user guides (`configuration`, `guests`, `internals`, `troubleshooting`) |
| `packaging/` | what the release workflow renders for package managers |
| `spikes/` | untracked experiments; never built or shipped |

## Language

- README, docs and commit messages: English
- Code comments, log and error messages: English

## Commits

Scoped Commits:

```
<scope>: <description>

<why this change is needed>
```

- The scope names where the change is, from the table below. Several scopes
  are joined with `, `; a change across the whole tree uses `treewide`.
- The description starts in lower case, has no trailing period, and stays
  within 72 characters.
- The body explains why. The code shows how; the tests show what.
- A release commit is `release: vX.Y.Z`.
- A breaking change says so in the body and carries a `BREAKING CHANGE:`
  trailer.

Commits before this convention was adopted use Conventional Commits
(`feat:`, `fix(ci):`); they are left as they are.

| Scope | Covers |
| --- | --- |
| `cli` | `src/cli.rs`, `src/main.rs`, `src/commands.rs` |
| `<command>` | `src/commands/<command>.rs`, e.g. `run`, `exec`, `doctor` |
| `config` | `src/config.rs`, `src/units.rs`, `src/paths.rs`, `src/tools.rs` |
| `state` | `src/state.rs`, `src/process.rs`, `src/signals.rs` |
| `qemu` | `src/qemu.rs`, `src/qmp.rs` |
| `lume` | `src/lume.rs` |
| `project` | `src/project.rs`, `src/commands/test.rs` |
| `ssh` | `src/ssh.rs` |
| `sync` | `src/sync.rs` |
| `image` | `src/image.rs`, `src/golden.rs`, `src/iso.rs`, `src/media.rs`, `src/seed.rs`, `src/unattend.rs` |
| `test` | `tests/` |
| `docs` | `README.md`, `docs/*.md` |
| `adr` | `docs/adr/` |
| `spec` | `docs/spec/` |
| `ci` | `.github/` |
| `nix` | `flake.nix`, `flake.lock` |
| `packaging` | `packaging/` |
| `repo` | `.gitignore`, `.gitattributes`, `.envrc.sample` |
| `deps` | dependency updates |

Add a row when a new area appears.
