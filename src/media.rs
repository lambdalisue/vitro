//! Getting installation media onto the local disk, and proving it arrived
//! intact.
//!
//! Downloads are fetched as bounded byte ranges rather than one open-ended
//! GET, several at a time. Distribution CDNs shape the latter: measured
//! against one vendor's, a whole-file request ran at 2 MB/s while 64 MiB
//! ranges from the same host ran at 40 MB/s, and eight of those at once ran at
//! 156 MB/s. A browser issues the slow shape too, so this is not something a
//! user can work around by fetching the file themselves.
//!
//! The ranges are claimed from a shared counter rather than split evenly, so a
//! slow one holds up nothing but itself.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

/// Large enough that per-request overhead disappears, small enough that a
/// failed range is cheap to retry.
const CHUNK: u64 = 64 * 1024 * 1024;
const ATTEMPTS: usize = 3;

/// How many ranges are in flight. Eight is what the measurement above used;
/// past that the gain flattens and a CDN starts to look at you differently.
const STREAMS: usize = 8;

/// Where a URL's contents are cached, named after the last path segment.
pub fn cache_path(cache_dir: &Path, url: &str) -> PathBuf {
    let name = url
        .rsplit('/')
        .find(|segment| !segment.is_empty())
        .and_then(|segment| segment.split('?').next())
        .filter(|name| !name.is_empty())
        .unwrap_or("download");
    cache_dir.join(name)
}

/// Fetch `url` into `out` unless it is already there and matches `sha256`.
///
/// Progress is reported through `on_progress` so the caller decides how to
/// show it; a multi-gigabyte download with no output looks like a hang.
pub fn fetch(
    url: &str,
    out: &Path,
    sha256: Option<&str>,
    mut on_progress: impl FnMut(u64, u64) + Send,
) -> Result<()> {
    if let Some(expected) = sha256 {
        if out.exists() && checksum(out)? == expected.to_ascii_lowercase() {
            return Ok(());
        }
    }

    let total = content_length(url)?;

    // Without a checksum the length is the only thing there is to go on, and it
    // is better than nothing: fetching twenty gigabytes again on every build
    // because the configuration omits a sha256 is not a useful default.
    if sha256.is_none() && reusable(out, total) {
        eprintln!(
            "vitro: reusing {} without verifying it; set `sha256` to have it checked",
            out.display()
        );
        return Ok(());
    }
    if let Some(parent) = out.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    // Write to a sibling and rename, so an interrupted download never leaves
    // something that looks like a complete image.
    let partial = out.with_extension("part");
    let file =
        File::create(&partial).with_context(|| format!("cannot create {}", partial.display()))?;
    file.set_len(total)?;

    let result = fetch_ranges(url, &file, total, &mut on_progress);
    if result.is_err() {
        drop(file);
        let _ = fs::remove_file(&partial);
        return result;
    }
    file.sync_all()?;
    drop(file);

    if let Some(expected) = sha256 {
        let actual = checksum(&partial)?;
        if actual != expected.to_ascii_lowercase() {
            let _ = fs::remove_file(&partial);
            bail!("{url} does not match the configured sha256\n  expected {expected}\n  got      {actual}");
        }
    }

    fs::rename(&partial, out)
        .with_context(|| format!("cannot move {} into place", partial.display()))?;
    Ok(())
}

/// Whether a file already on disk is the right length to be the one wanted.
fn reusable(path: &Path, total: u64) -> bool {
    fs::metadata(path).is_ok_and(|existing| existing.len() == total)
}

