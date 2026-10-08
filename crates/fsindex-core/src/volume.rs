//! Volume discovery and lifecycle.
//!
//! A "volume" is the device backing a root (identified by `st_dev`). We learn
//! its mount point and a human name from `/proc/self/mountinfo` so results can
//! be labelled and so an unmounted volume can be marked offline without
//! disturbing the rest of the index.

use crate::index::dev_of;
use crate::model::VolumeId;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct VolumeInfo {
    pub id: VolumeId,
    pub name: String,
    pub mount: String,
}

#[derive(Clone, Debug)]
struct MountEntry {
    mount_point: String,
    source: String,
}

fn read_mounts() -> Vec<MountEntry> {
    let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        // <pre> - <fstype> <source> <superopts>
        let Some((pre, post)) = line.split_once(" - ") else {
            continue;
        };
        let pre_fields: Vec<&str> = pre.split_whitespace().collect();
        if pre_fields.len() < 5 {
            continue;
        }
        let mount_point = decode_mount_field(pre_fields[4]);
        let post_fields: Vec<&str> = post.split_whitespace().collect();
        let source = post_fields.get(1).copied().unwrap_or("").to_string();
        out.push(MountEntry {
            mount_point,
            source,
        });
    }
    out
}

/// mountinfo escapes space as `\040` and a few other characters.
fn decode_mount_field(s: &str) -> String {
    s.replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

fn longest_mount_for<'a>(path: &str, mounts: &'a [MountEntry]) -> Option<&'a MountEntry> {
    mounts
        .iter()
        .filter(|m| {
            path == m.mount_point
                || (path.starts_with(&m.mount_point)
                    && path.as_bytes().get(m.mount_point.len()) == Some(&b'/'))
                || m.mount_point == "/"
        })
        .max_by_key(|m| m.mount_point.len())
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// Probe the volume backing `path`. Returns `None` when the path cannot be
/// stat'd (unmounted or missing).
pub fn probe(path: &Path) -> Option<VolumeInfo> {
    let meta = std::fs::metadata(path).ok()?;
    let id = dev_of(&meta);
    let canon = canonical(path);
    let canon_str = canon.to_string_lossy().to_string();
    let mounts = read_mounts();
    let entry = longest_mount_for(&canon_str, &mounts);
    let mount = entry
        .map(|m| m.mount_point.clone())
        .unwrap_or_else(|| "/".to_string());
    let name = entry
        .and_then(|m| {
            if m.source.starts_with('/') {
                Path::new(&m.source)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
            } else {
                None
            }
        })
        .or_else(|| {
            Path::new(&mount)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "root".to_string());
    Some(VolumeInfo { id, name, mount })
}

/// Re-probe the device id at a mount point; used for periodic online/offline
/// checks. Returns the new id (which changes when a different device is now
/// mounted at the same path).
pub fn probe_id(path: &Path) -> Option<VolumeId> {
    std::fs::metadata(path).ok().map(|m| dev_of(&m))
}

/// True if a mount point currently exists and is a directory.
pub fn mount_point_present(mount: &str) -> bool {
    Path::new(mount).is_dir()
}
