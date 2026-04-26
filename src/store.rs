use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use dirs_next::data_dir;

use crate::gdu::{SnapshotTree, export_snapshot};

const APPLICATION: &str = "gdu-diff";
const MAX_BUCKET_NAME_LEN: usize = 120;

#[derive(Clone, Debug)]
pub struct StoredSnapshot {
    pub source: PathBuf,
    pub snapshot: SnapshotTree,
}

pub struct SnapshotStore {
    data_dir: PathBuf,
    snapshots_dir: PathBuf,
}

impl SnapshotStore {
    pub fn new() -> Result<Self> {
        let base_dir = data_dir()
            .ok_or_else(|| anyhow!("failed to determine user data directory from dirs-next"))?;
        let data_dir = base_dir.join(APPLICATION);
        let snapshots_dir = data_dir.join("snapshots");
        Ok(Self {
            data_dir,
            snapshots_dir,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn save_shot(&self, target: &Path) -> Result<StoredSnapshot> {
        let canonical_target = canonicalize_dir(target)?;
        let bucket = self.bucket_dir_for(&canonical_target);
        fs::create_dir_all(&bucket)
            .with_context(|| format!("failed to create snapshot directory {}", bucket.display()))?;

        let temp_path = bucket.join(format!("pending-{}.json", unix_millis()?));
        export_snapshot(&canonical_target, &temp_path)?;
        let snapshot = SnapshotTree::load_with_label(temp_path.clone(), String::from("latest"))?;
        let final_path = self.unique_snapshot_path(&bucket, snapshot.exported_at)?;
        fs::rename(&temp_path, &final_path).with_context(|| {
            format!(
                "failed to move snapshot {} to {}",
                temp_path.display(),
                final_path.display()
            )
        })?;

        let label = final_path
            .file_stem()
            .and_then(OsStr::to_str)
            .map_or_else(|| String::from("snapshot"), str::to_owned);
        let snapshot = SnapshotTree::load_with_label(final_path.clone(), label)?;
        Ok(StoredSnapshot {
            source: final_path,
            snapshot,
        })
    }

    pub fn find_latest_for(&self, target: &Path) -> Result<Option<StoredSnapshot>> {
        let canonical_target = canonicalize_dir(target)?;
        let bucket = self.bucket_dir_for(&canonical_target);
        if !bucket.is_dir() {
            return Ok(None);
        }

        let mut best: Option<StoredSnapshot> = None;
        for entry in fs::read_dir(&bucket)
            .with_context(|| format!("failed to read snapshot directory {}", bucket.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str) != Some("json") {
                continue;
            }

            let label = path
                .file_stem()
                .and_then(OsStr::to_str)
                .map_or_else(|| String::from("snapshot"), str::to_owned);
            let snapshot = SnapshotTree::load_with_label(path.clone(), label)?;
            let candidate = StoredSnapshot {
                source: path,
                snapshot,
            };

            let replace = match &best {
                Some(current) => compare_snapshot_order(&candidate, current).is_gt(),
                None => true,
            };
            if replace {
                best = Some(candidate);
            }
        }

        Ok(best)
    }

    fn bucket_dir_for(&self, canonical_target: &Path) -> PathBuf {
        self.snapshots_dir
            .join(encode_bucket_name(&canonical_target.to_string_lossy()))
    }

    fn unique_snapshot_path(&self, bucket: &Path, exported_at: Option<u64>) -> Result<PathBuf> {
        let stem = exported_at
            .map(|value| format!("shot-{value}"))
            .unwrap_or_else(|| format!("shot-{}", unix_millis().unwrap_or_default()));
        let primary = bucket.join(format!("{stem}.json"));
        if !primary.exists() {
            return Ok(primary);
        }

        for index in 1..1000 {
            let candidate = bucket.join(format!("{stem}-{index}.json"));
            if !candidate.exists() {
                return Ok(candidate);
            }
        }

        bail!(
            "too many snapshots with the same timestamp in {}",
            bucket.display()
        )
    }
}

pub fn canonicalize_dir(target: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(target)
        .with_context(|| format!("failed to resolve path {}", target.display()))?;
    if !canonical.is_dir() {
        bail!("{} is not a directory", canonical.display());
    }
    Ok(canonical)
}

fn compare_snapshot_order(left: &StoredSnapshot, right: &StoredSnapshot) -> std::cmp::Ordering {
    left.snapshot
        .exported_at
        .cmp(&right.snapshot.exported_at)
        .then_with(|| left.source.cmp(&right.source))
}

fn encode_bucket_name(input: &str) -> String {
    let mut encoded = String::with_capacity(input.len() * 3);
    for byte in input.as_bytes() {
        match byte {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' => {
                encoded.push(*byte as char);
            }
            _ => {
                encoded.push('_');
                encoded.push(hex(*byte >> 4));
                encoded.push(hex(*byte & 0x0f));
            }
        }
    }

    if encoded.len() <= MAX_BUCKET_NAME_LEN {
        return encoded;
    }

    let hash = stable_hash_suffix(input.as_bytes());
    let prefix_limit = MAX_BUCKET_NAME_LEN.saturating_sub(hash.len() + 2);
    encoded.truncate(prefix_limit);
    while encoded.ends_with('_') {
        encoded.pop();
    }
    if encoded.is_empty() {
        encoded.push_str("path");
    }
    format!("{encoded}__{hash}")
}

fn hex(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + value - 10) as char,
        _ => unreachable!(),
    }
}

fn stable_hash_suffix(bytes: &[u8]) -> String {
    const OFFSET_BASIS: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x00000100000001b3;

    let mut hash = OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    format!("{hash:016X}")
}

fn unix_millis() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{
        MAX_BUCKET_NAME_LEN, SnapshotStore, StoredSnapshot, compare_snapshot_order,
        encode_bucket_name,
    };
    use crate::gdu::SnapshotTree;

    #[test]
    fn bucket_name_is_filesystem_safe() {
        let encoded = encode_bucket_name("/Users/test/dir name");
        assert!(!encoded.contains('/'));
        assert!(encoded.contains("_2F"));
        assert!(encoded.contains("_20"));
    }

    #[test]
    fn long_bucket_name_is_shortened_with_hash() {
        let long_path = format!("/{}", "very-long-segment/".repeat(40));
        let encoded = encode_bucket_name(&long_path);
        assert!(encoded.len() <= MAX_BUCKET_NAME_LEN);
        assert!(encoded.contains("__"));
        assert!(!encoded.contains('/'));
    }

    #[test]
    fn prefers_newer_exported_at() -> Result<()> {
        let dir = tempdir()?;
        let older_path = dir.path().join("shot-10.json");
        let newer_path = dir.path().join("shot-20.json");
        fs::write(
            &older_path,
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},{"name":"a","asize":1,"dsize":1,"mtime":1}]]"#,
        )?;
        fs::write(
            &newer_path,
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},{"name":"a","asize":1,"dsize":1,"mtime":1}]]"#,
        )?;

        let older = StoredSnapshot {
            source: older_path.clone(),
            snapshot: SnapshotTree::load_with_label(older_path, String::from("older"))?,
        };
        let newer = StoredSnapshot {
            source: newer_path.clone(),
            snapshot: SnapshotTree::load_with_label(newer_path, String::from("newer"))?,
        };

        assert!(compare_snapshot_order(&newer, &older).is_gt());
        Ok(())
    }

    #[test]
    fn store_can_be_created() -> Result<()> {
        let store = SnapshotStore::new()?;
        assert!(store.data_dir().is_absolute());
        Ok(())
    }
}
