//! `vitro build` — turn installation media into a golden image.
//!
//! This is the slow command: minutes for a cloud image, tens of minutes for an
//! unattended Windows install. Everything happens inside a build directory
//! that is thrown away unless the whole thing succeeds, because a half-built
//! image that `run` can find is worse than no image at all.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use time::OffsetDateTime;

use crate::{
    golden, image, iso, lume, media, qemu, qmp, seed, ssh, tools, unattend, Build, BuildKind,
    ByteSize, Config, Golden, Guest, Image, Paths, ProcessProbe, Source, SysinfoProbe,
};

const DISK_FILE: &str = "disk.qcow2";
const VARSTORE_FILE: &str = "efi-vars.fd";
const SERIAL_LOG: &str = "serial.log";
const QEMU_LOG: &str = "qemu.log";
const PIDFILE: &str = "qemu.pid";
const KNOWN_HOSTS: &str = "known_hosts";

/// A cloud image boots, gets configured by cloud-init and answers SSH in well
/// under a minute; the generous ceiling is for a first boot that has to grow a
/// filesystem or install packages from a `provision` script.
const CLOUD_IMAGE_BOOT_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// A Windows install with nowhere to put the operating system is not worth
/// starting, and 64 GiB is the smallest size that leaves room to work in.
const DEFAULT_WINDOWS_DISK: ByteSize = ByteSize::from_gib(64);

/// The drivers vitro lifts off the virtio-win image. Storage is needed by Setup
/// itself, the network by the script that fetches OpenSSH, and the display by
/// anything with a window.
const VIRTIO_DRIVERS: [&str; 3] = ["viostor", "NetKVM", "viogpudo"];

/// Where those drivers sit on the image, in the layout it uses.
///
/// The architecture has to match the media: a driver from the wrong directory
/// is one Setup cannot load, and `pnputil` reports only "cannot find the file
/// specified" — half an hour into an install.
fn virtio_driver_paths(arch: unattend::Arch) -> Vec<String> {
    VIRTIO_DRIVERS
        .iter()
        .map(|driver| format!("{driver}/w11/{}", arch.virtio_directory()))
        .collect()
}

/// Where the virtio drivers come from when the configuration does not say.
///
/// Red Hat builds and signs these and publishes them here; this is the
/// upstream rather than a mirror. The stable channel rather than the latest
/// one, because a Windows guest rebuilt next month should get the drivers it
/// got this month.
const VIRTIO_WIN_URL: &str =
    "https://fedorapeople.org/groups/virt/virtio-win/direct-downloads/stable-virtio/virtio-win.iso";

/// Refuse installation media for the wrong architecture, now rather than in
/// half an hour.
///
/// This is the single most expensive mistake available here. Setup ignores an
/// answer file whose architecture does not match its media in exactly the way
/// it ignores a missing one, so the wrong ISO produces an installer sitting at
/// its first screen — which, with nobody watching, looks like a hang and is
/// only noticed when the build times out.
///
/// Media vitro cannot read confidently is allowed through: somebody's custom
/// image should not be refused because its label says nothing.
fn check_media_arch(media: &Path, wanted: unattend::Arch) -> Result<()> {
    let Ok(image) = iso::Iso::open(media) else {
        return Ok(());
    };
    let label = image.volume_id().to_string();
    let Some(found) = unattend::arch_from_volume_id(&label) else {
        return Ok(());
    };
    if found != wanted {
        bail!(
            "{} is {} media ({label:?}) and this host needs {}; Setup would ignore the \
             answer file and stop at its first screen half an hour from now",
            media.display(),
            found.processor_architecture(),
            wanted.processor_architecture()
        );
    }
    Ok(())
}

/// Find the virtio-win image, fetching it if that is the only way and somebody
/// says yes.
///
/// The URL is known, so making every user go and find it by hand is friction
/// for its own sake. Downloading several hundred megabytes without asking is
/// worse, though, so the default path asks — and only when there is somebody
/// to ask.
fn resolve_virtio_iso(paths: &Paths, spec: &Build) -> Result<PathBuf> {
    match &spec.virtio_iso {
        Some(Source::LocalPath(path)) => {
            if !path.exists() {
                bail!("virtio_iso {} does not exist", path.display());
            }
            Ok(path.clone())
        }
        Some(Source::Url(url)) => fetch_media(paths, url),
        Some(Source::Latest) => {
            bail!("`latest` has no meaning for `virtio_iso`; give a path or a URL")
        }
        None => {
            // Already fetched once: nothing to ask about.
            let cached = media::cache_path(&paths.media_cache_dir(), VIRTIO_WIN_URL);
            if cached.exists() {
                return Ok(cached);
            }
            if !confirm(&format!(
                "An unattended Windows install needs the virtio drivers, which Windows does not \
                 ship.\nDownload them from {VIRTIO_WIN_URL}?"
            ))? {
                bail!(
                    "no virtio drivers, so Setup would not see the disk to install onto; set \
                     `virtio_iso` to a path or a URL, or answer yes when asked"
                );
            }
            fetch_media(paths, VIRTIO_WIN_URL)
        }
    }
}

