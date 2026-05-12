use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use dirs_next::data_dir;
use tokio::fs;

use crate::gdu::{GduNode, SnapshotTree, SNAPSHOT_FILE_SUFFIX, export_snapshot};

const APPLICATION: &str = "gdu-diff";
const MAX_BUCKET_NAME_LEN: usize = 120;
const MAX_SHOTS_PER_BUCKET: usize = 3;

#[derive(Clone, Debug)]
pub struct StoredSnapshot {
    pub source: PathBuf,
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

    pub async fn save_shot(&self, target: &Path) -> Result<StoredSnapshot> {
        let canonical_target = canonicalize_dir(target).await?;
        let bucket = self.bucket_dir_for(&canonical_target);
        fs::create_dir_all(&bucket)
            .await
            .with_context(|| format!("failed to create snapshot directory {}", bucket.display()))?;

        let temp_path = bucket.join(format!("pending-{}{SNAPSHOT_FILE_SUFFIX}", unix_millis()?));
        export_snapshot(&canonical_target, &temp_path).await?;
        let snapshot =
            SnapshotTree::load_with_label(temp_path.clone(), String::from("latest")).await?;
        let final_path = self
            .unique_snapshot_path(&bucket, snapshot.exported_at)
            .await?;
        fs::rename(&temp_path, &final_path).await.with_context(|| {
            format!(
                "failed to move snapshot {} to {}",
                temp_path.display(),
                final_path.display()
            )
        })?;
        self.prune_bucket(&bucket).await?;
        Ok(StoredSnapshot { source: final_path })
    }

    pub async fn find_nth_latest_path_for(
        &self,
        target: &Path,
        ordinal_from_newest: usize,
    ) -> Result<Option<PathBuf>> {
        if ordinal_from_newest == 0 {
            bail!("shot index must start at 1");
        }

        let canonical_target = canonicalize_dir(target).await?;
        let bucket = self.bucket_dir_for(&canonical_target);
        match fs::metadata(&bucket).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to read snapshot directory {}", bucket.display())
                });
            }
        }

        let paths = self.list_ordered_snapshot_paths_in_bucket(&bucket).await?;
        Ok(paths.into_iter().nth(ordinal_from_newest - 1))
    }

    fn bucket_dir_for(&self, canonical_target: &Path) -> PathBuf {
        self.snapshots_dir
            .join(encode_bucket_name(&canonical_target.to_string_lossy()))
    }

    async fn unique_snapshot_path(
        &self,
        bucket: &Path,
        exported_at: Option<u64>,
    ) -> Result<PathBuf> {
        let stem = exported_at
            .map(|value| format!("shot-{value}"))
            .unwrap_or_else(|| format!("shot-{}", unix_millis().unwrap_or_default()));
        let primary = bucket.join(format!("{stem}{SNAPSHOT_FILE_SUFFIX}"));
        if !path_exists(&primary).await? {
            return Ok(primary);
        }

        for index in 1..1000 {
            let candidate = bucket.join(format!("{stem}-{index}{SNAPSHOT_FILE_SUFFIX}"));
            if !path_exists(&candidate).await? {
                return Ok(candidate);
            }
        }

        bail!(
            "too many snapshots with the same timestamp in {}",
            bucket.display()
        )
    }

    async fn prune_bucket(&self, bucket: &Path) -> Result<()> {
        let paths = self.list_ordered_snapshot_paths_in_bucket(bucket).await?;
        if paths.len() <= MAX_SHOTS_PER_BUCKET {
            return Ok(());
        }

        let protected_path = find_smallest_snapshot_path(&paths).await?;
        for path in paths.into_iter().skip(MAX_SHOTS_PER_BUCKET) {
            if protected_path.as_ref() == Some(&path) {
                continue;
            }
            fs::remove_file(&path)
                .await
                .with_context(|| format!("failed to remove old snapshot {}", path.display()))?;
        }
        Ok(())
    }

    async fn list_ordered_snapshot_paths_in_bucket(&self, bucket: &Path) -> Result<Vec<PathBuf>> {
        let mut paths = Vec::new();
        let mut entries = fs::read_dir(bucket)
            .await
            .with_context(|| format!("failed to read snapshot directory {}", bucket.display()))?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or_default();
            if !(file_name.ends_with(".json") || file_name.ends_with(SNAPSHOT_FILE_SUFFIX)) {
                continue;
            }
            paths.push(path);
        }
        paths.sort_by(|left, right| compare_snapshot_path_order(left, right));
        Ok(paths)
    }
}

