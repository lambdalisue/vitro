//! `vitro setup` — get from nothing to a guest you can build.
//!
//! `doctor` says what is wrong. That is the right answer once vitro is
//! installed and something has broken, and the wrong one on the first day:
//! knowing that `firmware` is not set does not help somebody who has never
//! heard of edk2. This does the parts vitro can do, and for the parts it
//! cannot — a Windows ISO nobody may redistribute, a package manager it should
//! not run for you — it says exactly what to go and do.
//!
//! Nothing here happens without consent, and consent is only asked for where
//! somebody can give it: driven by a script, this explains rather than waits.

use std::io::{BufRead, IsTerminal, Write};

use anyhow::{Context, Result};

use crate::commands::doctor::{self, Level};
use crate::{
    commands::{keygen, sshconfig},
    qemu, tools, Config, Paths,
};

/// Which guest the starting configuration is written for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Starter {
    /// The reason vitro exists, and so the default.
    Windows,
    /// Needs nothing downloaded by hand, which makes it the quickest way to
    /// find out whether the rest of the machine is in order.
    Linux,
    Macos,
}

pub fn setup(paths: &Paths, config: &Config) -> Result<i32> {
    let interactive = std::io::stdin().is_terminal();

    println!("vitro setup\n");

    ensure_key(paths, config)?;
    ensure_config(paths, interactive)?;
    ensure_include(paths, interactive);

    // Re-read rather than reuse: the configuration handed in was loaded before
    // this ran, so reporting against it says there are no images in a file that
    // now has one.
    let config = &reload(paths).unwrap_or_else(|_| config.clone());

    // Whatever is still missing, reported the way `doctor` reports it, because
    // a second opinion that disagrees with the first is worse than one report.
    let outstanding: Vec<doctor::Check> = doctor::run(paths, config)
        .into_iter()
        .filter(|check| check.level != Level::Ok)
        .collect();

    if outstanding.is_empty() {
        println!("\nEverything vitro needs is in place. `vitro build <image>` next.");
        return Ok(0);
    }

    println!("\nStill to do:\n");
    for check in &outstanding {
        println!("  {} — {}", check.name, check.detail);
        if let Some(fix) = &check.fix {
            println!("      {fix}");
        }
    }
    println!("\nRun `vitro doctor` again once those are dealt with.");
    Ok(0)
}

/// The configuration as it stands on disk right now.
fn reload(paths: &Paths) -> Result<Config> {
    let path = paths.config_file();
    if !path.exists() {
        return Ok(Config::default());
    }
    Config::load(&path, paths.home(), &paths.ssh_key())
}

/// The key is free to make, required by every guest, and destroys nothing when
/// it is already there, so it needs no question.
fn ensure_key(paths: &Paths, config: &Config) -> Result<()> {
    let generated = keygen::keygen(paths, config, false)?;
    let state = if generated.existed {
        "already there"
    } else {
        "created"
    };
    println!("  key       {} ({state})", generated.path.display());
    Ok(())
}

