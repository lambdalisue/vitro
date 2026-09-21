//! Turning a VM description into a QEMU command line, and starting it.
//!
//! The argument list is a pure function of the spec so it can be asserted on
//! in full. When a guest fails to start, the exact line vitro ran is the
//! single most useful thing to be able to show, and a list built by a function
//! with no I/O in it is a list that can be printed before anything is spawned.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use crate::{ByteSize, Resolution};

/// EDK2 expects a variable store exactly this size next to its code half.
/// A zero-filled file is enough: the firmware formats it on first boot. The
/// distributed template is not needed, and on aarch64 it is not even named
/// after the architecture, so not needing it removes a thing users must find.
const VARSTORE_BYTES: u64 = 64 * 1024 * 1024;

/// What the host can run, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTarget {
    pub binary: String,
    pub machine: String,
    pub accel: String,
    pub cpu: String,
    /// Whether this QEMU can put itself into the background.
    daemonizes: bool,
}

impl HostTarget {
    pub fn detect() -> Result<Self> {
        Self::for_platform(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// Guests always match the host architecture. Emulating a different one is
    /// out of scope: a foreign-architecture binary is run by the guest OS's own
    /// user-mode emulation instead.
    pub fn for_platform(os: &str, arch: &str) -> Result<Self> {
        let (binary, machine) = match arch {
            "aarch64" => ("qemu-system-aarch64", "virt"),
            "x86_64" => ("qemu-system-x86_64", "q35"),
            other => bail!("unsupported host architecture {other:?}"),
        };
        let accel = match os {
            "macos" => "hvf",
            "linux" => "kvm",
            "windows" => "whpx",
            other => bail!("unsupported host operating system {other:?}"),
        };
        // WHPX refuses `-cpu host`; `max` is the closest it accepts.
        let cpu = if accel == "whpx" { "max" } else { "host" };

        Ok(Self {
            binary: if os == "windows" {
                format!("{binary}.exe")
            } else {
                binary.to_string()
            },
            machine: machine.to_string(),
            accel: accel.to_string(),
            daemonizes: os != "windows",
            cpu: cpu.to_string(),
        })
    }

    /// `virt` has no legacy BIOS, so it cannot start without a firmware image.
    pub fn requires_firmware(&self) -> bool {
        self.machine == "virt"
    }

    /// Windows has no Unix sockets for QMP, and vitro does not implement the
    /// named-pipe transport, so there the guest is stopped by other means.
    pub fn supports_qmp(&self) -> bool {
        !cfg!(windows)
    }

    /// Whether QEMU can put itself into the background.
    ///
    /// `-daemonize` is a POSIX thing and the Windows build does not have it, so
    /// there vitro detaches the child itself and keeps the PID it was given
    /// instead of reading one back out of a pidfile.
    ///
    /// A property of the target rather than of whoever compiled vitro. Asking
    /// `cfg!(windows)` here made a macOS target claim it could not daemonize
    /// whenever the build happened on Windows — harmless in production, where
    /// the two always agree, and enough to make the rendered command line
    /// untestable anywhere else.
    pub fn daemonizes(&self) -> bool {
        self.daemonizes
    }
}

/// Everything needed to start one VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VmSpec {
    pub name: String,
    pub cpus: u32,
    pub memory: ByteSize,
    pub system_disk: PathBuf,
    /// The read-only half of the UEFI firmware pair.
    pub firmware: Option<PathBuf>,
    pub varstore: PathBuf,
    pub ssh_port: u16,
    pub resolution: Resolution,
    pub serial_log: PathBuf,
    pub pidfile: PathBuf,
    pub qmp_socket: Option<PathBuf>,
    /// Removable volumes, attached over USB in the order given.
    ///
    /// Non-empty means "this guest is being installed": the first CD-ROM
    /// becomes the boot device, and the display falls back to `ramfb` because
    /// an installer has no driver for anything better.
    pub media: Vec<Media>,
    /// Appended verbatim, as an escape hatch for hosts vitro does not model.
    pub extra_args: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Media {
    /// Installation media, attached read-only.
    Cdrom(PathBuf),
    /// A writable volume, such as the one carrying an answer file.
    Volume(PathBuf),
}

/// USB ports 1 and 2 belong to the keyboard and the tablet, so removable
/// volumes start after them.
const FIRST_MEDIA_PORT: u32 = 3;

pub fn args(spec: &VmSpec, host: &HostTarget) -> Result<Vec<OsString>> {
    let mut args = Args::default();

    args.pair("-name", &spec.name);
    args.pair("-machine", format!("{},accel={}", host.machine, host.accel));
    args.pair("-cpu", &host.cpu);
    args.pair("-smp", spec.cpus.to_string());
    args.pair("-m", spec.memory.mib().to_string());

    match &spec.firmware {
        Some(code) => {
            args.pair_os(
                "-drive",
                join_os("if=pflash,format=raw,readonly=on,file=", code.as_os_str()),
            );
            args.pair_os(
                "-drive",
                join_os("if=pflash,format=raw,file=", spec.varstore.as_os_str()),
            );
        }
        None if host.requires_firmware() => bail!(
            "machine {:?} needs a UEFI firmware image; set `firmware` in the configuration",
            host.machine
        ),
        None => {}
    }

    // With nothing else attached the system disk is the only bootable device,
    // and `if=virtio` is the shortest way to say so. Adding installation media
    // means saying which of the two boots first, and that needs the drive and
    // the device spelled out separately.
    if spec.media.is_empty() {
        args.pair_os(
            "-drive",
            join_os("if=virtio,format=qcow2,file=", spec.system_disk.as_os_str()),
        );
    } else {
        args.pair_os(
            "-drive",
            join_os(
                "if=none,id=systemdisk,format=qcow2,file=",
                spec.system_disk.as_os_str(),
            ),
        );
        args.pair("-device", "virtio-blk-pci,drive=systemdisk,bootindex=1");
    }

    args.pair(
        "-netdev",
        format!("user,id=net0,hostfwd=tcp:127.0.0.1:{}-:22", spec.ssh_port),
    );
    args.pair("-device", "virtio-net-pci,netdev=net0");
    if spec.media.is_empty() {
        args.pair(
            "-device",
            format!(
                "virtio-gpu-pci,xres={},yres={}",
                spec.resolution.width, spec.resolution.height
            ),
        );
    } else {
        // An installer has no driver for virtio-gpu; ramfb is the framebuffer
        // the UEFI GOP already set up, so the screen works from the first frame.
        args.pair("-device", "ramfb");
    }

    // Keyboard and pointer exist for QMP input synthesis, not for a viewer.
    // Explicit ports keep the second USB device off an auto-created 1.1 hub,
    // where the firmware does not enumerate it.
    args.pair("-device", "qemu-xhci,id=usb,p2=8,p3=8");
    args.pair("-device", "usb-kbd,bus=usb.0,port=1");
    args.pair("-device", "usb-tablet,bus=usb.0,port=2");

    let mut booted_from_media = false;
    for (index, medium) in spec.media.iter().enumerate() {
        let id = format!("media{index}");
        let port = FIRST_MEDIA_PORT + index as u32;
        match medium {
            Media::Cdrom(path) => {
                args.pair_os(
                    "-drive",
                    join_os(
                        &format!("if=none,id={id},media=cdrom,readonly=on,file="),
                        path.as_os_str(),
                    ),
                );
                // The first CD-ROM is the installer, and it has to win against
                // the empty system disk.
                let boot = if booted_from_media {
                    String::new()
                } else {
                    booted_from_media = true;
                    ",bootindex=0".into()
                };
                args.pair(
                    "-device",
                    format!("usb-storage,bus=usb.0,port={port},drive={id}{boot}"),
                );
            }
            Media::Volume(path) => {
                args.pair_os(
                    "-drive",
                    join_os(
                        &format!("if=none,id={id},format=raw,file="),
                        path.as_os_str(),
                    ),
                );
                args.pair(
                    "-device",
                    format!("usb-storage,bus=usb.0,port={port},drive={id},removable=on"),
                );
            }
        }
    }

    args.pair("-display", "none");
    args.pair_os("-serial", join_os("file:", spec.serial_log.as_os_str()));
    args.pair_os("-pidfile", spec.pidfile.clone().into_os_string());

    if let Some(socket) = &spec.qmp_socket {
        args.pair_os(
            "-qmp",
            join_os(
                "unix:",
                &join_os_suffix(socket.as_os_str(), ",server=on,wait=off"),
            ),
        );
    }

    if host.daemonizes() {
        args.flag("-daemonize");
    }

    for extra in &spec.extra_args {
        args.flag(extra);
    }

    Ok(args.0)
}

#[derive(Default)]
struct Args(Vec<OsString>);

impl Args {
    fn flag(&mut self, value: impl AsRef<OsStr>) {
        self.0.push(value.as_ref().to_os_string());
    }

    fn pair(&mut self, flag: &str, value: impl AsRef<OsStr>) {
        self.flag(flag);
        self.flag(value);
    }

    fn pair_os(&mut self, flag: &str, value: OsString) {
        self.flag(flag);
        self.0.push(value);
    }
}

fn join_os(prefix: &str, value: &OsStr) -> OsString {
    let mut out = OsString::from(prefix);
    out.push(value);
    out
}

fn join_os_suffix(value: &OsStr, suffix: &str) -> OsString {
    let mut out = value.to_os_string();
    out.push(suffix);
    out
}

/// The command line as a single shell-ish line, for error messages and logs.
/// Not shell-quoted well enough to paste blindly, but enough to read.
pub fn command_line(binary: &str, args: &[OsString]) -> String {
    let mut out = String::from(binary);
    for arg in args {
        out.push(' ');
        out.push_str(&arg.to_string_lossy());
    }
    out
}

/// The longest a Unix socket's path may be.
///
/// `sockaddr_un.sun_path` is 104 bytes on macOS and 108 on Linux, including
/// the terminator, and QEMU refuses to start rather than truncating. A VM
/// directory under a deep home reaches that easily, so the limit has to be
/// checked before the command line is built.
const MAX_UNIX_SOCKET_PATH: usize = 104;

/// Where to put a VM's QMP socket.
///
/// Beside the VM by preference, so everything about a VM is in one directory.
/// When that path is too long the socket moves to the temporary directory,
/// which is short by definition. If even that does not fit, the VM runs
/// without QMP: losing a graceful shutdown is better than not starting.
pub fn qmp_socket_path(vm_dir: &Path, name: &str) -> Option<PathBuf> {
    let beside = vm_dir.join("qmp.sock");
    if fits_in_sun_path(&beside) {
        return Some(beside);
    }
    let short = std::env::temp_dir().join(format!("vitro-{name}.sock"));
    fits_in_sun_path(&short).then_some(short)
}

fn fits_in_sun_path(path: &Path) -> bool {
    path.as_os_str().len() < MAX_UNIX_SOCKET_PATH
}

/// Create the per-VM variable store if it is not there yet.
pub fn ensure_varstore(path: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let file =
        fs::File::create(path).with_context(|| format!("cannot create {}", path.display()))?;
    file.set_len(VARSTORE_BYTES)
        .with_context(|| format!("cannot size {}", path.display()))?;
    Ok(())
}

/// Start QEMU detached and return its PID.
///
/// `-daemonize` means the process vitro spawns exits immediately after forking,
/// so the PID has to come from the pidfile rather than from the child.
/// Start QEMU and return the PID of the process running the guest.
///
/// Two shapes, because `-daemonize` does not exist on Windows. Where it does,
/// the command vitro ran forks and exits, and the PID comes back through the
/// pidfile. Where it does not, the child vitro spawned *is* the guest, and its
/// PID is already in hand.
pub fn spawn(binary: &Path, args: &[OsString], log: &Path, pidfile: &Path) -> Result<u32> {
    // A stale pidfile would otherwise be read back as the new VM's PID.
    let _ = fs::remove_file(pidfile);

    let stderr =
        fs::File::create(log).with_context(|| format!("cannot create {}", log.display()))?;

    if !cfg!(windows) {
        return spawn_daemonized(binary, args, log, pidfile, stderr);
    }

    let child = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .spawn()
        .with_context(|| format!("cannot run {}", binary.display()))?;
    Ok(child.id())
}

fn spawn_daemonized(
    binary: &Path,
    args: &[OsString],
    log: &Path,
    pidfile: &Path,
    stderr: fs::File,
) -> Result<u32> {
    let status = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr)
        .status()
        .with_context(|| format!("cannot run {}", binary.display()))?;

    if !status.success() {
        let detail = fs::read_to_string(log).unwrap_or_default();
        let detail = detail.trim();
        bail!(
            "{} exited with {}{}\n  {}",
            binary.display(),
            status,
            if detail.is_empty() {
                String::new()
            } else {
                format!(": {detail}")
            },
            command_line(&binary.to_string_lossy(), args)
        );
    }

    // `-daemonize` means the process vitro ran has already forked and exited,
    // so by this point a QEMU is live whether or not its PID can be read. A
    // bare error here would strand it: no record, no pidfile, nothing to find
    // it by. Say so rather than pretending the start failed cleanly.
    let text = fs::read_to_string(pidfile).map_err(|e| {
        anyhow::anyhow!(
            "{} started but its pidfile {} could not be read ({e}); \
             a QEMU process may still be running and will have to be stopped by hand",
            binary.display(),
            pidfile.display()
        )
    })?;
    text.trim().parse().map_err(|_| {
        anyhow::anyhow!(
            "{} wrote {:?} to {}, which is not a PID; \
             a QEMU process may still be running and will have to be stopped by hand",
            binary.display(),
            text.trim(),
            pidfile.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> VmSpec {
        VmSpec {
            name: "linux-7f3a2c".into(),
            cpus: 4,
            memory: ByteSize::from_gib(8),
            system_disk: PathBuf::from("/state/vms/linux-7f3a2c/overlay.qcow2"),
            firmware: Some(PathBuf::from("/opt/edk2-aarch64-code.fd")),
            varstore: PathBuf::from("/state/vms/linux-7f3a2c/efi-vars.fd"),
            ssh_port: 53422,
            resolution: Resolution::new(1280, 800),
            serial_log: PathBuf::from("/state/vms/linux-7f3a2c/serial.log"),
            pidfile: PathBuf::from("/state/vms/linux-7f3a2c/qemu.pid"),
            qmp_socket: Some(PathBuf::from("/state/vms/linux-7f3a2c/qmp.sock")),
            media: Vec::new(),
            extra_args: Vec::new(),
        }
    }

    fn apple_silicon() -> HostTarget {
        HostTarget::for_platform("macos", "aarch64").unwrap()
    }

    fn rendered(spec: &VmSpec, host: &HostTarget) -> String {
        command_line("qemu", &args(spec, host).unwrap())
    }

    #[test]
    fn apple_silicon_uses_the_virt_machine_with_hvf() {
        let host = apple_silicon();

        assert_eq!(host.binary, "qemu-system-aarch64");
        assert_eq!(host.machine, "virt");
        assert_eq!(host.accel, "hvf");
        assert_eq!(host.cpu, "host");
    }

    #[test]
    fn linux_on_x86_uses_q35_with_kvm() {
        let host = HostTarget::for_platform("linux", "x86_64").unwrap();

        assert_eq!(host.binary, "qemu-system-x86_64");
        assert_eq!(host.machine, "q35");
        assert_eq!(host.accel, "kvm");
    }

    #[test]
    fn whpx_does_not_get_cpu_host() {
        // WHPX rejects it outright, so the host description has to differ.
        let host = HostTarget::for_platform("windows", "x86_64").unwrap();

        assert_eq!(host.accel, "whpx");
        assert_eq!(host.cpu, "max");
        assert_eq!(host.binary, "qemu-system-x86_64.exe");
    }

    #[test]
    fn an_unknown_platform_is_named_in_the_error() {
        let err = HostTarget::for_platform("macos", "riscv64")
            .unwrap_err()
            .to_string();
        assert!(err.contains("riscv64"), "{err}");

        let err = HostTarget::for_platform("plan9", "x86_64")
            .unwrap_err()
            .to_string();
        assert!(err.contains("plan9"), "{err}");
    }

    #[test]
    fn the_command_line_carries_the_shape_the_experiments_settled_on() {
        let line = rendered(&spec(), &apple_silicon());

        assert!(line.contains("-machine virt,accel=hvf"), "{line}");
        assert!(line.contains("-cpu host"), "{line}");
        assert!(line.contains("-smp 4"), "{line}");
        assert!(line.contains("-m 8192"), "{line}");
        assert!(line.contains("-display none"), "{line}");
        assert!(line.contains("-daemonize"), "{line}");
    }

    #[test]
    fn only_a_windows_target_is_denied_daemonizing() {
        // Whoever compiled vitro has no say in this: the flag exists or does
        // not on the QEMU being started, and the rendered command line has to
        // be assertable from any host.
        assert!(HostTarget::for_platform("macos", "aarch64")
            .unwrap()
            .daemonizes());
        assert!(HostTarget::for_platform("linux", "x86_64")
            .unwrap()
            .daemonizes());
        assert!(!HostTarget::for_platform("windows", "x86_64")
            .unwrap()
            .daemonizes());
    }

    #[test]
    fn the_firmware_pair_is_read_only_code_plus_a_writable_store() {
        let line = rendered(&spec(), &apple_silicon());

        assert!(
            line.contains("if=pflash,format=raw,readonly=on,file=/opt/edk2-aarch64-code.fd"),
            "{line}"
        );
        assert!(
            line.contains("if=pflash,format=raw,file=/state/vms/linux-7f3a2c/efi-vars.fd"),
            "{line}"
        );
    }

    #[test]
    fn a_virt_machine_without_firmware_is_refused_with_the_setting_to_fix() {
        let mut spec = spec();
        spec.firmware = None;

        let err = args(&spec, &apple_silicon()).unwrap_err().to_string();

        assert!(err.contains("firmware"), "{err}");
    }

    #[test]
    fn q35_can_start_without_firmware() {
        let mut spec = spec();
        spec.firmware = None;

        let host = HostTarget::for_platform("linux", "x86_64").unwrap();

        assert!(args(&spec, &host).is_ok());
    }

    #[test]
    fn the_system_disk_is_the_only_bootable_device() {
        // Anything else bootable moves the PCI topology the firmware recorded
        // its boot entry against.
        let line = rendered(&spec(), &apple_silicon());

        assert!(line.contains("if=virtio,format=qcow2,file="), "{line}");
        assert!(!line.contains("media=cdrom"), "{line}");
        assert!(!line.contains("-boot"), "{line}");
    }

    #[test]
    fn installation_media_boots_before_the_empty_system_disk() {
        let spec = VmSpec {
            media: vec![
                Media::Cdrom(PathBuf::from("/media/windows.iso")),
                Media::Volume(PathBuf::from("/build/seed.img")),
            ],
            ..spec()
        };

        let line = rendered(&spec, &apple_silicon());

        assert!(
            line.contains("virtio-blk-pci,drive=systemdisk,bootindex=1"),
            "{line}"
        );
        assert!(
            line.contains("usb-storage,bus=usb.0,port=3,drive=media0,bootindex=0"),
            "{line}"
        );
    }

    #[test]
    fn every_removable_volume_gets_a_port_of_its_own() {
        // Left to itself QEMU puts the second one behind an auto-created USB
        // 1.1 hub, where the firmware never enumerates it — and the answer file
        // simply never appears.
        let spec = VmSpec {
            media: vec![
                Media::Cdrom(PathBuf::from("/media/windows.iso")),
                Media::Volume(PathBuf::from("/build/seed.img")),
            ],
            ..spec()
        };

        let line = rendered(&spec, &apple_silicon());

        assert!(line.contains("port=3,drive=media0"), "{line}");
        assert!(line.contains("port=4,drive=media1"), "{line}");
        assert!(line.contains("drive=media1,removable=on"), "{line}");
    }

    #[test]
    fn an_installer_gets_the_framebuffer_the_firmware_already_set_up() {
        let installing = rendered(
            &VmSpec {
                media: vec![Media::Cdrom(PathBuf::from("/media/windows.iso"))],
                ..spec()
            },
            &apple_silicon(),
        );

        assert!(installing.contains("-device ramfb"), "{installing}");
        assert!(!installing.contains("virtio-gpu"), "{installing}");
    }

    #[test]
    fn a_host_that_cannot_daemonize_is_not_asked_to() {
        let mut host = apple_silicon();
        host.machine = "virt".into();
        let line = rendered(&spec(), &host);

        // The flag is emitted for this host; the Windows shape is decided by
        // `daemonizes`, which is what spawn() branches on too.
        assert_eq!(line.contains("-daemonize"), host.daemonizes(), "{line}");
    }

    #[test]
    fn ssh_is_forwarded_from_loopback_only() {
        let line = rendered(&spec(), &apple_silicon());

        assert!(
            line.contains("user,id=net0,hostfwd=tcp:127.0.0.1:53422-:22"),
            "{line}"
        );
    }

    #[test]
    fn the_display_is_virtio_gpu_at_the_configured_size() {
        let mut spec = spec();
        spec.resolution = Resolution::new(1920, 1080);

        let line = rendered(&spec, &apple_silicon());

        assert!(
            line.contains("virtio-gpu-pci,xres=1920,yres=1080"),
            "{line}"
        );
    }

    #[test]
    fn usb_devices_get_explicit_ports() {
        // Left to itself QEMU hides the second one behind a USB 1.1 hub that
        // the firmware never enumerates.
        let line = rendered(&spec(), &apple_silicon());

        assert!(line.contains("usb-kbd,bus=usb.0,port=1"), "{line}");
        assert!(line.contains("usb-tablet,bus=usb.0,port=2"), "{line}");
    }

    #[test]
    fn the_qmp_socket_is_added_only_when_one_is_given() {
        let line = rendered(&spec(), &apple_silicon());
        assert!(
            line.contains("unix:/state/vms/linux-7f3a2c/qmp.sock,server=on,wait=off"),
            "{line}"
        );

        let mut without = spec();
        without.qmp_socket = None;
        assert!(!rendered(&without, &apple_silicon()).contains("-qmp"));
    }

    #[test]
    fn extra_args_are_appended_verbatim_and_last() {
        let mut spec = spec();
        spec.extra_args = vec!["-device".into(), "usb-audio".into()];

        let args = args(&spec, &apple_silicon()).unwrap();

        assert_eq!(args[args.len() - 2], OsString::from("-device"));
        assert_eq!(args[args.len() - 1], OsString::from("usb-audio"));
    }

    #[test]
    fn a_qmp_socket_sits_beside_the_vm_when_the_path_is_short_enough() {
        let path = qmp_socket_path(Path::new("/state/vms/linux-7f3a2c"), "linux-7f3a2c");

        assert_eq!(path.unwrap(), Path::new("/state/vms/linux-7f3a2c/qmp.sock"));
    }

    #[test]
    fn a_qmp_socket_moves_out_of_a_deep_directory_rather_than_failing_to_start() {
        // QEMU refuses to start at all when sun_path would overflow, so the
        // socket has to move even though it leaves the VM directory.
        let deep = PathBuf::from("/".to_string() + &"very-long-directory-name/".repeat(6));

        let path = qmp_socket_path(&deep, "linux-7f3a2c").unwrap();

        assert!(!path.starts_with(&deep), "{path:?}");
        assert!(path.to_string_lossy().contains("linux-7f3a2c"), "{path:?}");
        assert!(fits_in_sun_path(&path), "{path:?}");
    }

    #[test]
    fn a_variable_store_is_created_at_the_size_edk2_expects() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested/efi-vars.fd");

        ensure_varstore(&path).unwrap();

        assert_eq!(fs::metadata(&path).unwrap().len(), VARSTORE_BYTES);
    }

    #[test]
    fn an_existing_variable_store_is_left_alone() {
        // It holds the guest's boot entries; recreating it would lose them.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("efi-vars.fd");
        fs::write(&path, b"already formatted").unwrap();

        ensure_varstore(&path).unwrap();

        assert_eq!(fs::read(&path).unwrap(), b"already formatted");
    }
}