fn fetch_media(paths: &Paths, url: &str) -> Result<PathBuf> {
    let cached = media::cache_path(&paths.media_cache_dir(), url);
    media::fetch(url, &cached, None, |done, total| {
        report_progress(done, total);
    })?;
    Ok(cached)
}

/// Ask a yes-or-no question, and take silence for no.
///
/// Only when a terminal is attached. vitro is usually driven by a script or an
/// agent, and a prompt with nothing on the other end of stdin is a hang rather
/// than a question — which is exactly the failure this is meant to prevent.
fn confirm(question: &str) -> Result<bool> {
    use std::io::{BufRead, IsTerminal, Write};

    if !std::io::stdin().is_terminal() {
        return Ok(false);
    }
    eprint!("{question} [y/N] ");
    let _ = std::io::stderr().flush();

    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer)?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// Debug symbols outweigh the drivers several times over, and the volume they
/// are going onto is measured in megabytes.
fn is_driver_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    [".inf", ".sys", ".cat", ".exe", ".dll"]
        .iter()
        .any(|extension| lower.ends_with(extension))
}

/// Room for the answer file, the setup script and three driver sets, with
/// enough left over that adding one more does not silently overflow.
const WINDOWS_SEED_SIZE: ByteSize = ByteSize::from_bytes(32 * 1024 * 1024);

/// How long the installer is fed keystrokes after the machine starts.
///
/// The firmware's boot prompt wants "any key" and appears at a moment that
/// depends on the host. An arrow key satisfies the prompt and activates nothing
/// once Setup has the focus, so the window can be generous rather than exact —
/// which is the only way it works on both a fast and a slow host.
const INSTALLER_KEY_WINDOW: Duration = Duration::from_secs(45);
const INSTALLER_KEY_INTERVAL: Duration = Duration::from_millis(500);

pub struct Options {
    pub image: String,
    pub keep_failed: bool,
}

pub struct Built {
    pub golden: PathBuf,
    /// The guest account's password, for a guest that needed one made up.
    /// Nothing else records it, so `build` has to be the one to say it.
    pub password: Option<String>,
    pub took: Duration,
}

pub fn build(paths: &Paths, config: &Config, options: &Options) -> Result<Built> {
    let (image, spec) = config.buildable_image(&options.image)?;
    if spec.kind == BuildKind::LumeIpsw {
        return build_lume(paths, config, image, spec);
    }

    let unattended_kind = spec.kind == BuildKind::UnattendedInstall;
    let started = Instant::now();
    let now = OffsetDateTime::now_utc();
    let stamp = golden::stamp(now);
    let dir = paths.builds_dir().join(format!("{}-{stamp}", image.key));
    fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    restrict_to_owner(&dir);
    let mut guard = BuildGuard::new(dir.clone(), options.keep_failed);

    let host = qemu::HostTarget::detect()?;
    // The guest's architecture is the host's: vitro runs accelerated
    // same-architecture guests and nothing else. Both the answer file and the
    // driver directories have to name it, and an architecture vitro cannot
    // spell has to stop here — Setup ignores an answer file whose architecture
    // does not match its media exactly the way it ignores a missing one, so
    // guessing would produce an install that sits at the first screen half an
    // hour from now.
    let windows_arch = unattend::Arch::of_host();
    if unattended_kind && windows_arch.is_none() {
        bail!(
            "an unattended Windows install needs an architecture vitro knows how to \
             name in the answer file; this host is {}",
            std::env::consts::ARCH
        );
    }
    let cfg_tools = config.tools();
    let tools = tools::Tools::for_qemu(
        &host.binary,
        cfg_tools.qemu.as_deref(),
        cfg_tools.qemu_img.as_deref(),
        cfg_tools.ssh.as_deref(),
        cfg_tools.scp.as_deref(),
    )?;

    let source = resolve_media(paths, spec, windows_arch)?;
    let unattended = unattended_kind;

    let (disk, media, secret) = if unattended {
        let disk = blank_disk(&tools.qemu_img, &dir, spec)?;
        let arch = windows_arch.expect("the architecture was checked above");
        check_media_arch(&source, arch)?;
        // Before anything long-running: a missing driver image should stop the
        // build here rather than half an hour into an install.
        let virtio_iso = resolve_virtio_iso(paths, spec)?;
        let (seed_image, password) = write_windows_seed(&dir, image, spec, arch, &virtio_iso)?;
        let media = vec![
            qemu::Media::Cdrom(source.clone()),
            qemu::Media::Volume(seed_image),
        ];
        (disk, media, password)
    } else {
        let disk = prepare_disk(&tools.qemu_img, &source, &dir, spec)?;
        let seed_image = write_seed(&dir, image, paths, &stamp)?;
        (disk, vec![qemu::Media::Volume(seed_image)], None)
    };

    let (pid, port, qmp_socket) = boot(&tools.qemu, &host, image, &dir, &disk, &media, unattended)?;
    guard.owns_process(pid);

    let target = ssh::SshTarget {
        host: "127.0.0.1".into(),
        port,
        user: image.ssh_user.clone(),
        key: image.ssh_key.clone(),
        known_hosts: dir.join(KNOWN_HOSTS),
    };

    let timeout = if unattended {
        // The installer reboots several times and fetches OpenSSH over the
        // network before it answers, so the ceiling is the whole build's.
        wake_the_installer(qmp_socket.as_deref())?;
        spec.build_timeout
    } else {
        CLOUD_IMAGE_BOOT_TIMEOUT
    };
    wait_for_guest(&tools.ssh, &target, timeout, pid, &dir)?;

    if let Some(script) = &spec.provision {
        provision(&tools.ssh, &tools.scp, &target, script)?;
    }

    if image.guest == Guest::Unix {
        golden::quiesce_cloud_init(&tools.ssh, &target);
    }
    golden::shut_down(&tools.ssh, &target, image.guest, pid, qmp_socket.as_deref())?;
    guard.released_process();

    // Flattening drops the seed's influence and normalises the image, so the
    // artifact does not carry the build's scaffolding.
    let destination = golden::dated_path(&paths.golden_dir(), &image.key, now);
    crate::signals::check()?;
    golden::install(&tools.qemu_img, &disk, &destination)?;

    guard.commit();
    Ok(Built {
        golden: destination,
        password: secret,
        took: started.elapsed(),
    })
}

