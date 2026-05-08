use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use dirs_next::data_dir;

use crate::gdu::{SnapshotTree, export_snapshot};

const APPLICATION: &str = "gdu-diff";
const MAX_BUCKET_NAME_LEN: usize = 120;
const MAX_SHOTS_PER_BUCKET: usize = 3;

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
        self.prune_bucket(&bucket)?;
        Ok(StoredSnapshot {
            source: final_path,
            snapshot,
        })
    }

    pub fn find_latest_for(&self, target: &Path) -> Result<Option<StoredSnapshot>> {
        self.find_nth_latest_for(target, 1)
    }

    pub fn find_nth_latest_path_for(
        &self,
        target: &Path,
        ordinal_from_newest: usize,
    ) -> Result<Option<PathBuf>> {
        if ordinal_from_newest == 0 {
            bail!("shot index must start at 1");
        }

        let canonical_target = canonicalize_dir(target)?;
        let bucket = self.bucket_dir_for(&canonical_target);
        if !bucket.is_dir() {
            return Ok(None);
        }

        let paths = self.list_ordered_snapshot_paths_in_bucket(&bucket)?;
        Ok(paths.into_iter().nth(ordinal_from_newest - 1))
    }

    pub fn find_nth_latest_for(
        &self,
        target: &Path,
        ordinal_from_newest: usize,
    ) -> Result<Option<StoredSnapshot>> {
        let Some(path) = self.find_nth_latest_path_for(target, ordinal_from_newest)? else {
            return Ok(None);
        };
        Ok(Some(self.load_snapshot(path)?))
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

    fn prune_bucket(&self, bucket: &Path) -> Result<()> {
        let paths = self.list_ordered_snapshot_paths_in_bucket(bucket)?;
        if paths.len() <= MAX_SHOTS_PER_BUCKET {
            return Ok(());
        }

        for path in paths.into_iter().skip(MAX_SHOTS_PER_BUCKET) {
            fs::remove_file(&path)
                .with_context(|| format!("failed to remove old snapshot {}", path.display()))?;
        }
        Ok(())
    }

    fn list_ordered_snapshot_paths_in_bucket(&self, bucket: &Path) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        for entry in fs::read_dir(bucket)
            .with_context(|| format!("failed to read snapshot directory {}", bucket.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(OsStr::to_str) != Some("json") {
                continue;
            }
            paths.push(path);
        }
        paths.sort_by(|left, right| compare_snapshot_path_order(left, right));
        Ok(paths)
    }

    fn load_snapshot(&self, path: PathBuf) -> Result<StoredSnapshot> {
        let label = path
            .file_stem()
            .and_then(OsStr::to_str)
            .map_or_else(|| String::from("snapshot"), str::to_owned);
        let snapshot = SnapshotTree::load_with_label(path.clone(), label)?;
        Ok(StoredSnapshot {
            source: path,
            snapshot,
        })
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

fn compare_snapshot_path_order(left: &Path, right: &Path) -> std::cmp::Ordering {
    match (snapshot_name_sort_key(left), snapshot_name_sort_key(right)) {
        (Some(left_key), Some(right_key)) => right_key.cmp(&left_key).then_with(|| right.cmp(left)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => right.cmp(left),
    }
}

fn snapshot_name_sort_key(path: &Path) -> Option<(u64, u32)> {
    let stem = path.file_stem()?.to_str()?;
    let suffix = stem.strip_prefix("shot-")?;
    let (timestamp, ordinal) = match suffix.split_once('-') {
        Some((timestamp, ordinal)) => (timestamp, ordinal.parse().ok()?),
        None => (suffix, 0),
    };
    Some((timestamp.parse().ok()?, ordinal))
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
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{
        MAX_BUCKET_NAME_LEN, MAX_SHOTS_PER_BUCKET, SnapshotStore, canonicalize_dir,
        compare_snapshot_path_order, encode_bucket_name,
    };

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
    fn prefers_newer_snapshot_names() {
        let older = Path::new("/tmp/shot-10.json");
        let newer = Path::new("/tmp/shot-20.json");
        let duplicate = Path::new("/tmp/shot-20-1.json");

        assert!(compare_snapshot_path_order(newer, older).is_lt());
        assert!(compare_snapshot_path_order(duplicate, newer).is_lt());
    }

    #[test]
    fn store_can_be_created() -> Result<()> {
        let store = SnapshotStore::new()?;
        assert!(store.data_dir().is_absolute());
        Ok(())
    }

    #[test]
    fn find_nth_latest_for_returns_shots_from_newest_to_oldest() -> Result<()> {
        let dir = tempdir()?;
        let snapshots_dir = dir.path().join("snapshots");
        let target = dir.path().join("target");
        fs::create_dir_all(&target)?;
        let canonical_target = canonicalize_dir(&target)?;
        let bucket = snapshots_dir.join(encode_bucket_name(&canonical_target.to_string_lossy()));
        fs::create_dir_all(&bucket)?;

        for timestamp in [10, 20, 30, 40] {
            let path = bucket.join(format!("shot-{timestamp}.json"));
            fs::write(
                &path,
                format!(
                    r#"[1,2,{{"progname":"gdu","progver":"v0","timestamp":{timestamp}}},[{{"name":"/root","mtime":1}},{{"name":"a","asize":1,"dsize":1,"mtime":1}}]]"#
                ),
            )?;
        }

        let store = SnapshotStore {
            data_dir: dir.path().to_path_buf(),
            snapshots_dir,
        };
        let newest = store
            .find_nth_latest_for(&canonical_target, 1)?
            .expect("newest snapshot");
        let second = store
            .find_nth_latest_for(&canonical_target, 2)?
            .expect("second newest snapshot");

        assert_eq!(newest.snapshot.exported_at, Some(40));
        assert_eq!(second.snapshot.exported_at, Some(30));
        Ok(())
    }

    #[test]
    fn find_nth_latest_for_only_loads_selected_snapshot() -> Result<()> {
        let dir = tempdir()?;
        let snapshots_dir = dir.path().join("snapshots");
        let target = dir.path().join("target");
        fs::create_dir_all(&target)?;
        let canonical_target = canonicalize_dir(&target)?;
        let bucket = snapshots_dir.join(encode_bucket_name(&canonical_target.to_string_lossy()));
        fs::create_dir_all(&bucket)?;

        fs::write(bucket.join("shot-10.json"), "{not valid json")?;
        fs::write(
            bucket.join("shot-20.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},{"name":"a","asize":1,"dsize":1,"mtime":1}]]"#,
        )?;

        let store = SnapshotStore {
            data_dir: dir.path().to_path_buf(),
            snapshots_dir,
        };
        let newest = store
            .find_nth_latest_for(&canonical_target, 1)?
            .expect("newest snapshot");

        assert_eq!(
            newest.source.file_name().and_then(OsStr::to_str),
            Some("shot-20.json")
        );
        assert_eq!(newest.snapshot.exported_at, Some(20));
        Ok(())
    }

    #[test]
    fn find_nth_latest_path_for_returns_path_without_parsing_snapshot() -> Result<()> {
        let dir = tempdir()?;
        let snapshots_dir = dir.path().join("snapshots");
        let target = dir.path().join("target");
        fs::create_dir_all(&target)?;
        let canonical_target = canonicalize_dir(&target)?;
        let bucket = snapshots_dir.join(encode_bucket_name(&canonical_target.to_string_lossy()));
        fs::create_dir_all(&bucket)?;

        let snapshot_path = bucket.join("shot-10.json");
        fs::write(&snapshot_path, "not-json")?;

        let store = SnapshotStore {
            data_dir: dir.path().to_path_buf(),
            snapshots_dir,
        };
        let resolved = store
            .find_nth_latest_path_for(&canonical_target, 1)?
            .expect("snapshot path");

        assert_eq!(resolved, snapshot_path);
        Ok(())
    }

    #[test]
    fn prune_bucket_keeps_only_latest_three_shots() -> Result<()> {
        let dir = tempdir()?;
        let snapshots_dir = dir.path().join("snapshots");
        let bucket = snapshots_dir.join("bucket");
        fs::create_dir_all(&bucket)?;

        for timestamp in [10, 20, 30, 40] {
            let path = bucket.join(format!("shot-{timestamp}.json"));
            fs::write(
                &path,
                format!(
                    r#"[1,2,{{"progname":"gdu","progver":"v0","timestamp":{timestamp}}},[{{"name":"/root","mtime":1}},{{"name":"a","asize":1,"dsize":1,"mtime":1}}]]"#
                ),
            )?;
        }

        let store = SnapshotStore {
            data_dir: dir.path().to_path_buf(),
            snapshots_dir,
        };
        store.prune_bucket(&bucket)?;

        let mut remaining = store
            .list_ordered_snapshot_paths_in_bucket(&bucket)?
            .into_iter()
            .filter_map(|path| {
                path.file_stem()
                    .and_then(OsStr::to_str)
                    .and_then(|stem| stem.strip_prefix("shot-"))
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .collect::<Vec<_>>();
        remaining.sort_unstable();

        assert_eq!(remaining.len(), MAX_SHOTS_PER_BUCKET);
        assert_eq!(remaining, vec![20, 30, 40]);
        Ok(())
    }
}
