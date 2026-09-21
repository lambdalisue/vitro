//! Running the real binary against a home directory that belongs to the test.
//!
//! Two things make an integration test here worth writing. The environment is
//! isolated, so a test never reads the developer's own configuration or leaves
//! anything in their state directory. And the external programs vitro shells
//! out to are replaced by scripts that record their arguments, so the argument
//! lists can be asserted on without a hypervisor.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A throwaway home directory plus a `bin` directory that shadows PATH.
pub struct Sandbox {
    temp: tempfile::TempDir,
}

impl Sandbox {
    pub fn new() -> Self {
        let temp = tempfile::tempdir().expect("temporary directory");
        fs::create_dir_all(temp.path().join("bin")).expect("bin directory");
        Self { temp }
    }

    pub fn home(&self) -> &Path {
        self.temp.path()
    }

    pub fn config_dir(&self) -> PathBuf {
        self.home().join(".config/vitro")
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir().join("config.toml")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.home().join(".local/state/vitro")
    }

    pub fn write_config(&self, toml: &str) -> &Self {
        fs::create_dir_all(self.config_dir()).expect("config directory");
        fs::write(self.config_file(), toml).expect("write config");
        self
    }

    /// Create a file with some bytes in it, making parent directories. Used to
    /// stand in for a golden image without producing a real one.
    pub fn write_file(&self, relative: impl AsRef<Path>, bytes: &[u8]) -> PathBuf {
        let path = self.home().join(relative);
        fs::create_dir_all(path.parent().expect("a parent")).expect("parent directories");
        fs::write(&path, bytes).expect("write file");
        path
    }

    /// Put an executable named `name` at the front of PATH which appends its
    /// arguments to `<home>/calls/<name>.log`, one invocation per line, and
    /// exits with `exit_code`.
    ///
    /// Unix-only, like the rest of the fake-binary scaffolding; the Windows
    /// host story is a separate piece of work.
    #[cfg(unix)]
    pub fn fake_bin(&self, name: &str, exit_code: i32) -> &Self {
        self.fake_bin_with(name, &format!("exit {exit_code}"))
    }

    /// The same, but with `body` running after the arguments are recorded. Use
    /// it when a fake has to produce a side effect a command depends on, such
    /// as QEMU's pidfile.
    #[cfg(unix)]
    pub fn fake_bin_with(&self, name: &str, body: &str) -> &Self {
        use std::os::unix::fs::PermissionsExt;

        let calls = self.home().join("calls");
        fs::create_dir_all(&calls).expect("calls directory");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}/{name}.log'\n{body}\n",
            calls.display()
        );
        let path = self.home().join("bin").join(name);
        fs::write(&path, script).expect("write fake binary");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
        self
    }

    /// Stand in for the QEMU that `run` starts: leave a real, long-lived
    /// process behind and write its PID where `-pidfile` says, because the
    /// record is only trusted while that process exists.
    #[cfg(unix)]
    pub fn fake_qemu(&self, name: &str) -> &Self {
        self.fake_bin_with(
            name,
            r#"
pidfile=""
serial=""
prev=""
for arg in "$@"; do
  case "$prev" in
    -pidfile) pidfile="$arg" ;;
    -serial) serial="${arg#file:}" ;;
  esac
  prev="$arg"
done
[ -n "$serial" ] && : > "$serial"
sleep 300 &
[ -n "$pidfile" ] && printf '%s\n' "$!" > "$pidfile"
exit 0
"#,
        )
    }

    /// Write a VM record straight into the state directory.
    ///
    /// Through the library rather than by hand-writing JSON, so a change to the
    /// record's shape breaks here at compile time instead of at run time. Pass
    /// a live PID to make the VM look running; any other value reads as dead.
    pub fn write_vm_record(&self, name: &str, pid: u32, port: u16) -> PathBuf {
        use vitro::ProcessProbe;

        let store = vitro::Store::new(self.state_dir());
        let dir = store.vm_dir(name);
        store
            .save(&vitro::VmRecord {
                name: name.into(),
                image: "linux".into(),
                backend: vitro::Backend::Qemu,
                guest: Default::default(),
                pid,
                pid_start_time: vitro::SysinfoProbe::new().start_time(pid).unwrap_or(1),
                ssh_host: "127.0.0.1".into(),
                ssh_port: port,
                ssh_user: "dev".into(),
                golden: vitro::Golden::Image(PathBuf::from("/srv/linux.qcow2")),
                qmp_socket: None,
                dir: dir.clone(),
                created_at: time::OffsetDateTime::UNIX_EPOCH,
            })
            .expect("write VM record");
        dir
    }

    /// Every invocation of a fake binary, in order.
    ///
    /// Unix-only, like the fakes themselves: on Windows nothing writes these
    /// logs, so a reader for them would be dead code rather than a helper
    /// waiting to be used.
    #[cfg(unix)]
    pub fn calls(&self, name: &str) -> Vec<String> {
        let path = self.home().join("calls").join(format!("{name}.log"));
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    pub fn run(&self, args: &[&str]) -> Run {
        self.run_with_env(args, &[])
    }

    pub fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        // The host's separator, not a colon: on Windows a colon makes the whole
        // variable one unusable entry, and every program lookup then fails for
        // a reason that has nothing to do with the test.
        let separator = if cfg!(windows) { ";" } else { ":" };
        let mut path = self.home().join("bin").into_os_string();
        if let Some(existing) = std::env::var_os("PATH") {
            path.push(separator);
            path.push(existing);
        }

        // vitro finds the home directory under the name the platform uses, so
        // an isolated run has to set that one. With `env_clear` and only `HOME`
        // set, every command on Windows failed before it started.
        let home_var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };

        let mut command = Command::new(env!("CARGO_BIN_EXE_vitro"));
        command
            .args(args)
            .env_clear()
            .env(home_var, self.home())
            .env("PATH", path)
            // The tests assert on messages, so keep the locale out of it.
            .env("LC_ALL", "C");
        // Windows keeps parts of its own API behind these; clearing them breaks
        // process creation itself rather than anything vitro does.
        #[cfg(windows)]
        for name in ["SYSTEMROOT", "SYSTEMDRIVE", "TEMP", "TMP"] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        for (key, value) in env {
            command.env(key, value);
        }

        Run::new(command.output().expect("vitro should be runnable"))
    }
}

impl Default for Sandbox {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Run {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Run {
    fn new(output: Output) -> Self {
        Self {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    #[track_caller]
    pub fn success(self) -> Self {
        assert_eq!(self.code, 0, "expected success\nstderr: {}", self.stderr);
        self
    }

    #[track_caller]
    pub fn failure(self) -> Self {
        assert_eq!(self.code, 1, "expected failure\nstdout: {}", self.stdout);
        self
    }

    #[track_caller]
    pub fn usage_error(self) -> Self {
        assert_eq!(
            self.code, 2,
            "expected a usage error\nstderr: {}",
            self.stderr
        );
        self
    }

    #[track_caller]
    pub fn stdout_contains(self, needle: &str) -> Self {
        assert!(
            self.stdout.contains(needle),
            "stdout did not contain {needle:?}\n{}",
            self.stdout
        );
        self
    }

    #[track_caller]
    pub fn stdout_is(self, expected: &str) -> Self {
        assert_eq!(self.stdout, expected, "stderr: {}", self.stderr);
        self
    }

    #[track_caller]
    pub fn stderr_contains(self, needle: &str) -> Self {
        assert!(
            self.stderr.contains(needle),
            "stderr did not contain {needle:?}\n{}",
            self.stderr
        );
        self
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.stdout).expect("stdout should be JSON")
    }
}