/// Build a macOS golden image by having `lume` install one.
///
/// Nothing is flattened here: the artifact is a VM `lume` keeps, not a disk
/// vitro owns, so the golden image is a name rather than a path. The install
/// itself is unattended because `lume`'s preset does the setup assistant's job
/// offline — vitro has no answer-file equivalent for macOS and needs none.
fn build_lume(paths: &Paths, config: &Config, image: &Image, spec: &Build) -> Result<Built> {
    let started = Instant::now();
    let Golden::VmName(golden) = &image.golden else {
        bail!(
            "image {:?} uses the lume backend, so `golden` must name a VM rather than a disk",
            image.key
        );
    };

    let binary = tools::resolve("lume", config.tools().lume.as_deref(), "tools.lume")?.path;
    let ipsw = match spec.source.clone().unwrap_or(Source::Latest) {
        // `latest` is the point of this backend: which macOS builds install on
        // which host is Apple's business, and lume is what tracks it.
        Source::Latest => {
            let url = lume::latest_ipsw(&binary)?;
            let cached = media::cache_path(&paths.media_cache_dir(), &url);
            media::fetch(&url, &cached, spec.sha256.as_deref(), report_progress)?;
            cached
        }
        _ => resolve_media(paths, spec, None)?,
    };

    // Built under a working name and renamed by cloning at the end, so a build
    // that fails part-way never leaves something `run` would start.
    let staging = format!("{golden}-building");
    let _ = lume::run(&binary, &lume::delete_args(&staging));

    lume::run_visibly(
        &binary,
        &lume::create_args(&lume::CreateSpec {
            name: &staging,
            ipsw: &ipsw,
            cpus: image.cpus,
            memory: image.memory,
            disk_size: spec.disk_size,
            unattended: spec.unattended.as_deref(),
        }),
    )?;

    let result = finish_lume(&binary, config, paths, image, spec, golden, &staging);
    if result.is_err() {
        let _ = lume::run(&binary, &lume::stop_args(&staging));
        let _ = lume::run(&binary, &lume::delete_args(&staging));
    }
    result?;

    Ok(Built {
        golden: PathBuf::from(golden),
        password: None,
        took: started.elapsed(),
    })
}

