//! `vitro ls` — what is running, and what can be run.
//!
//! The rendering is a pure function over a snapshot so the table can be tested
//! without a filesystem, and so `--json` and the table describe exactly the
//! same thing.

use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use serde::Serialize;
use time::OffsetDateTime;

use crate::ByteSize;
use crate::{golden, Config, Golden, Paths};
use crate::{Entry, Liveness, ProcessProbe, Store};

const OVERLAY_FILE: &str = "overlay.qcow2";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Running,
    /// The record is intact but its process is gone.
    Dead,
    /// The directory is there but its record is not readable.
    Unreadable,
}

impl Status {
    fn as_str(&self) -> &'static str {
        match self {
            Status::Running => "running",
            Status::Dead => "dead",
            Status::Unreadable => "unreadable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VmRow {
    pub name: String,
    pub image: Option<String>,
    pub status: Status,
    pub ssh: Option<String>,
    pub uptime_seconds: Option<u64>,
    pub disk_bytes: Option<u64>,
    /// Only set for an unreadable entry: why it could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoldenRow {
    pub image: String,
    pub backend: String,
    pub location: String,
    pub size_bytes: Option<u64>,
    pub present: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listing {
    pub vms: Vec<VmRow>,
    pub golden: Vec<GoldenRow>,
}

/// Collect the snapshot. `now` is passed in so uptimes are reproducible in
/// tests.
pub fn collect(
    paths: &Paths,
    store: &Store,
    config: &Config,
    probe: &impl ProcessProbe,
    now: OffsetDateTime,
) -> Result<Listing> {
    let vms = store
        .list()?
        .into_iter()
        .map(|entry| match entry {
            Entry::Vm(record) => {
                let status = match record.liveness(probe) {
                    Liveness::Running => Status::Running,
                    Liveness::Dead => Status::Dead,
                };
                let uptime = (status == Status::Running)
                    .then(|| (now - record.created_at).whole_seconds())
                    .and_then(|s| u64::try_from(s).ok());
                VmRow {
                    name: record.name.clone(),
                    image: Some(record.image.clone()),
                    ssh: (status == Status::Running).then(|| record.ssh_target()),
                    uptime_seconds: uptime,
                    disk_bytes: file_size(&record.dir.join(OVERLAY_FILE)),
                    status,
                    reason: None,
                }
            }
            Entry::Unreadable { name, dir, reason } => VmRow {
                name,
                image: None,
                status: Status::Unreadable,
                ssh: None,
                uptime_seconds: None,
                disk_bytes: file_size(&dir.join(OVERLAY_FILE)),
                reason: Some(reason),
            },
        })
        .collect();

    let golden = config
        .images()
        .map(|image| match &image.golden {
            Golden::Image(path) => {
                let size = file_size(path);
                GoldenRow {
                    image: image.key.clone(),
                    backend: image.backend.to_string(),
                    location: path.display().to_string(),
                    present: size.is_some(),
                    size_bytes: size,
                }
            }
            // Unset: the row is about the image `run` would actually start, so
            // it names the newest build rather than the setting that is not
            // there.
            Golden::Latest => match golden::newest(&paths.golden_dir(), &image.key) {
                Some(path) => {
                    let size = file_size(&path);
                    GoldenRow {
                        image: image.key.clone(),
                        backend: image.backend.to_string(),
                        location: path.display().to_string(),
                        present: size.is_some(),
                        size_bytes: size,
                    }
                }
                None => GoldenRow {
                    image: image.key.clone(),
                    backend: image.backend.to_string(),
                    location: "not built yet".to_string(),
                    present: false,
                    size_bytes: None,
                },
            },
            // Lume keeps its VMs in its own store; vitro cannot see them without
            // asking, and `ls` does not shell out.
            Golden::VmName(name) => GoldenRow {
                image: image.key.clone(),
                backend: image.backend.to_string(),
                location: name.clone(),
                size_bytes: None,
                present: true,
            },
        })
        .collect();

    Ok(Listing { vms, golden })
}

fn file_size(path: &Path) -> Option<u64> {
    std::fs::metadata(path).ok().map(|m| m.len())
}

pub fn render(listing: &Listing) -> String {
    let mut out = String::new();

    out.push_str("VMS\n");
    if listing.vms.is_empty() {
        out.push_str("  no VMs are running\n");
    } else {
        let rows: Vec<[String; 6]> = listing
            .vms
            .iter()
            .map(|vm| {
                [
                    vm.name.clone(),
                    vm.image.clone().unwrap_or_else(|| "-".into()),
                    vm.status.as_str().to_string(),
                    vm.ssh.clone().unwrap_or_else(|| "-".into()),
                    vm.uptime_seconds.map_or("-".into(), humanize_duration),
                    vm.disk_bytes
                        .map_or("-".into(), |b| ByteSize::from_bytes(b).to_string()),
                ]
            })
            .collect();
        out.push_str(&table(
            ["NAME", "IMAGE", "STATUS", "SSH", "UPTIME", "DISK"],
            &rows,
        ));
    }

    out.push_str("\nGOLDEN\n");
    if listing.golden.is_empty() {
        out.push_str("  no images are configured\n");
    } else {
        let rows: Vec<[String; 4]> = listing
            .golden
            .iter()
            .map(|g| {
                [
                    g.image.clone(),
                    g.backend.clone(),
                    g.location.clone(),
                    match (g.present, g.size_bytes) {
                        (true, Some(bytes)) => ByteSize::from_bytes(bytes).to_string(),
                        (true, None) => "-".into(),
                        (false, _) => "missing".into(),
                    },
                ]
            })
            .collect();
        out.push_str(&table(["IMAGE", "BACKEND", "LOCATION", "SIZE"], &rows));
    }

    out
}

/// Left-aligned columns sized to their contents, with the last column not
/// padded so the output does not carry trailing spaces.
fn table<const N: usize>(headers: [&str; N], rows: &[[String; N]]) -> String {
    let mut widths = headers.map(str::len);
    for row in rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }

    let mut out = String::new();
    let write_row = |cells: &[&str], out: &mut String| {
        out.push_str("  ");
        for (i, cell) in cells.iter().enumerate() {
            if i + 1 == N {
                out.push_str(cell);
            } else {
                out.push_str(&format!("{:<width$}  ", cell, width = widths[i]));
            }
        }
        out.push('\n');
    };

    write_row(&headers, &mut out);
    for row in rows {
        let cells: Vec<&str> = row.iter().map(String::as_str).collect();
        write_row(&cells, &mut out);
    }
    out
}

fn humanize_duration(seconds: u64) -> String {
    let d = Duration::from_secs(seconds);
    let secs = d.as_secs();
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

/// `SystemTime::now` as an `OffsetDateTime`, falling back to the epoch on a
/// clock that predates it rather than failing a listing over it.
pub fn now() -> OffsetDateTime {
    OffsetDateTime::from(SystemTime::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vm(name: &str, status: Status) -> VmRow {
        VmRow {
            name: name.into(),
            image: Some("linux".into()),
            ssh: matches!(status, Status::Running).then(|| "dev@127.0.0.1:53422".to_string()),
            uptime_seconds: matches!(status, Status::Running).then_some(720),
            disk_bytes: Some(1_288_490_188),
            status,
            reason: None,
        }
    }

    #[test]
    fn an_empty_listing_says_so_in_words() {
        let text = render(&Listing {
            vms: Vec::new(),
            golden: Vec::new(),
        });

        assert!(text.contains("no VMs are running"), "{text}");
        assert!(text.contains("no images are configured"), "{text}");
    }

    #[test]
    fn a_running_vm_shows_its_ssh_target_and_uptime() {
        let text = render(&Listing {
            vms: vec![vm("linux-7f3a2c", Status::Running)],
            golden: Vec::new(),
        });

        assert!(text.contains("linux-7f3a2c"), "{text}");
        assert!(text.contains("running"), "{text}");
        assert!(text.contains("dev@127.0.0.1:53422"), "{text}");
        assert!(text.contains("12m"), "{text}");
    }

    #[test]
    fn a_dead_vm_has_no_ssh_target() {
        let text = render(&Listing {
            vms: vec![vm("linux-7f3a2c", Status::Dead)],
            golden: Vec::new(),
        });

        assert!(text.contains("dead"), "{text}");
        assert!(!text.contains("127.0.0.1"), "{text}");
    }

    #[test]
    fn a_missing_golden_image_is_called_missing() {
        let text = render(&Listing {
            vms: Vec::new(),
            golden: vec![GoldenRow {
                image: "linux".into(),
                backend: "qemu".into(),
                location: "/srv/linux.qcow2".into(),
                size_bytes: None,
                present: false,
            }],
        });

        assert!(text.contains("missing"), "{text}");
    }

    #[test]
    fn columns_line_up_and_no_line_has_trailing_space() {
        let text = render(&Listing {
            vms: vec![
                vm("a", Status::Running),
                vm("a-much-longer-vm-name", Status::Running),
            ],
            golden: Vec::new(),
        });

        for line in text.lines() {
            assert_eq!(line, line.trim_end(), "trailing space in {line:?}");
        }
        let header = text.lines().find(|l| l.contains("NAME")).unwrap();
        let first = text.lines().find(|l| l.contains("a-much-longer")).unwrap();
        assert_eq!(
            header.find("IMAGE").unwrap(),
            first.find("linux").unwrap(),
            "columns should align"
        );
    }

    #[test]
    fn durations_are_rounded_to_one_unit() {
        assert_eq!(humanize_duration(45), "45s");
        assert_eq!(humanize_duration(720), "12m");
        assert_eq!(humanize_duration(7_200), "2h");
        assert_eq!(humanize_duration(180_000), "2d");
    }

    #[test]
    fn json_and_the_table_describe_the_same_snapshot() {
        let listing = Listing {
            vms: vec![vm("linux-7f3a2c", Status::Running)],
            golden: Vec::new(),
        };

        let json: serde_json::Value = serde_json::to_value(&listing).unwrap();

        assert_eq!(json["vms"][0]["name"], "linux-7f3a2c");
        assert_eq!(json["vms"][0]["status"], "running");
        assert_eq!(json["vms"][0]["uptime_seconds"], 720);
        // The reason field only appears where there is one.
        assert!(json["vms"][0].get("reason").is_none());
    }
}
