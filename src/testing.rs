//! Helpers the unit tests share.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// `ETXTBSY`, which Linux and macOS both number 26. `ErrorKind` has no stable
/// name for it.
const TEXT_FILE_BUSY: i32 = 26;

/// Write a shell script that runs `body`, make it executable, and return once
/// it can actually be run.
///
/// Tests run on many threads. When another thread forks while this one still
/// has the file open for writing, the child holds that descriptor until it
/// execs, and running the script meanwhile fails with "Text file busy". A
/// probe run, which the script answers by exiting before `body`, waits that
/// out without any of `body`'s side effects.
pub(crate) fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    let script = format!("#!/bin/sh\n[ -n \"$VITRO_TEST_PROBE\" ] && exit 0\n{body}\n");
    std::fs::write(path, script).expect("write the script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    for _ in 0..200 {
        match Command::new(path).env("VITRO_TEST_PROBE", "1").status() {
            Err(e) if e.raw_os_error() == Some(TEXT_FILE_BUSY) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => panic!("cannot run {}: {e}", path.display()),
            Ok(_) => return,
        }
    }
    panic!("{} stayed busy", path.display());
}