/// A starting configuration, for the guest the user says they want.
///
/// Writing a file into somebody's config directory is not something to do
/// quietly, so it is asked for — and skipped rather than guessed at when there
/// is nobody to ask.
fn ensure_config(paths: &Paths, interactive: bool) -> Result<()> {
    let path = paths.config_file();
    if path.exists() {
        println!("  config    {} (already there)", path.display());
        return Ok(());
    }

    let firmware = firmware_for_host();
    println!("  config    {} is missing", path.display());

    if !interactive {
        // Windows, because that is what somebody reaching for vitro came for.
        let starter = starter_config(Starter::Windows, &firmware, None);
        println!("\nA starting point for it:\n");
        for line in starter.lines() {
            println!("    {line}");
        }
        println!("\n{}", windows_iso_instructions(paths));
        return Ok(());
    }

    let starter = match ask_starter()? {
        Some(Starter::Windows) => {
            println!("\n{}\n", windows_iso_instructions(paths));
            // Blank is the expected answer: the ISO usually is not downloaded
            // yet, and vitro finds it in its own directory afterwards without
            // anybody naming a path.
            let iso = ask("            path or URL, or blank to use that directory: ")?
                .filter(|value| !value.is_empty());
            starter_config(Starter::Windows, &firmware, iso.as_deref())
        }
        Some(other) => starter_config(other, &firmware, None),
        None => {
            println!("            left alone");
            return Ok(());
        }
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    std::fs::write(&path, &starter).with_context(|| format!("cannot write {}", path.display()))?;
    println!("            wrote {}", path.display());
    Ok(())
}

/// Make `scp`, `rsync`, an editor's remote connection and anything else that
/// reads `ssh_config` able to say `vitro-<vm>`.
///
/// Asked for rather than done, because it is the user's own `~/.ssh/config`
/// and every SSH connection they make reads it. Declining leaves everything
/// working — `vitro exec` and `vitro ssh` never needed it.
fn ensure_include(paths: &Paths, interactive: bool) {
    if sshconfig::is_included(paths) {
        println!(
            "  ssh config  {} includes vitro's",
            sshconfig::user_config(paths).display()
        );
        return;
    }
    if !interactive {
        println!(
            "  ssh config  `vitro ssh-config --write` publishes an alias per VM for scp and rsync"
        );
        return;
    }
    match ask("  ssh config  let scp and rsync reach VMs by name? [Y/n] ") {
        Ok(Some(answer)) if matches!(answer.to_ascii_lowercase().as_str(), "n" | "no") => {
            println!("              left alone; `vitro ssh-config --write` does it later");
            return;
        }
        Ok(Some(_)) => {}
        // No answer at all: the safe reading of silence is "do not touch my
        // SSH configuration".
        _ => return,
    }
    let written = sshconfig::write(paths, "").and_then(|_| sshconfig::add_include(paths));
    match written {
        Ok(path) => println!("              added the include to {}", path.display()),
        // Not fatal: nothing else setup did depends on it.
        Err(e) => println!("              could not update it ({e:#})"),
    }
}

fn ask_starter() -> Result<Option<Starter>> {
    let answer = ask("            which guest first? [Windows/linux/macos/skip] ")?;
    Ok(
        match answer.unwrap_or_default().to_ascii_lowercase().as_str() {
            // Empty means the default, which is the point of having one.
            "" | "w" | "windows" => Some(Starter::Windows),
            "l" | "linux" => Some(Starter::Linux),
            "m" | "mac" | "macos" => Some(Starter::Macos),
            _ => None,
        },
    )
}

fn ask(prompt: &str) -> Result<Option<String>> {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    if std::io::stdin().lock().read_line(&mut answer)? == 0 {
        return Ok(None);
    }
    Ok(Some(answer.trim().to_string()))
}

/// The same steps `build` and `doctor` give, so one task reads as one task.
///
/// The directory is created rather than only named: a browser's save dialog
/// cannot be pointed at somewhere that does not exist, and "make this
/// directory, then save into it" is two instructions where one will do.
fn windows_iso_instructions(paths: &Paths) -> String {
    let dir = paths.media_dir();
    let _ = std::fs::create_dir_all(&dir);
    format!(
        "{}\n\nThe virtio drivers Windows also needs are a separate download, and that \
         one\n`build` does fetch — after asking.",
        crate::media::windows_media_instructions(&dir, std::env::consts::ARCH)
    )
}

/// What the starting configuration should say about `firmware`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Firmware {
    /// This host's machine has a legacy BIOS and starts without a firmware
    /// image, so the setting would do nothing. x86-64 hosts land here.
    NotNeeded,
    Found(String),
    /// Needed, but not where QEMU usually keeps it. The file is named because
    /// "the edk2 code file for this architecture" is not something a newcomer
    /// can act on, and the name differs per architecture.
    Missing {
        file: String,
    },
}

