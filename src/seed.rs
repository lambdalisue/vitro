//! Building the FAT volume that tells a fresh guest who it belongs to.
//!
//! Both guest families read one: cloud-init's NoCloud datasource wants a
//! volume labelled `CIDATA` holding `user-data` and `meta-data`, and Windows
//! Setup scans attached volumes for `autounattend.xml`. One FAT writer serves
//! both, so vitro needs no ISO tooling on the host at all.
//!
//! The label has to reach two places. `fatfs` writes it into the BPB *and* as
//! a root-directory entry with attribute 0x08, and `blkid` reads the second
//! one — a writer that filled only the BPB would look correct and never be
//! found by cloud-init. That is the first thing to check if this is ever
//! ported to another crate.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::ByteSize;

/// Big enough for an answer file plus a set of Windows drivers, small enough
/// that a mistake in disk selection is obvious.
const DEFAULT_SIZE: ByteSize = ByteSize::from_bytes(8 * 1024 * 1024);

pub const CLOUD_INIT_LABEL: &str = "CIDATA";
pub const UNATTEND_LABEL: &str = "UNATTEND";

/// A file to place at the root of the volume.
pub struct Entry {
    pub name: String,
    pub contents: Vec<u8>,
}

impl Entry {
    pub fn new(name: impl Into<String>, contents: impl Into<Vec<u8>>) -> Self {
        Self {
            name: name.into(),
            contents: contents.into(),
        }
    }

    pub fn from_path(path: &Path) -> Result<Self> {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow::anyhow!("{} has no usable file name", path.display()))?;
        let contents =
            std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
        Ok(Self::new(name, contents))
    }
}

/// Write a FAT16 volume holding `entries` at its root.
pub fn write(out: &Path, label: &str, entries: &[Entry]) -> Result<()> {
    write_sized(out, label, entries, DEFAULT_SIZE)
}

pub fn write_sized(out: &Path, label: &str, entries: &[Entry], size: ByteSize) -> Result<()> {
    let label = pad_label(label)?;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(out)
        .with_context(|| format!("cannot create {}", out.display()))?;
    file.set_len(size.bytes())
        .with_context(|| format!("cannot size {}", out.display()))?;

    let mut stream = fscommon::BufStream::new(file);
    let options = fatfs::FormatVolumeOptions::new()
        .fat_type(fatfs::FatType::Fat16)
        .volume_label(label)
        // Left at its default every seed carries the same volume UUID, which
        // makes two attached seeds indistinguishable to anything matching on
        // it.
        .volume_id(rand::random());
    fatfs::format_volume(&mut stream, options)
        .with_context(|| format!("cannot format {}", out.display()))?;

    {
        let fs = fatfs::FileSystem::new(&mut stream, fatfs::FsOptions::new())
            .with_context(|| format!("cannot open the volume in {}", out.display()))?;
        {
            let root = fs.root_dir();
            for entry in entries {
                let mut file = root
                    .create_file(&entry.name)
                    .with_context(|| format!("cannot create {} in the seed", entry.name))?;
                file.truncate()?;
                file.write_all(&entry.contents)
                    .with_context(|| format!("cannot write {} in the seed", entry.name))?;
            }
        }
        fs.unmount()
            .with_context(|| format!("cannot finish writing {}", out.display()))?;
    }
    Ok(())
}

/// FAT stores the label as exactly 11 bytes, space padded.
fn pad_label(label: &str) -> Result<[u8; 11]> {
    let bytes = label.as_bytes();
    if bytes.len() > 11 || !bytes.is_ascii() {
        bail!("volume label {label:?} must be at most 11 ASCII characters");
    }
    let mut padded = [b' '; 11];
    padded[..bytes.len()].copy_from_slice(bytes);
    Ok(padded)
}

/// The cloud-init payload that gives a guest its identity and vitro's key.
pub struct CloudInit {
    pub instance_id: String,
    pub hostname: String,
    pub user: String,
    pub authorized_key: String,
}

impl CloudInit {
    pub fn entries(&self) -> Vec<Entry> {
        vec![
            Entry::new("user-data", self.user_data()),
            Entry::new("meta-data", self.meta_data()),
        ]
    }

    pub fn user_data(&self) -> String {
        format!(
            "#cloud-config\n\
             users:\n  \
               - name: {user}\n    \
                 sudo: ALL=(ALL) NOPASSWD:ALL\n    \
                 shell: /bin/bash\n    \
                 lock_passwd: true\n    \
                 ssh_authorized_keys:\n      \
                   - {key}\n\
             ssh_pwauth: false\n",
            user = self.user,
            key = self.authorized_key.trim(),
        )
    }

    /// `instance-id` is what cloud-init compares against to decide whether it
    /// has already configured this machine. A promoted golden image carries
    /// the one it was built with, so every VM made from it needs a new value
    /// or nothing is applied — not the hostname, not the key.
    pub fn meta_data(&self) -> String {
        format!(
            "instance-id: {}\nlocal-hostname: {}\n",
            self.instance_id, self.hostname
        )
    }
}

/// Where a build keeps its seed.
pub fn seed_path(dir: &Path) -> PathBuf {
    dir.join("seed.img")
}