/// Fetch every range of the file, `STREAMS` at a time, into `file`.
fn fetch_ranges(
    url: &str,
    file: &File,
    total: u64,
    on_progress: &mut (impl FnMut(u64, u64) + Send),
) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    let next = AtomicU64::new(0);
    let done = AtomicU64::new(0);
    let failure: Mutex<Option<anyhow::Error>> = Mutex::new(None);
    let progress = Mutex::new(on_progress);

    std::thread::scope(|scope| {
        for _ in 0..STREAMS.min(total.div_ceil(CHUNK) as usize) {
            scope.spawn(|| {
                loop {
                    if failure.lock().unwrap().is_some() {
                        return;
                    }
                    if let Err(e) = crate::signals::check() {
                        *failure.lock().unwrap() = Some(e);
                        return;
                    }

                    let offset = next.fetch_add(CHUNK, Ordering::Relaxed);
                    if offset >= total {
                        return;
                    }
                    let end = (offset + CHUNK).min(total) - 1;

                    match fetch_one(url, file, offset, end) {
                        Ok(written) => {
                            let so_far = done.fetch_add(written, Ordering::Relaxed) + written;
                            // One at a time: the callback is the caller's, and
                            // it is not expecting to be on several threads.
                            (progress.lock().unwrap())(so_far, total);
                        }
                        Err(e) => {
                            let mut slot = failure.lock().unwrap();
                            if slot.is_none() {
                                *slot = Some(e);
                            }
                            return;
                        }
                    }
                }
            });
        }
    });

    match failure.into_inner().unwrap() {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Fetch one range and put it where it belongs, returning how much arrived.
fn fetch_one(url: &str, file: &File, offset: u64, end: u64) -> Result<u64> {
    let bytes = fetch_range(url, offset, end)?;
    let want = (end - offset + 1) as usize;
    if bytes.len() != want {
        bail!(
            "{url} returned {} bytes for range {offset}-{end}, expected {want}",
            bytes.len()
        );
    }
    write_at(file, &bytes, offset)?;
    Ok(bytes.len() as u64)
}

/// Write at an absolute offset without moving a shared file cursor, which is
/// what lets several ranges land at once.
fn write_at(file: &File, mut bytes: &[u8], mut offset: u64) -> Result<()> {
    while !bytes.is_empty() {
        #[cfg(unix)]
        let written = std::os::unix::fs::FileExt::write_at(file, bytes, offset)?;
        #[cfg(windows)]
        let written = std::os::windows::fs::FileExt::seek_write(file, bytes, offset)?;
        if written == 0 {
            bail!("the download stopped being writable at offset {offset}");
        }
        bytes = &bytes[written..];
        offset += written as u64;
    }
    Ok(())
}

fn content_length(url: &str) -> Result<u64> {
    let response = ureq::head(url)
        .call()
        .with_context(|| format!("cannot reach {url}"))?;
    response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| {
            anyhow::anyhow!("{url} did not say how large it is, so it cannot be fetched in ranges")
        })
}

fn fetch_range(url: &str, from: u64, to: u64) -> Result<Vec<u8>> {
    let mut last = None;
    for _ in 0..ATTEMPTS {
        match try_range(url, from, to) {
            Ok(bytes) => return Ok(bytes),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("cannot fetch {url}")))
}