/// Authorise vitro's key in the account the preset made.
///
/// Through `lume ssh`, which handles the preset's password, because this is the
/// one moment vitro has no key to connect with.
fn install_key(binary: &Path, staging: &str, image: &Image) -> Result<()> {
    let public_key = format!("{}.pub", image.ssh_key.display());
    let key =
        fs::read_to_string(&public_key).with_context(|| format!("cannot read {public_key}"))?;
    let key = key.trim();

    // Single-quoted for the guest's shell, and the key itself cannot contain a
    // quote — it is base64 and a comment.
    let command = format!(
        "mkdir -p ~/.ssh && chmod 700 ~/.ssh && \
         printf '%s\\n' '{key}' >> ~/.ssh/authorized_keys && \
         chmod 600 ~/.ssh/authorized_keys"
    );
    lume::run(binary, &lume::ssh_args(staging, &command))
        .with_context(|| format!("cannot authorise vitro's key in {staging}"))?;
    Ok(())
}

/// Provision the freshly installed VM and leave it as the golden image.
#[allow(clippy::too_many_arguments)]
fn finish_lume(
    binary: &Path,
    config: &Config,
    paths: &Paths,
    image: &Image,
    spec: &Build,
    golden: &str,
    staging: &str,
) -> Result<()> {
    // Always, not only for a provision script: the preset leaves an account
    // with a password lume publishes, and vitro reaches every other guest with
    // a key. `provision` cannot be what installs it, because `provision` is
    // run over the connection the key is for.
    lume::run(binary, &lume::run_args(staging))?;
    // One ceiling covering both waits, not one each.
    let booting = Instant::now();
    let address = lume::wait_until_ready(binary, staging, spec.build_timeout)?;
    install_key(binary, staging, image)?;

    if let Some(script) = &spec.provision {
        let cfg_tools = config.tools();
        let ssh = tools::resolve("ssh", cfg_tools.ssh.as_deref(), "tools.ssh")?.path;
        let scp = tools::resolve("scp", cfg_tools.scp.as_deref(), "tools.scp")?.path;

        // One known_hosts for every connection of this build, in a directory
        // thrown away with it. `/dev/null` would mean each connection accepts
        // whatever key it is offered, which is no check at all.
        let dir = paths.builds_dir().join(format!("{}-lume", image.key));
        fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
        restrict_to_owner(&dir);
        let target = ssh::SshTarget {
            host: address,
            port: 22,
            user: image.ssh_user.clone(),
            key: image.ssh_key.clone(),
            known_hosts: dir.join(KNOWN_HOSTS),
        };
        let left = ssh::remaining(spec.build_timeout, booting.elapsed());
        ssh::wait_until_reachable(&ssh, &target, left, || true)?;
        let provisioned = provision(&ssh, &scp, &target, script);
        let _ = fs::remove_dir_all(&dir);
        provisioned?;
    }

    lume::run(binary, &lume::stop_args(staging))?;

    // A clone on APFS is made by reference, so this is seconds rather than the
    // minutes a copy of tens of gigabytes would take.
    //
    // The existing golden is moved aside rather than deleted: deleting first
    // means a failed clone leaves neither the new image nor the one that was
    // working before it.
    let previous = format!("{golden}-previous");
    let had_one = lume::get(binary, golden).is_ok();
    if had_one {
        let _ = lume::run(binary, &lume::delete_args(&previous));
        lume::run(binary, &lume::clone_args(golden, &previous))?;
        lume::run(binary, &lume::delete_args(golden))?;
    }

    match lume::run(binary, &lume::clone_args(staging, golden)) {
        Ok(_) => {
            if had_one {
                let _ = lume::run(binary, &lume::delete_args(&previous));
            }
            lume::run(binary, &lume::delete_args(staging))?;
            Ok(())
        }
        Err(e) => {
            if had_one {
                // Put back what was working. Nothing else knows it is there.
                let _ = lume::run(binary, &lume::clone_args(&previous, golden));
                let _ = lume::run(binary, &lume::delete_args(&previous));
            }
            Err(e)
        }
    }
}

fn resolve_media(paths: &Paths, spec: &Build, arch: Option<unattend::Arch>) -> Result<PathBuf> {
    let Some(source) = &spec.source else {
        // Only reachable for an unattended install; anything else is refused
        // while the configuration is read.
        return find_supplied_media(paths, arch);
    };
    match source {
        Source::LocalPath(path) => {
            if !path.exists() {
                bail!("{} does not exist", path.display());
            }
            if let Some(sha) = &spec.sha256 {
                media::verify(path, sha)?;
            }
            Ok(path.clone())
        }
        Source::Url(url) => {
            let cached = media::cache_path(&paths.media_cache_dir(), url);
            media::fetch(url, &cached, spec.sha256.as_deref(), |done, total| {
                report_progress(done, total);
            })?;
            if spec.sha256.is_none() {
                eprintln!(
                    "vitro: {} has no sha256 in the configuration, so it was not verified",
                    cached.display()
                );
            }
            Ok(cached)
        }
        Source::Latest => bail!("`latest` has no meaning for this build kind"),
    }
}

