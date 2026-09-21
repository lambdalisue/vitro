//! Reading files out of an ISO9660 image.
//!
//! Only one thing needs this: the Windows guest's virtio drivers ship as an
//! ISO, and they have to end up on the answer file's volume before Setup runs.
//! Asking the user to extract them first would be a step that fails silently —
//! a missing driver shows up as an installation that hangs half an hour later.
//!
//! Deliberately minimal. There is no Joliet or Rock Ridge handling because the
//! images that matter record usable mixed-case names in the primary descriptor
//! already, and no seeking inside a file because every file here is read whole.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{bail, Context, Result};

const SECTOR: u64 = 2048;
/// Volume descriptors start here, by the standard.
const FIRST_DESCRIPTOR: u64 = 16;
const DESCRIPTOR_PRIMARY: u8 = 1;
const DESCRIPTOR_TERMINATOR: u8 = 255;
const FLAG_DIRECTORY: u8 = 0x02;

/// Directory records that are not files: `.` and `..`, which the standard
/// spells as a one-byte identifier of 0 or 1.
fn is_self_or_parent(name: &[u8]) -> bool {
    matches!(name, [0] | [1])
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    extent: u32,
    length: u32,
}

impl Entry {
    pub fn size(&self) -> u64 {
        u64::from(self.length)
    }
}

pub struct Iso {
    file: File,
    size: u64,
    root: Entry,
    volume_id: String,
}

impl Iso {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let mut file =
            File::open(path).with_context(|| format!("cannot open {}", path.display()))?;
        let size = file
            .metadata()
            .with_context(|| format!("cannot measure {}", path.display()))?
            .len();
        let (root, volume_id) = primary(&mut file)
            .with_context(|| format!("{} is not a readable ISO9660 image", path.display()))?;
        Ok(Self {
            file,
            size,
            root,
            volume_id,
        })
    }

    /// The volume identifier from the primary descriptor.
    ///
    /// Worth having on its own because a Windows installer's contents live in a
    /// UDF filesystem that this reader cannot see — the ISO9660 side of the
    /// image holds almost nothing. The label is the one thing it does carry,
    /// and it names the architecture the media is for.
    pub fn volume_id(&self) -> &str {
        &self.volume_id
    }

    /// Entries directly inside `path`, which is `/`-separated and matched
    /// case-insensitively — the images in question are inconsistent about it.
    pub fn list(&mut self, path: &str) -> Result<Vec<Entry>> {
        let dir = self.locate(path)?;
        if !dir.is_dir {
            bail!("{path} is a file, not a directory");
        }
        self.entries(&dir)
    }

    pub fn read(&mut self, path: &str) -> Result<Vec<u8>> {
        let entry = self.locate(path)?;
        if entry.is_dir {
            bail!("{path} is a directory, not a file");
        }
        self.contents(&entry)
    }

    /// Every file directly inside `path`, as name → contents.
    ///
    /// `keep` decides what comes along. Driver directories carry debug symbols
    /// several times the size of the drivers themselves, and the volume they
    /// are going onto is measured in megabytes.
    pub fn read_dir_files(
        &mut self,
        path: &str,
        keep: impl Fn(&str) -> bool,
    ) -> Result<HashMap<String, Vec<u8>>> {
        let mut out = HashMap::new();
        for entry in self.list(path)? {
            if entry.is_dir || !keep(&entry.name) {
                continue;
            }
            let contents = self.contents(&entry)?;
            out.insert(entry.name.clone(), contents);
        }
        Ok(out)
    }

    fn locate(&mut self, path: &str) -> Result<Entry> {
        let mut current = self.root.clone();
        for part in path.split('/').filter(|part| !part.is_empty()) {
            let found = self
                .entries(&current)?
                .into_iter()
                .find(|entry| entry.name.eq_ignore_ascii_case(part));
            current = match found {
                Some(entry) => entry,
                None => bail!("{path} is not in this image ({part} is missing)"),
            };
        }
        Ok(current)
    }

    fn entries(&mut self, dir: &Entry) -> Result<Vec<Entry>> {
        let data = self.contents(dir)?;
        let mut out = Vec::new();
        let mut offset = 0usize;

        while offset < data.len() {
            let len = data[offset] as usize;
            if len == 0 {
                // A record never straddles a sector; zero padding means the
                // rest of this sector is empty.
                offset = (offset / SECTOR as usize + 1) * SECTOR as usize;
                continue;
            }
            if offset + len > data.len() || len < 34 {
                bail!("a directory record runs past the end of its extent");
            }
            let record = &data[offset..offset + len];
            let name_len = record[32] as usize;
            if 33 + name_len > len {
                bail!("a directory record claims a name longer than itself");
            }
            let name = &record[33..33 + name_len];
            if !is_self_or_parent(name) {
                out.push(Entry {
                    // Version suffixes are part of the standard's file names
                    // and never part of what anyone means by the file.
                    name: String::from_utf8_lossy(name)
                        .split(';')
                        .next()
                        .unwrap_or_default()
                        .to_string(),
                    is_dir: record[25] & FLAG_DIRECTORY != 0,
                    extent: le_u32(&record[2..6]) + u32::from(record[1]),
                    length: le_u32(&record[10..14]),
                });
            }
            offset += len;
        }
        Ok(out)
    }

    fn contents(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        // The length comes out of the image, so it is checked against the
        // image before it becomes an allocation: a corrupt or hostile one can
        // otherwise ask for four gigabytes.
        let start = u64::from(entry.extent) * SECTOR;
        let end = start.saturating_add(u64::from(entry.length));
        if end > self.size {
            bail!(
                "{} claims {} bytes at offset {start}, past the end of the image",
                entry.name,
                entry.length
            );
        }
        let mut buffer = vec![0u8; entry.length as usize];
        self.file
            .seek(SeekFrom::Start(start))
            .context("cannot seek in the image")?;
        self.file
            .read_exact(&mut buffer)
            .with_context(|| format!("cannot read {} from the image", entry.name))?;
        Ok(buffer)
    }
}