fn try_range(url: &str, from: u64, to: u64) -> Result<Vec<u8>> {
    let mut response = ureq::get(url)
        .header("range", format!("bytes={from}-{to}"))
        .call()
        .with_context(|| format!("cannot fetch bytes {from}-{to} of {url}"))?;
    let mut bytes = Vec::with_capacity((to - from + 1) as usize);
    response.body_mut().as_reader().read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The lowercase hex SHA-256 of a file.
pub fn checksum(path: &Path) -> Result<String> {
    let mut file = File::open(path).with_context(|| format!("cannot read {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

/// Check a file already on disk, so a local path can be verified too.
pub fn verify(path: &Path, sha256: &str) -> Result<()> {
    let actual = checksum(path)?;
    let expected = sha256.to_ascii_lowercase();
    if actual != expected {
        bail!(
            "{} does not match the configured sha256\n  expected {expected}\n  got      {actual}",
            path.display()
        );
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cached_file_of_the_right_length_is_reused_when_there_is_no_checksum() {
        // Not proof it is the right file, but re-fetching twenty gigabytes on
        // every build because the configuration omits a sha256 is worse.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("media.iso");
        fs::write(&path, b"0123456789").unwrap();

        assert!(reusable(&path, 10));
        assert!(!reusable(&path, 11));
        assert!(!reusable(&temp.path().join("absent.iso"), 10));
    }

    #[test]
    fn ranges_land_where_they_belong_whatever_order_they_arrive_in() {
        // Several ranges are in flight at once, so each writes at an absolute
        // offset rather than through a cursor they would otherwise share.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let file = File::create(&path).unwrap();
        file.set_len(9).unwrap();

        write_at(&file, b"ghi", 6).unwrap();
        write_at(&file, b"abc", 0).unwrap();
        write_at(&file, b"def", 3).unwrap();
        drop(file);

        assert_eq!(fs::read(&path).unwrap(), b"abcdefghi");
    }

    #[test]
    fn a_write_past_the_end_grows_the_file_rather_than_being_lost() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("out.bin");
        let file = File::create(&path).unwrap();

        write_at(&file, b"tail", 4).unwrap();
        drop(file);

        let written = fs::read(&path).unwrap();
        assert_eq!(written.len(), 8);
        assert_eq!(&written[4..], b"tail");
    }

    #[test]
    fn a_cached_download_is_named_after_the_last_path_segment() {
        assert_eq!(
            cache_path(
                Path::new("/cache"),
                "https://example.com/a/b/noble-arm64.img"
            ),
            Path::new("/cache/noble-arm64.img")
        );
    }

    #[test]
    fn a_query_string_does_not_become_part_of_the_file_name() {
        // Signed download links carry long, expiring query strings.
        assert_eq!(
            cache_path(
                Path::new("/cache"),
                "https://example.com/Win11.iso?t=abc&P1=123"
            ),
            Path::new("/cache/Win11.iso")
        );
    }

    #[test]
    fn a_url_with_no_usable_segment_still_yields_a_path() {
        assert_eq!(
            cache_path(Path::new("/cache"), "https://example.com/"),
            Path::new("/cache/example.com")
        );
    }

    #[test]
    fn a_checksum_is_lowercase_hex() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("f");
        fs::write(&path, b"abc").unwrap();

        assert_eq!(
            checksum(&path).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verification_accepts_either_case() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("f");
        fs::write(&path, b"abc").unwrap();

        verify(
            &path,
            "BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
        )
        .unwrap();
    }

    #[test]
    fn a_mismatch_shows_both_checksums() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("f");
        fs::write(&path, b"abc").unwrap();

        let err = verify(&path, "00").unwrap_err().to_string();

        assert!(err.contains("expected 00"), "{err}");
        assert!(err.contains("ba7816bf"), "{err}");
    }
}

/// Where a Windows installer ISO comes from, spelled out as steps.
///
/// The same words wherever the question comes up — `setup`, `build` and
/// `doctor` — because three descriptions of one task read as three tasks.
///
/// Numbered and concrete on purpose. vitro is not allowed to fetch this file,
/// so the instruction is the product: a paragraph explaining redistribution
/// leaves somebody still wondering which link to click and where to put what
/// comes back.
pub fn windows_media_instructions(media_dir: &Path, arch: &str) -> String {
    let (page, edition) = if arch == "aarch64" {
        (
            "https://www.microsoft.com/software-download/windows11arm64",
            "Windows 11 (multi-edition ISO for Arm-based PCs)",
        )
    } else {
        (
            "https://www.microsoft.com/software-download/windows11",
            "Windows 11 (multi-edition ISO for x64 devices)",
        )
    };

    format!(
        "Windows needs an installer ISO, and Microsoft does not allow anyone to\n\
         redistribute it — so this is the one part vitro cannot do for you.\n\
         \n  \
         1. Open  {page}\n  \
         2. Choose \"{edition}\", pick a language, and download\n  \
         3. Save it into  {}\n\
         \n\
         The filename does not matter: vitro identifies the media by the volume label\n\
         inside it. That label also says which architecture the ISO is for, and this\n\
         host needs {arch} — the wrong one does not fail, it stalls at the installer's\n\
         first screen, so vitro checks before starting anything.",
        media_dir.display()
    )
}

#[cfg(test)]
mod instruction_tests {
    use super::*;

    #[test]
    fn the_steps_name_the_page_and_the_directory_for_this_host() {
        let text = windows_media_instructions(Path::new("/m/media"), "aarch64");

        assert!(text.contains("windows11arm64"), "{text}");
        assert!(text.contains("/m/media"), "{text}");
        // An x64 reader must not be handed the Arm page.
        let other = windows_media_instructions(Path::new("/m/media"), "x86_64");
        assert!(!other.contains("windows11arm64"), "{other}");
    }
}