/// The Windows media sitting in the directory vitro keeps for it.
///
/// vitro is not allowed to fetch a Windows ISO, so the next best thing is to
/// own the place it goes: "put it here" is a shorter instruction than "tell me
/// where you put it", and it survives the file being renamed by a browser.
/// Candidates are identified by their volume label, which names the
/// architecture the media is for.
fn find_supplied_media(paths: &Paths, arch: Option<unattend::Arch>) -> Result<PathBuf> {
    let dir = paths.media_dir();
    let wanted = arch.map(|arch| arch.processor_architecture());
    let mut matching = Vec::new();
    let mut rejected = Vec::new();

    for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if !path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("iso"))
        {
            continue;
        }
        let Ok(image) = iso::Iso::open(&path) else {
            continue;
        };
        match unattend::arch_from_volume_id(image.volume_id()) {
            Some(found) if Some(found.processor_architecture()) == wanted => matching.push(path),
            Some(found) => rejected.push((path, found.processor_architecture())),
            // A label that names no architecture is somebody's own media, and
            // taking it is better than ignoring it — but only when there is
            // nothing better.
            None => matching.push(path),
        }
    }

    matching.sort();
    match matching.len() {
        1 => Ok(matching.remove(0)),
        0 => {
            let mut message = String::new();
            // What was rejected comes first: somebody who has already
            // downloaded an ISO needs to hear that theirs is the wrong one,
            // not to be told from the top to go and download one.
            for (path, found) in &rejected {
                message.push_str(&format!(
                    "{} is {found} media, and this host needs {}.\n\n",
                    path.display(),
                    wanted.unwrap_or("?")
                ));
            }
            message.push_str(&media::windows_media_instructions(
                &dir,
                std::env::consts::ARCH,
            ));
            bail!("{message}")
        }
        _ => {
            let names: Vec<String> = matching
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            bail!(
                "more than one candidate in {}; set `source` to the one you mean:\n  {}",
                dir.display(),
                names.join("\n  ")
            )
        }
    }
}

/// Report a download's progress, in a shape that suits where it is going.
///
/// A terminal gets one line rewritten in place. A log file gets a line every
/// ten percent instead: the carriage returns that redraw a terminal leave a
/// redirected log as one unreadable line thousands of columns wide, and a
/// 20 GB download is exactly the kind that ends up in a log.
fn report_progress(done: u64, total: u64) {
    let percent = done
        .checked_mul(100)
        .and_then(|n| n.checked_div(total))
        .unwrap_or(100);
    let finished = done >= total;

    if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
        eprint!(
            "\rvitro: downloading {percent:>3}% ({} of {})",
            ByteSize::from_bytes(done),
            ByteSize::from_bytes(total)
        );
        if finished {
            eprintln!();
        }
        return;
    }

    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(u64::MAX);
    let step = percent / 10;
    if finished || LAST.swap(step, std::sync::atomic::Ordering::Relaxed) != step {
        eprintln!(
            "vitro: downloading {percent:>3}% ({} of {})",
            ByteSize::from_bytes(done),
            ByteSize::from_bytes(total)
        );
    }
}

/// A cloud image is already installed; it only has to be copied somewhere
/// writable and grown to the configured size.
fn prepare_disk(qemu_img: &Path, source: &Path, dir: &Path, spec: &Build) -> Result<PathBuf> {
    let disk = dir.join(DISK_FILE);
    fs::copy(source, &disk)
        .with_context(|| format!("cannot copy {} into the build", source.display()))?;
    if let Some(size) = spec.disk_size {
        image::run(qemu_img, &image::resize_args(&disk, size))?;
    }
    Ok(disk)
}