pub async fn canonicalize_dir(target: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(target)
        .await
        .with_context(|| format!("failed to resolve path {}", target.display()))?;
    let metadata = fs::metadata(&canonical)
        .await
        .with_context(|| format!("failed to stat {}", canonical.display()))?;
    if !metadata.is_dir() {
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
    let file_name = path.file_name()?.to_str()?;
    let stem = file_name
        .strip_suffix(".json.zst")
        .or_else(|| file_name.strip_suffix(".json"))
        .unwrap_or(file_name);
    let suffix = stem.strip_prefix("shot-")?;
    let (timestamp, ordinal) = match suffix.split_once('-') {
        Some((timestamp, ordinal)) => (timestamp, ordinal.parse().ok()?),
        None => (suffix, 0),
    };
    Some((timestamp.parse().ok()?, ordinal))
}

async fn find_smallest_snapshot_path(paths: &[PathBuf]) -> Result<Option<PathBuf>> {
    let mut smallest: Option<(u64, PathBuf)> = None;
    for path in paths {
        let snapshot = SnapshotTree::load_with_progress(path.clone(), || {}).await?;
        let size = snapshot_disk_usage(&snapshot.root);
        match &smallest {
            Some((smallest_size, _)) if *smallest_size < size => {}
            _ => smallest = Some((size, path.clone())),
        }
    }
    Ok(smallest.map(|(_, path)| path))
}

fn snapshot_disk_usage(node: &GduNode) -> u64 {
    match node {
        GduNode::File(file) => file.disk_size,
        GduNode::Dir(dir) => dir.children.iter().map(snapshot_disk_usage).sum(),
    }
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

async fn path_exists(path: &Path) -> Result<bool> {
    match fs::metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("failed to stat {}", path.display())),
    }
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
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use tempfile::tempdir;

    use crate::gdu::{SnapshotTree, compress_snapshot_file_blocking};

    use super::{
        MAX_BUCKET_NAME_LEN, MAX_SHOTS_PER_BUCKET, SnapshotStore, canonicalize_dir,
        compare_snapshot_path_order, encode_bucket_name, find_smallest_snapshot_path,
        snapshot_disk_usage,
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
        let older = Path::new("/tmp/shot-10.json.zst");
        let newer = Path::new("/tmp/shot-20.json.zst");
        let duplicate = Path::new("/tmp/shot-20-1.json.zst");

        assert!(compare_snapshot_path_order(newer, older).is_lt());
        assert!(compare_snapshot_path_order(duplicate, newer).is_lt());
    }

    #[test]
    fn store_can_be_created() -> Result<()> {
        let store = SnapshotStore::new()?;
        assert!(store.data_dir().is_absolute());
        Ok(())
    }

    #[tokio::test]
    async fn find_nth_latest_for_returns_shots_from_newest_to_oldest() -> Result<()> {
        let dir = tempdir()?;
        let snapshots_dir = dir.path().join("snapshots");
        let target = dir.path().join("target");
        fs::create_dir_all(&target)?;
        let canonical_target = canonicalize_dir(&target).await?;
        let bucket = snapshots_dir.join(encode_bucket_name(&canonical_target.to_string_lossy()));
        fs::create_dir_all(&bucket)?;

        for timestamp in [10, 20, 30, 40] {
            let path = bucket.join(format!("shot-{timestamp}.json.zst"));
            write_compressed_snapshot(
                &path,
                &format!(
                    r#"[1,2,{{"progname":"gdu","progver":"v0","timestamp":{timestamp}}},[{{"name":"/root","mtime":1}},{{"name":"a","asize":1,"dsize":1,"mtime":1}}]]"#
                ),
            )?;
        }

        let store = SnapshotStore {
            data_dir: dir.path().to_path_buf(),
            snapshots_dir,
        };
        let newest = store
            .find_nth_latest_path_for(&canonical_target, 1)
            .await?
            .expect("newest snapshot");
        let second = store
            .find_nth_latest_path_for(&canonical_target, 2)
            .await?
            .expect("second newest snapshot");
        let newest_snapshot = SnapshotTree::load_with_label(newest, String::from("latest"))
            .await
            .expect("latest snapshot");
        let second_snapshot = SnapshotTree::load_with_label(second, String::from("previous"))
            .await
            .expect("previous snapshot");

        assert_eq!(newest_snapshot.exported_at, Some(40));
        assert_eq!(second_snapshot.exported_at, Some(30));
        Ok(())
    }

    #[tokio::test]
    async fn find_nth_latest_for_only_loads_selected_snapshot() -> Result<()> {
        let dir = tempdir()?;
        let snapshots_dir = dir.path().join("snapshots");
        let target = dir.path().join("target");
        fs::create_dir_all(&target)?;
        let canonical_target = canonicalize_dir(&target).await?;
        let bucket = snapshots_dir.join(encode_bucket_name(&canonical_target.to_string_lossy()));
        fs::create_dir_all(&bucket)?;

        fs::write(bucket.join("shot-10.json.zst"), "{not valid json")?;
        write_compressed_snapshot(
            &bucket.join("shot-20.json.zst"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},{"name":"a","asize":1,"dsize":1,"mtime":1}]]"#,
        )?;

        let store = SnapshotStore {
            data_dir: dir.path().to_path_buf(),
            snapshots_dir,
        };
        let newest = store
            .find_nth_latest_path_for(&canonical_target, 1)
            .await?
            .expect("newest snapshot");
        let newest_snapshot = SnapshotTree::load_with_progress(newest.clone(), || {}).await?;

        assert_eq!(
            newest.file_name().and_then(OsStr::to_str),
            Some("shot-20.json.zst")
        );
        assert_eq!(newest_snapshot.label, "shot-20");
        assert_eq!(newest_snapshot.exported_at, Some(20));
        Ok(())
    }

    #[tokio::test]
    async fn find_nth_latest_path_for_returns_path_without_parsing_snapshot() -> Result<()> {
        let dir = tempdir()?;
        let snapshots_dir = dir.path().join("snapshots");
        let target = dir.path().join("target");
        fs::create_dir_all(&target)?;
        let canonical_target = canonicalize_dir(&target).await?;
        let bucket = snapshots_dir.join(encode_bucket_name(&canonical_target.to_string_lossy()));
        fs::create_dir_all(&bucket)?;

        let snapshot_path = bucket.join("shot-10.json.zst");
        fs::write(&snapshot_path, "not-json")?;

        let store = SnapshotStore {
            data_dir: dir.path().to_path_buf(),
            snapshots_dir,
        };
        let resolved = store
            .find_nth_latest_path_for(&canonical_target, 1)
            .await?
            .expect("snapshot path");

        assert_eq!(resolved, snapshot_path);
        Ok(())
    }

    #[tokio::test]
    async fn prune_bucket_keeps_only_latest_three_shots() -> Result<()> {
        let dir = tempdir()?;
        let snapshots_dir = dir.path().join("snapshots");
        let bucket = snapshots_dir.join("bucket");
        fs::create_dir_all(&bucket)?;

        for (timestamp, size) in [(10, 100), (20, 10), (30, 20), (40, 30)] {
            let path = bucket.join(format!("shot-{timestamp}.json.zst"));
            write_compressed_snapshot(
                &path,
                &format!(
                    r#"[1,2,{{"progname":"gdu","progver":"v0","timestamp":{timestamp}}},[{{"name":"/root","mtime":1}},{{"name":"a","asize":{size},"dsize":{size},"mtime":1}}]]"#
                ),
            )?;
        }

        let store = SnapshotStore {
            data_dir: dir.path().to_path_buf(),
            snapshots_dir,
        };
        store.prune_bucket(&bucket).await?;

        let mut remaining = store
            .list_ordered_snapshot_paths_in_bucket(&bucket)
            .await?
            .into_iter()
            .filter_map(|path| {
                snapshot_stem(&path)
                    .and_then(|stem| stem.strip_prefix("shot-"))
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .collect::<Vec<_>>();
        remaining.sort_unstable();

        assert_eq!(remaining.len(), MAX_SHOTS_PER_BUCKET);
        assert_eq!(remaining, vec![20, 30, 40]);
        Ok(())
    }

    #[tokio::test]
    async fn prune_bucket_keeps_smallest_snapshot_even_if_it_is_oldest() -> Result<()> {
        let dir = tempdir()?;
        let snapshots_dir = dir.path().join("snapshots");
        let bucket = snapshots_dir.join("bucket");
        fs::create_dir_all(&bucket)?;

        for (timestamp, size) in [(10, 1), (20, 20), (30, 30), (40, 40)] {
            let path = bucket.join(format!("shot-{timestamp}.json.zst"));
            write_compressed_snapshot(
                &path,
                &format!(
                    r#"[1,2,{{"progname":"gdu","progver":"v0","timestamp":{timestamp}}},[{{"name":"/root","mtime":1}},{{"name":"a","asize":{size},"dsize":{size},"mtime":1}}]]"#
                ),
            )?;
        }

        let store = SnapshotStore {
            data_dir: dir.path().to_path_buf(),
            snapshots_dir,
        };
        store.prune_bucket(&bucket).await?;

        let mut remaining = store
            .list_ordered_snapshot_paths_in_bucket(&bucket)
            .await?
            .into_iter()
            .filter_map(|path| {
                snapshot_stem(&path)
                    .and_then(|stem| stem.strip_prefix("shot-"))
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .collect::<Vec<_>>();
        remaining.sort_unstable();

        assert_eq!(remaining, vec![10, 20, 30, 40]);
        Ok(())
    }

    #[tokio::test]
    async fn finds_oldest_smallest_snapshot_when_sizes_tie() -> Result<()> {
        let dir = tempdir()?;
        let bucket = dir.path().join("bucket");
        fs::create_dir_all(&bucket)?;
        let first = bucket.join("shot-10.json.zst");
        let second = bucket.join("shot-20.json.zst");

        for path in [&first, &second] {
            write_compressed_snapshot(
                path,
                r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},{"name":"a","asize":5,"dsize":5,"mtime":1}]]"#,
            )?;
        }

        let ordered = vec![second.clone(), first.clone()];
        let protected = find_smallest_snapshot_path(&ordered).await?;

        assert_eq!(protected, Some(first));
        Ok(())
    }

    #[test]
    fn computes_snapshot_disk_usage_recursively() -> Result<()> {
        let snapshot = SnapshotTree::from_json_str(
            "latest".into(),
            PathBuf::from("sample.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},[{"name":"dir","mtime":1},{"name":"a","asize":1,"dsize":2,"mtime":1},{"name":"b","asize":1,"dsize":3,"mtime":1}],{"name":"c","asize":1,"dsize":5,"mtime":1}]]"#,
        )?;

        assert_eq!(snapshot_disk_usage(&snapshot.root), 10);
        Ok(())
    }

    fn write_compressed_snapshot(path: &Path, content: &str) -> Result<()> {
        let raw_path = path.with_extension("json");
        fs::write(&raw_path, content)?;
        compress_snapshot_file_blocking(&raw_path, path)?;
        fs::remove_file(raw_path)?;
        Ok(())
    }

    fn snapshot_stem(path: &Path) -> Option<&str> {
        let file_name = path.file_name()?.to_str()?;
        file_name
            .strip_suffix(".json.zst")
            .or_else(|| file_name.strip_suffix(".json"))
    }
}
