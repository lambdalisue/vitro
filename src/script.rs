//! Running a script from the host inside a guest.
//!
//! A command that needs a pipeline, a loop or a hashtable does not survive the
//! trip through ssh into PowerShell intact, so the script travels as a file
//! instead: copied into the guest's home under a name of its own, run there,
//! and removed whatever its exit code.

use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::{ssh, Guest};

/// How a Windows guest runs a script, chosen by its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsRunner {
    PowerShell,
    Cmd,
}

/// The extensions a Windows guest can run, as the error names them.
const WINDOWS_EXTENSIONS: &str = ".ps1, .cmd or .bat";

fn windows_runner(script: &Path) -> Option<WindowsRunner> {
    let ext = script.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "ps1" => Some(WindowsRunner::PowerShell),
        "cmd" | "bat" => Some(WindowsRunner::Cmd),
        _ => None,
    }
}

/// Refuse a script the guest would have no way to run, before anything is
/// copied or booted. A Unix guest runs anything: a file without a `#!` line is
/// run by `sh`.
pub fn check(script: &Path, guest: Guest) -> Result<()> {
    if !script.is_file() {
        bail!("script {} does not exist", script.display());
    }
    if guest == Guest::Windows && windows_runner(script).is_none() {
        bail!(
            "a Windows guest runs a script by its extension, and {} has none it knows; \
             use {WINDOWS_EXTENSIONS}",
            script.display()
        );
    }
    Ok(())
}

/// The script's name in the guest's home: unique, hidden, and keeping the
/// extension Windows chooses the interpreter by.
fn remote_name(script: &Path) -> String {
    let tag: u32 = rand::random();
    match script.extension().and_then(|ext| ext.to_str()) {
        Some(ext) => format!(".vitro-script-{tag:08x}.{ext}"),
        None => format!(".vitro-script-{tag:08x}"),
    }
}

/// The command that runs `remote` from the guest's home with `args`, removes
/// it, and exits with the script's own code.
///
/// The home is named through the shell's own variable rather than as a
/// relative path, so `--cwd` can move the command elsewhere and the script is
/// still found.
fn runner(guest: Guest, script: &Path, remote: &str, args: &[String]) -> Vec<String> {
    match guest {
        Guest::Unix => {
            // Arguments travel as "$@" so each keeps its boundaries; the
            // script's own name stands in for $0.
            let mut words = vec![
                "sh".to_string(),
                "-c".to_string(),
                format!(
                    "f=\"$HOME/{remote}\"; chmod +x \"$f\" && \"$f\" \"$@\"; rc=$?; rm -f \"$f\"; exit $rc"
                ),
                remote.to_string(),
            ];
            words.extend(args.iter().cloned());
            words
        }
        Guest::Windows => {
            let call = match windows_runner(script) {
                Some(WindowsRunner::Cmd) => "& cmd /c $f",
                _ => "& powershell -NoProfile -ExecutionPolicy Bypass -File $f",
            };
            let mut line = format!("$f = Join-Path $HOME '{remote}'; {call}");
            for arg in args {
                line.push(' ');
                line.push_str(arg);
            }
            line.push_str(
                "; $rc = $LASTEXITCODE; Remove-Item -LiteralPath $f -ErrorAction SilentlyContinue; exit $rc",
            );
            vec![line]
        }
    }
}

/// Copy `script` into the guest, run it with `args`, and return its exit code.
pub fn run(
    ssh_binary: &Path,
    scp_binary: &Path,
    target: &ssh::SshTarget,
    guest: Guest,
    script: &Path,
    args: &[String],
    options: ssh::SshOptions,
) -> Result<i32> {
    check(script, guest)?;
    let remote = remote_name(script);

    let status = std::process::Command::new(scp_binary)
        .args(ssh::scp_args(target, script, &remote))
        .status()
        .with_context(|| format!("cannot run {}", scp_binary.display()))?;
    if !status.success() {
        bail!("could not copy {} into the guest", script.display());
    }

    ssh::run_passthrough(
        ssh_binary,
        target,
        &ssh::SshOptions {
            command: runner(guest, script, &remote, args),
            quoting: guest.into(),
            ..options
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(words: &[&str]) -> Vec<String> {
        words.iter().map(|w| w.to_string()).collect()
    }

    #[test]
    fn a_unix_guest_runs_the_script_with_its_arguments_intact_and_removes_it() {
        let words = runner(
            Guest::Unix,
            Path::new("setup.sh"),
            ".vitro-script-0000abcd.sh",
            &strings(&["--name", "two words"]),
        );

        assert_eq!(words[0..2], ["sh", "-c"]);
        assert!(
            words[2].contains("\"$HOME/.vitro-script-0000abcd.sh\""),
            "{}",
            words[2]
        );
        assert!(words[2].contains("\"$f\" \"$@\""), "{}", words[2]);
        assert!(words[2].contains("rm -f \"$f\""), "{}", words[2]);
        assert!(words[2].ends_with("exit $rc"), "{}", words[2]);
        assert_eq!(words[4..], ["--name", "two words"]);
    }

    #[test]
    fn a_powershell_script_runs_without_the_execution_policy_in_the_way() {
        let words = runner(
            Guest::Windows,
            Path::new("Setup.PS1"),
            ".vitro-script-0000abcd.PS1",
            &strings(&["-Name", "x"]),
        );

        assert_eq!(
            words,
            ["$f = Join-Path $HOME '.vitro-script-0000abcd.PS1'; \
              & powershell -NoProfile -ExecutionPolicy Bypass -File $f -Name x; \
              $rc = $LASTEXITCODE; Remove-Item -LiteralPath $f -ErrorAction SilentlyContinue; exit $rc"]
        );
    }

    #[test]
    fn a_batch_file_runs_under_cmd() {
        let words = runner(
            Guest::Windows,
            Path::new("build.bat"),
            ".vitro-script-1.bat",
            &[],
        );

        assert!(words[0].contains("& cmd /c $f;"), "{}", words[0]);
    }

    #[test]
    fn the_guest_copy_keeps_the_extension_and_never_collides() {
        let first = remote_name(Path::new("dir/setup.ps1"));
        let second = remote_name(Path::new("dir/setup.ps1"));

        assert!(first.starts_with(".vitro-script-"), "{first}");
        assert!(first.ends_with(".ps1"), "{first}");
        assert_ne!(first, second);
    }

    #[test]
    fn a_script_without_an_extension_gets_none_in_the_guest() {
        let name = remote_name(Path::new("provision"));

        assert!(name.starts_with(".vitro-script-"), "{name}");
        assert_eq!(name.matches('.').count(), 1, "{name}");
    }

    #[test]
    fn a_windows_guest_refuses_a_script_it_has_no_interpreter_for_before_copying() {
        let temp = tempfile::tempdir().unwrap();
        let sh = temp.path().join("setup.sh");
        std::fs::write(&sh, "echo hi").unwrap();

        let err = check(&sh, Guest::Windows).unwrap_err().to_string();

        assert!(err.contains(".ps1"), "{err}");
        assert!(check(&sh, Guest::Unix).is_ok());
    }

    #[test]
    fn a_missing_script_is_refused() {
        assert!(check(Path::new("/nonexistent/setup.ps1"), Guest::Windows).is_err());
    }
}