/// Keep the build's working files to the user who started it.
///
/// A Windows build writes an answer file carrying the account's password, and
/// the state directory's own permissions are whatever the user's umask made
/// them. Best effort, and a no-op where permissions do not work this way.
fn restrict_to_owner(dir: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// An empty disk for an operating system that is about to install itself.
fn blank_disk(qemu_img: &Path, dir: &Path, spec: &Build) -> Result<PathBuf> {
    let disk = dir.join(DISK_FILE);
    let size = spec.disk_size.unwrap_or(DEFAULT_WINDOWS_DISK);
    image::run(qemu_img, &image::create_blank_args(&disk, size))?;
    Ok(disk)
}

/// The volume Setup reads its answer file, its drivers and its first-logon
/// script from. Returns the volume and the account password it was given.
fn write_windows_seed(
    dir: &Path,
    image: &Image,
    spec: &Build,
    arch: unattend::Arch,
    virtio_iso: &Path,
) -> Result<(PathBuf, Option<String>)> {
    let public_key = format!("{}.pub", image.ssh_key.display());
    let key = fs::read_to_string(&public_key).with_context(|| {
        format!(
            "cannot read {public_key}; generate a key pair there, or point `ssh_key` at one \
             (vitro expects <ssh_key>.pub beside it)"
        )
    })?;

    let password = generated_password();
    let answers = unattend::Answers {
        user: image.ssh_user.clone(),
        password: password.clone(),
        computer_name: image.key.clone(),
        arch,
        ..unattend::Answers::default()
    };

    // A caller's own answer file is used whole, so it — not vitro — decides what
    // the account's password is. Reporting a password vitro made up would be
    // reporting one that is not set, and writing it into the guest's automatic
    // logon would break the logon rather than enable it.
    let (answer_file, reported) = match &spec.unattend {
        Some(path) => (
            fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?,
            None,
        ),
        None => (unattend::answer_file(&answers)?, Some(password.clone())),
    };

    let mut entries = vec![
        seed::Entry::new("autounattend.xml", answer_file),
        seed::Entry::new("authorized_keys", key),
    ];
    // The setup script sets up automatic logon, which needs the password that
    // is actually in the answer file. With somebody else's answer file vitro
    // does not know it, so it does not pretend to.
    if reported.is_some() {
        entries.push(seed::Entry::new(
            "vitro-setup.ps1",
            unattend::setup_script(&answers),
        ));
    }
    entries.extend(windows_drivers(virtio_iso, arch)?);

    let out = dir.join("seed.img");
    seed::write_sized(&out, seed::UNATTEND_LABEL, &entries, WINDOWS_SEED_SIZE)?;
    Ok((out, reported))
}

/// Lift the drivers for the architecture being installed off the virtio-win
/// image.
///
/// Every file each driver's `.inf` refers to comes along, because picking
/// individual ones is how this first went wrong: `netkvm.inf` copies
/// `netkvmp.exe` as well, and `pnputil` reports only "cannot find the file
/// specified" when one is missing — half an hour into an install.
fn windows_drivers(virtio_iso: &Path, arch: unattend::Arch) -> Result<Vec<seed::Entry>> {
    let path = virtio_iso;
    let mut image = iso::Iso::open(path)?;

    let mut entries = Vec::new();
    for directory in virtio_driver_paths(arch) {
        let files = image
            .read_dir_files(&directory, is_driver_file)
            .with_context(|| format!("cannot read {directory} from {}", path.display()))?;
        if files.is_empty() {
            bail!("{directory} in {} holds no drivers", path.display());
        }
        for (name, contents) in files {
            entries.push(seed::Entry::new(name, contents));
        }
    }
    Ok(entries)
}

/// A password for the guest account.
///
/// Made up rather than configured: it exists because Windows insists on one,
/// the key is what actually gets used, and a golden image that ships with a
/// password someone could guess is worse than one nobody remembers.
fn generated_password() -> String {
    // Ambiguous glyphs left out: this is a password someone may have to read
    // off a screenshot of a guest that has no other way in.
    const ALPHABET: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let body: String = (0..20)
        .map(|_| {
            let pick = usize::from(rand::random::<u8>()) % ALPHABET.len();
            ALPHABET[pick] as char
        })
        .collect();
    // Windows wants three of its four character classes, and the alphabet above
    // supplies only two.
    format!("{body}-7")
}

/// Press an arrow key until the installer is past the firmware's boot prompt.
///
/// Arrow keys rather than Enter: the prompt takes any key, and Setup's focus
/// sits on Cancel, so a stray Enter opens "Are you sure you want to quit?" on
/// top of an install that was going fine.
fn wake_the_installer(qmp_socket: Option<&Path>) -> Result<()> {
    let Some(socket) = qmp_socket else {
        bail!(
            "an unattended install needs the QEMU monitor to get past the firmware's \
             boot prompt, and this host has none"
        );
    };
    let deadline = Instant::now() + INSTALLER_KEY_WINDOW;
    while Instant::now() < deadline {
        crate::signals::check()?;
        // Best effort: the monitor is briefly unavailable while the guest
        // resets, and that is not a reason to abandon a half-hour install.
        let _ = qmp::send_key(socket, &["down"]);
        std::thread::sleep(INSTALLER_KEY_INTERVAL);
    }
    Ok(())
}

fn write_seed(dir: &Path, image: &Image, paths: &Paths, stamp: &str) -> Result<PathBuf> {
    let public_key = format!("{}.pub", image.ssh_key.display());
    let key = fs::read_to_string(&public_key).with_context(|| {
        format!(
            "cannot read {public_key}; generate a key pair there, or point `ssh_key` at one \
             (vitro expects <ssh_key>.pub beside it)"
        )
    })?;
    let _ = paths;

    let payload = seed::CloudInit {
        instance_id: format!("vitro-{}-{stamp}", image.key),
        hostname: image.key.clone(),
        user: image.ssh_user.clone(),
        authorized_key: key,
    };
    let out = seed::seed_path(dir);
    seed::write(&out, seed::CLOUD_INIT_LABEL, &payload.entries())?;
    Ok(out)
}

/// Start the installer or the first boot.
///
fn boot(
    qemu_binary: &Path,
    host: &qemu::HostTarget,
    image: &Image,
    dir: &Path,
    disk: &Path,
    media: &[qemu::Media],
    unattended: bool,
) -> Result<(u32, u16, Option<PathBuf>)> {
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("cannot find a free port for the build")?;
        listener.local_addr()?.port()
    };

    let mut spec = qemu::VmSpec {
        name: format!("{}-build", image.key),
        cpus: image.cpus,
        memory: image.memory,
        system_disk: disk.to_path_buf(),
        firmware: image.firmware.clone(),
        varstore: dir.join(VARSTORE_FILE),
        ssh_port: port,
        resolution: image.resolution,
        serial_log: dir.join(SERIAL_LOG),
        pidfile: dir.join(PIDFILE),
        qmp_socket: host
            .supports_qmp()
            .then(|| qemu::qmp_socket_path(dir, &format!("{}-build", image.key)))
            .flatten(),
        media: if unattended {
            media.to_vec()
        } else {
            Vec::new()
        },
        extra_args: image.extra_args.clone(),
    };
    // A cloud image's seed rides on a second virtio disk rather than over USB:
    // it is not bootable, so it leaves the firmware's view of where the system
    // disk lives exactly as the image expects to find it.
    if !unattended {
        for medium in media {
            let qemu::Media::Volume(path) = medium else {
                bail!("only an unattended install boots from media");
            };
            spec.extra_args.extend([
                "-drive".to_string(),
                format!("if=virtio,format=raw,file={}", path.display()),
            ]);
        }
    }

    qemu::ensure_varstore(&spec.varstore)?;
    let args = qemu::args(&spec, host)?;
    let pid = qemu::spawn(qemu_binary, &args, &dir.join(QEMU_LOG), &dir.join(PIDFILE))?;
    Ok((pid, port, spec.qmp_socket))
}