fn primary(file: &mut File) -> Result<(Entry, String)> {
    for index in 0..8 {
        let mut sector = [0u8; SECTOR as usize];
        file.seek(SeekFrom::Start((FIRST_DESCRIPTOR + index) * SECTOR))?;
        file.read_exact(&mut sector)?;

        if &sector[1..6] != b"CD001" {
            bail!("no ISO9660 volume descriptor where one is required");
        }
        match sector[0] {
            DESCRIPTOR_PRIMARY => {
                let record = &sector[156..156 + 34];
                let root = Entry {
                    name: "/".into(),
                    is_dir: true,
                    extent: le_u32(&record[2..6]) + u32::from(record[1]),
                    length: le_u32(&record[10..14]),
                };
                // 32 bytes of space-padded ASCII, by the standard.
                let volume_id = String::from_utf8_lossy(&sector[40..72]).trim().to_string();
                return Ok((root, volume_id));
            }
            DESCRIPTOR_TERMINATOR => break,
            _ => {}
        }
    }
    bail!("the image has no primary volume descriptor")
}

fn le_u32(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory record as the standard lays one out, for a file.
    fn record(name: &str, is_dir: bool, extent: u32, length: u32) -> Vec<u8> {
        let mut out = vec![0u8; 33];
        out[2..6].copy_from_slice(&extent.to_le_bytes());
        out[10..14].copy_from_slice(&length.to_le_bytes());
        out[25] = if is_dir { FLAG_DIRECTORY } else { 0 };
        out[32] = name.len() as u8;
        out.extend_from_slice(name.as_bytes());
        // Records are padded to an even length.
        if out.len() % 2 == 1 {
            out.push(0);
        }
        out[0] = out.len() as u8;
        out
    }

    fn iso_with(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut image = vec![0u8; (FIRST_DESCRIPTOR as usize + 3) * SECTOR as usize];

        let root_extent = FIRST_DESCRIPTOR as u32 + 2;
        let mut directory = Vec::new();
        for entry in entries {
            directory.extend_from_slice(entry);
        }

        let pvd = FIRST_DESCRIPTOR as usize * SECTOR as usize;
        image[pvd] = DESCRIPTOR_PRIMARY;
        image[pvd + 1..pvd + 6].copy_from_slice(b"CD001");
        let root = record("\u{0}", true, root_extent, SECTOR as u32);
        image[pvd + 156..pvd + 156 + root.len()].copy_from_slice(&root);

        let terminator = pvd + SECTOR as usize;
        image[terminator] = DESCRIPTOR_TERMINATOR;
        image[terminator + 1..terminator + 6].copy_from_slice(b"CD001");

        let start = root_extent as usize * SECTOR as usize;
        image[start..start + directory.len()].copy_from_slice(&directory);
        image
    }

    fn write(image: &[u8]) -> (tempfile::TempDir, std::path::PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("test.iso");
        std::fs::write(&path, image).unwrap();
        (temp, path)
    }

    #[test]
    fn the_root_directory_lists_what_is_in_it() {
        let payload_extent = FIRST_DESCRIPTOR as u32 + 1;
        let (_temp, path) = write(&iso_with(&[
            record("\u{0}", true, payload_extent, 0),
            record("\u{1}", true, payload_extent, 0),
            record("README.TXT;1", false, payload_extent, 4),
            record("drivers", true, payload_extent, 0),
        ]));

        let listed = Iso::open(&path).unwrap().list("/").unwrap();

        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(
            listed[0].name, "README.TXT",
            "the version suffix is dropped"
        );
        assert!(!listed[0].is_dir);
        assert_eq!(listed[1].name, "drivers");
        assert!(listed[1].is_dir);
    }

    #[test]
    fn a_path_that_is_not_there_names_the_part_that_is_missing() {
        let (_temp, path) = write(&iso_with(&[record("viostor", true, 18, 0)]));

        let err = Iso::open(&path)
            .unwrap()
            .list("viostor/w11/ARM64")
            .unwrap_err()
            .to_string();

        assert!(err.contains("w11"), "{err}");
    }

    #[test]
    fn a_length_that_runs_past_the_image_is_refused_before_it_is_allocated() {
        let (_temp, path) = write(&iso_with(&[record("HUGE.BIN", false, 18, u32::MAX)]));

        let err = Iso::open(&path).unwrap().read("HUGE.BIN").unwrap_err();

        assert!(err.to_string().contains("past the end"), "{err}");
    }

    #[test]
    fn a_file_that_is_not_an_iso_is_refused_rather_than_misread() {
        let (_temp, path) = write(&vec![0u8; 64 * 1024]);

        let err = format!("{:#}", Iso::open(&path).err().expect("not an ISO"));

        assert!(err.contains("ISO9660"), "{err}");
    }
}