/// The UEFI image that ships inside the QEMU distribution, if this host's
/// machine needs one and QEMU is here to ship it.
fn firmware_for_host() -> Firmware {
    let Ok(host) = qemu::HostTarget::detect() else {
        return Firmware::NotNeeded;
    };
    if !host.requires_firmware() {
        return Firmware::NotNeeded;
    }
    let file = format!("edk2-{}-code.fd", std::env::consts::ARCH);
    let found = tools::resolve(&host.binary, None, "tools.qemu")
        .ok()
        .and_then(|found| {
            let prefix = found.path.parent()?.parent()?;
            let candidate = prefix.join("share").join("qemu").join(&file);
            candidate.exists().then(|| candidate.display().to_string())
        });
    match found {
        Some(path) => Firmware::Found(path),
        None => Firmware::Missing { file },
    }
}

fn starter_config(starter: Starter, firmware: &Firmware, windows_iso: Option<&str>) -> String {
    // A lume guest never touches QEMU, so naming a firmware there would be a
    // setting that does nothing.
    let firmware_line = match (starter, firmware) {
        (Starter::Macos, _) | (_, Firmware::NotNeeded) => String::new(),
        (_, Firmware::Found(path)) => format!("firmware = \"{path}\"\n"),
        // Present and commented rather than absent: a setting that is simply
        // missing is harder to discover than one staring back at you.
        (_, Firmware::Missing { file }) => {
            format!("# firmware = \"…/share/qemu/{file}\"  # `vitro doctor` finds it\n")
        }
    };
    // No `golden` either: unset means "whatever `build` last made", so `build`
    // and then `run` works with nothing edited in between. Naming a file here
    // would not, because a built image is dated.
    match starter {
        Starter::Windows => format!(
            "[defaults]\n\
             ssh_user = \"dev\"\n\
             cpus     = 6\n\
             memory   = \"8G\"\n\
             {firmware_line}\
             \n\
             # No `golden`: `run` starts whatever `build` last made for this image.\n\
             [images.windows]\n\
             \n\
             [images.windows.build]\n\
             kind          = \"unattended-install\"\n\
             {}\
             disk_size     = \"80G\"\n\
             build_timeout = \"90m\"\n",
            // No `source` at all is the better default: it means "whatever is
            // in vitro's media directory", so dropping an ISO there is the
            // whole of the remaining work. A placeholder path would have to be
            // edited, and a wrong one looks like a configured setting.
            match windows_iso {
                Some(iso) => format!("source        = \"{iso}\"\n"),
                None => String::new(),
            },
        ),
        Starter::Linux => {
            let source = match std::env::consts::ARCH {
                "aarch64" => "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-arm64.qcow2",
                _ => "https://cloud.debian.org/images/cloud/bookworm/latest/debian-12-genericcloud-amd64.qcow2",
            };
            format!(
                "[defaults]\n\
                 ssh_user = \"dev\"\n\
                 cpus     = 4\n\
                 memory   = \"4G\"\n\
                 {firmware_line}\
                 \n\
                 # No `golden`: `run` starts whatever `build` last made for this image.\n\
                 [images.linux]\n\
                 \n\
                 [images.linux.build]\n\
                 kind      = \"cloud-image\"\n\
                 source    = \"{source}\"\n\
                 disk_size = \"20G\"\n"
            )
        }
        Starter::Macos => "[defaults]\n\
             ssh_user = \"dev\"\n\
             cpus     = 4\n\
             memory   = \"8G\"\n\
             \n\
             # `golden` is a VM name here rather than a path: lume addresses its\n\
             # VMs by name, and vitro delegates macOS to it entirely.\n\
             [images.macos]\n\
             backend = \"lume\"\n\
             golden  = \"tahoe-base\"\n\
             \n\
             [images.macos.build]\n\
             kind       = \"lume-ipsw\"\n\
             source     = \"latest\"\n\
             unattended = \"tahoe\"\n"
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whatever vitro writes has to parse, or the first thing a newcomer does
    /// is debug a file they did not write.
    fn parses(starter: &str, paths: &Paths, home: &std::path::Path) -> Config {
        Config::parse(starter, home, &paths.ssh_key())
            .unwrap_or_else(|e| panic!("starter did not parse: {e:#}\n{starter}"))
    }

    #[test]
    fn the_default_starter_is_the_guest_vitro_exists_for() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        let starter = starter_config(
            Starter::Windows,
            &Firmware::Found("/q/share/qemu/edk2-aarch64-code.fd".into()),
            Some("/iso/win.iso"),
        );

        let config = parses(&starter, &paths, temp.path());
        assert!(config.image("windows").is_ok(), "{starter}");
        assert!(starter.contains("unattended-install"), "{starter}");
        assert!(starter.contains("/iso/win.iso"), "{starter}");
    }

    #[test]
    fn a_windows_starter_without_an_iso_still_parses() {
        // Somebody who has not downloaded it yet gets a file they can edit, not
        // a syntax error and no idea what was meant to go there.
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        let starter = starter_config(
            Starter::Windows,
            &Firmware::Missing {
                file: "edk2-aarch64-code.fd".into(),
            },
            None,
        );

        parses(&starter, &paths, temp.path());
        assert!(starter.contains("# firmware"), "{starter}");
    }

    #[test]
    fn the_commented_firmware_names_this_architecture_s_file() {
        // Writing `edk2-aarch64-code.fd` on every host sent people looking for
        // a file their QEMU never shipped.
        let starter = starter_config(
            Starter::Linux,
            &Firmware::Missing {
                file: "edk2-riscv64-code.fd".into(),
            },
            None,
        );

        assert!(starter.contains("edk2-riscv64-code.fd"), "{starter}");
        assert!(!starter.contains("aarch64"), "{starter}");
    }

    #[test]
    fn a_machine_that_boots_from_a_bios_is_told_nothing_about_firmware() {
        // q35 — every x86-64 host — starts without one, so even a commented
        // line is a setting to go and investigate for no reason.
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        for starter in [Starter::Windows, Starter::Linux] {
            let text = starter_config(starter, &Firmware::NotNeeded, None);
            parses(&text, &paths, temp.path());
            assert!(!text.contains("firmware"), "{text}");
        }
    }

    #[test]
    fn a_starter_never_names_a_golden_image_it_would_have_to_be_repointed_at() {
        // `build` writes a dated file, so a `golden` here is a path that is
        // wrong the moment the first build finishes — and the whole first hour
        // with vitro then hinges on noticing that.
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        for starter in [Starter::Windows, Starter::Linux] {
            let text = starter_config(starter, &Firmware::NotNeeded, None);
            parses(&text, &paths, temp.path());
            assert!(
                !text
                    .lines()
                    .any(|line| line.trim_start().starts_with("golden")),
                "{text}"
            );
        }
    }

    #[test]
    fn every_starter_parses() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        for starter in [Starter::Windows, Starter::Linux, Starter::Macos] {
            let text = starter_config(starter, &Firmware::Found("/q/edk2.fd".into()), None);
            parses(&text, &paths, temp.path());
        }
    }

    #[test]
    fn a_lume_guest_is_not_given_a_firmware_that_does_nothing() {
        let starter = starter_config(Starter::Macos, &Firmware::Found("/q/edk2.fd".into()), None);

        assert!(!starter.contains("firmware"), "{starter}");
    }

    #[test]
    fn the_instructions_create_the_directory_they_tell_you_to_save_into() {
        // A browser's save dialog cannot be pointed at somewhere that does not
        // exist, so naming it without making it is half an instruction.
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        let text = windows_iso_instructions(&paths);

        assert!(paths.media_dir().is_dir(), "{text}");
        assert!(
            text.contains(&paths.media_dir().display().to_string()),
            "{text}"
        );
    }

    #[test]
    fn an_existing_configuration_is_never_written_over() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        std::fs::create_dir_all(paths.config_dir()).unwrap();
        std::fs::write(paths.config_file(), "# mine\n").unwrap();

        ensure_config(&paths, false).unwrap();

        assert_eq!(
            std::fs::read_to_string(paths.config_file()).unwrap(),
            "# mine\n"
        );
    }

    #[test]
    fn nothing_is_written_when_there_is_nobody_to_ask() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        ensure_config(&paths, false).unwrap();

        assert!(
            !paths.config_file().exists(),
            "a non-interactive run put a file in somebody's config directory"
        );
    }
}