fn wait_for_guest(
    ssh_binary: &Path,
    target: &ssh::SshTarget,
    timeout: Duration,
    pid: u32,
    dir: &Path,
) -> Result<()> {
    let probe = SysinfoProbe::new();
    let alive = move || probe.start_time(pid).is_some();

    ssh::wait_until_reachable(ssh_binary, target, timeout, alive).map_err(|e| {
        match tail(&dir.join(SERIAL_LOG), 20) {
            Some(tail) if !tail.trim().is_empty() => {
                e.context(format!("last lines of serial.log:\n{tail}"))
            }
            _ => e,
        }
    })?;
    Ok(())
}

/// Copy the user's script in and run it. Its exit code decides the build:
/// a golden image whose provisioning half-failed is the worst kind to ship,
/// because nothing about it looks wrong until something built on it breaks.
fn provision(
    ssh_binary: &Path,
    scp_binary: &Path,
    target: &ssh::SshTarget,
    script: &Path,
) -> Result<()> {
    if !script.exists() {
        bail!("provision script {} does not exist", script.display());
    }
    let remote = "/tmp/vitro-provision";

    let status = std::process::Command::new(scp_binary)
        .args(ssh::scp_args(target, script, remote))
        .status()
        .with_context(|| format!("cannot run {}", scp_binary.display()))?;
    if !status.success() {
        bail!("cannot copy {} into the guest", script.display());
    }

    let code = ssh::run_passthrough(
        ssh_binary,
        target,
        &ssh::SshOptions {
            command: vec![
                "sh".into(),
                "-c".into(),
                format!("chmod +x {remote} && {remote}"),
            ],
            ..ssh::SshOptions::default()
        },
    )?;
    if code != 0 {
        bail!("the provision script exited with {code}");
    }
    Ok(())
}

fn tail(path: &Path, lines: usize) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    let collected: Vec<&str> = text.lines().rev().take(lines).collect();
    Some(collected.into_iter().rev().collect::<Vec<_>>().join("\n"))
}

struct BuildGuard {
    dir: PathBuf,
    pid: Option<u32>,
    keep_failed: bool,
    committed: bool,
}

impl BuildGuard {
    fn new(dir: PathBuf, keep_failed: bool) -> Self {
        Self {
            dir,
            pid: None,
            keep_failed,
            committed: false,
        }
    }

    fn owns_process(&mut self, pid: u32) {
        self.pid = Some(pid);
    }

    fn released_process(&mut self) {
        self.pid = None;
    }