/// Read a file back out of a FAT volume. Only used by tests and `doctor`, but
/// it is the cheapest way to prove the writer produced something mountable.
pub fn read_file(image: &Path, name: &str) -> Result<Vec<u8>> {
    use std::io::Read;

    let file = File::open(image).with_context(|| format!("cannot open {}", image.display()))?;
    let mut stream = fscommon::BufStream::new(file);
    let fs = fatfs::FileSystem::new(&mut stream, fatfs::FsOptions::new())
        .with_context(|| format!("cannot read the volume in {}", image.display()))?;
    let root = fs.root_dir();
    let mut contents = Vec::new();
    root.open_file(name)
        .with_context(|| format!("{name} is not in {}", image.display()))?
        .read_to_end(&mut contents)?;
    Ok(contents)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cloud_init() -> CloudInit {
        CloudInit {
            instance_id: "vitro-0001".into(),
            hostname: "linux-7f3a2c".into(),
            user: "dev".into(),
            authorized_key: "ssh-ed25519 AAAA... vitro\n".into(),
        }
    }

    #[test]
    fn a_seed_can_be_read_back_out_of_itself() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("seed.img");

        write(
            &image,
            CLOUD_INIT_LABEL,
            &[Entry::new("user-data", "#cloud-config\n")],
        )
        .unwrap();

        assert_eq!(read_file(&image, "user-data").unwrap(), b"#cloud-config\n");
    }

    #[test]
    fn the_label_reaches_the_root_directory_entry_not_just_the_bpb() {
        // `blkid` reads the directory entry, and cloud-init finds the volume
        // through `blkid`. A label only in the BPB would look right here and
        // never be found by a guest.
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("seed.img");
        write(&image, CLOUD_INIT_LABEL, &[]).unwrap();
        let bytes = std::fs::read(&image).unwrap();

        // Root directory starts after the reserved sector and both FATs.
        let bytes_per_sector = u16::from_le_bytes([bytes[11], bytes[12]]) as usize;
        let reserved = u16::from_le_bytes([bytes[14], bytes[15]]) as usize;
        let fats = bytes[16] as usize;
        let fat_sectors = u16::from_le_bytes([bytes[22], bytes[23]]) as usize;
        let root = (reserved + fats * fat_sectors) * bytes_per_sector;

        assert_eq!(&bytes[root..root + 11], b"CIDATA     ");
        assert_eq!(bytes[root + 11], 0x08, "the volume label attribute");
    }

    #[test]
    fn two_seeds_do_not_share_a_volume_id() {
        let temp = tempfile::tempdir().unwrap();
        let read_id = |name: &str| {
            let image = temp.path().join(name);
            write(&image, CLOUD_INIT_LABEL, &[]).unwrap();
            let bytes = std::fs::read(&image).unwrap();
            u32::from_le_bytes([bytes[39], bytes[40], bytes[41], bytes[42]])
        };

        assert_ne!(read_id("a.img"), read_id("b.img"));
    }

    #[test]
    fn long_file_names_survive() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("seed.img");

        write(
            &image,
            UNATTEND_LABEL,
            &[Entry::new("autounattend.xml", "<unattend/>")],
        )
        .unwrap();

        assert_eq!(
            read_file(&image, "autounattend.xml").unwrap(),
            b"<unattend/>"
        );
    }

    #[test]
    fn a_label_that_cannot_fit_is_refused_by_name() {
        let temp = tempfile::tempdir().unwrap();

        let err = write(&temp.path().join("seed.img"), "THIS-IS-FAR-TOO-LONG", &[])
            .unwrap_err()
            .to_string();

        assert!(err.contains("11 ASCII"), "{err}");
    }

    #[test]
    fn cloud_init_user_data_carries_the_user_and_the_key() {
        let text = cloud_init().user_data();

        assert!(text.starts_with("#cloud-config\n"), "{text}");
        assert!(text.contains("- name: dev"), "{text}");
        assert!(text.contains("- ssh-ed25519 AAAA... vitro\n"), "{text}");
        assert!(text.contains("ssh_pwauth: false"), "{text}");
    }

    #[test]
    fn cloud_init_meta_data_carries_a_fresh_instance_id() {
        // Reusing one would make cloud-init skip configuration entirely on a
        // guest built from a promoted image.
        let text = cloud_init().meta_data();

        assert_eq!(
            text,
            "instance-id: vitro-0001\nlocal-hostname: linux-7f3a2c\n"
        );
    }

    #[test]
    fn a_cloud_init_seed_holds_both_files_a_guest_looks_for() {
        let temp = tempfile::tempdir().unwrap();
        let image = temp.path().join("seed.img");

        write(&image, CLOUD_INIT_LABEL, &cloud_init().entries()).unwrap();

        assert!(read_file(&image, "user-data").is_ok());
        assert!(read_file(&image, "meta-data").is_ok());
    }

    #[test]
    fn an_entry_read_from_disk_keeps_its_file_name() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("autounattend.xml");
        std::fs::write(&source, "<unattend/>").unwrap();

        let entry = Entry::from_path(&source).unwrap();

        assert_eq!(entry.name, "autounattend.xml");
        assert_eq!(entry.contents, b"<unattend/>");
    }
}