    fn commit(&mut self) {
        self.committed = true;
    }
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            crate::process::terminate(pid);
        }
        // Kept only to investigate a failure. A finished build has nothing
        // worth keeping: the golden image is a flattened copy, so what is left
        // here is a second copy of the same disk — tens of gigabytes for a
        // Windows guest — along with the answer file holding its password.
        if !self.committed && self.keep_failed {
            eprintln!("vitro: kept the failed build in {}", self.dir.display());
            return;
        }
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_drivers_are_taken_from_the_architecture_being_installed() {
        // The names are virtio-win's own layout. A path that is right for the
        // other architecture still resolves to a directory full of drivers,
        // which is why this is worth pinning: the failure is not "missing", it
        // is a driver Setup declines to load much later.
        assert_eq!(
            virtio_driver_paths(unattend::Arch::Aarch64),
            [
                "viostor/w11/ARM64",
                "NetKVM/w11/ARM64",
                "viogpudo/w11/ARM64"
            ]
        );
        assert_eq!(
            virtio_driver_paths(unattend::Arch::X86_64),
            [
                "viostor/w11/amd64",
                "NetKVM/w11/amd64",
                "viogpudo/w11/amd64"
            ]
        );
    }

    #[test]
    fn a_failed_build_takes_its_directory_with_it() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("build");
        fs::create_dir_all(&dir).unwrap();

        drop(BuildGuard::new(dir.clone(), false));

        assert!(!dir.exists());
    }

    #[test]
    fn keep_failed_leaves_the_directory_for_inspection() {
        // Without this an unattended install that fails after twenty minutes
        // leaves nothing to look at.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("build");
        fs::create_dir_all(&dir).unwrap();

        drop(BuildGuard::new(dir.clone(), true));

        assert!(dir.exists());
    }

    #[test]
    fn a_finished_build_takes_its_scaffolding_with_it() {
        // The golden image is a flattened copy by this point, so what is left
        // in the build directory is a second copy of the same disk — and, for
        // a Windows build, the answer file with the account's password in it.
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join("build");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("seed.img"), b"password").unwrap();

        let mut guard = BuildGuard::new(dir.clone(), false);
        guard.commit();
        drop(guard);

        assert!(!dir.exists());
    }

    #[test]
    fn keep_failed_keeps_a_failure_and_not_a_success() {
        let temp = tempfile::tempdir().unwrap();
        let kept = temp.path().join("failed");
        let gone = temp.path().join("succeeded");
        fs::create_dir_all(&kept).unwrap();
        fs::create_dir_all(&gone).unwrap();

        drop(BuildGuard::new(kept.clone(), true));
        let mut committed = BuildGuard::new(gone.clone(), true);
        committed.commit();
        drop(committed);

        assert!(kept.exists(), "a failed build is there to be looked at");
        assert!(!gone.exists(), "a finished one has nothing to look at");
    }

    #[test]
    fn progress_reaches_a_hundred_percent_without_dividing_by_zero() {
        report_progress(0, 0);
        report_progress(5, 10);
        report_progress(10, 10);
    }

    #[test]
    fn media_vitro_has_nowhere_to_look_for_says_where_to_put_it() {
        // The instruction has to name the directory. "Set `source`" sends
        // somebody back to the documentation; "put it here" does not.
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());

        let err = find_supplied_media(&paths, Some(unattend::Arch::Aarch64))
            .unwrap_err()
            .to_string();

        assert!(
            err.contains(&paths.media_dir().display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn a_missing_local_source_is_named() {
        let temp = tempfile::tempdir().unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        let spec = Build {
            kind: BuildKind::CloudImage,
            source: Some(Source::LocalPath(PathBuf::from("/nonexistent/image.qcow2"))),
            sha256: None,
            disk_size: None,
            provision: None,
            unattend: None,
            virtio_iso: None,
            unattended: None,
            build_timeout: Duration::from_secs(1),
        };

        let err = resolve_media(&paths, &spec, None).unwrap_err().to_string();

        assert!(err.contains("/nonexistent/image.qcow2"), "{err}");
    }

    #[test]
    fn a_local_source_with_a_wrong_checksum_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let media_file = temp.path().join("image.qcow2");
        fs::write(&media_file, b"abc").unwrap();
        let paths = Paths::from_env(&Default::default(), temp.path());
        let spec = Build {
            kind: BuildKind::CloudImage,
            source: Some(Source::LocalPath(media_file)),
            sha256: Some("00".into()),
            disk_size: None,
            provision: None,
            unattend: None,
            virtio_iso: None,
            unattended: None,
            build_timeout: Duration::from_secs(1),
        };

        let err = resolve_media(&paths, &spec, None).unwrap_err().to_string();

        assert!(err.contains("sha256"), "{err}");
    }
}
