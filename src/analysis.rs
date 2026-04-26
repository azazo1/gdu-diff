use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, anyhow, bail};

use crate::gdu::{GduNode, SnapshotTree};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SizeMetric {
    Disk,
    Apparent,
}

impl SizeMetric {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disk => "disk",
            Self::Apparent => "apparent",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortMode {
    LatestSize,
    Delta,
    ShareDelta,
    Name,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::LatestSize => "latest size",
            Self::Delta => "delta",
            Self::ShareDelta => "share delta",
            Self::Name => "name",
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SizePair {
    pub disk: u64,
    pub apparent: u64,
}

impl SizePair {
    pub fn value(self, metric: SizeMetric) -> u64 {
        match metric {
            SizeMetric::Disk => self.disk,
            SizeMetric::Apparent => self.apparent,
        }
    }
}

#[derive(Clone, Debug)]
pub struct IndexedEntry {
    pub name: String,
    pub parent: Option<String>,
    pub is_dir: bool,
    pub sizes: SizePair,
}

#[derive(Clone, Debug)]
pub struct SnapshotIndex {
    pub label: String,
    pub root_name: String,
    pub entries: BTreeMap<String, IndexedEntry>,
}

impl SnapshotIndex {
    pub fn root_sizes(&self) -> Result<SizePair> {
        self.entries
            .get("")
            .map(|entry| entry.sizes)
            .ok_or_else(|| anyhow!("snapshot {} does not have a root node", self.label))
    }
}

#[derive(Clone, Debug)]
pub struct TimelinePoint {
    pub label: String,
    pub size: u64,
    pub local_share: f64,
    pub root_share: f64,
}

#[derive(Clone, Debug)]
pub struct RowData {
    pub name: String,
    pub path: String,
    pub kind: EntryKind,
    pub change_kind: ChangeKind,
    pub latest_size: u64,
    pub baseline_size: u64,
    pub delta: i64,
    pub latest_local_share: f64,
    pub baseline_local_share: f64,
    pub local_share_delta: f64,
    pub timeline: Vec<TimelinePoint>,
}

impl RowData {
    pub fn latest_root_share(&self) -> f64 {
        self.timeline.last().map_or(0.0, |point| point.root_share)
    }

    pub fn baseline_root_share(&self) -> f64 {
        self.timeline.first().map_or(0.0, |point| point.root_share)
    }

    pub fn root_share_delta(&self) -> f64 {
        self.latest_root_share() - self.baseline_root_share()
    }

    pub fn has_children(&self) -> bool {
        matches!(self.kind, EntryKind::Dir | EntryKind::Mixed)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChangeKind {
    Added,
    Removed,
    Changed,
    Unchanged,
}

impl ChangeKind {
    pub fn label(self) -> &'static str {
        self.short()
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Added => "+",
            Self::Removed => "-",
            Self::Changed => "~",
            Self::Unchanged => "=",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    Dir,
    File,
    Mixed,
}

impl EntryKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dir => "dir",
            Self::File => "file",
            Self::Mixed => "mixed",
        }
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Dir => "D",
            Self::File => "F",
            Self::Mixed => "M",
        }
    }
}

pub struct Analysis {
    snapshots: Vec<SnapshotIndex>,
}

impl Analysis {
    pub fn new(mut snapshots: Vec<SnapshotTree>) -> Result<Self> {
        if snapshots.len() < 2 {
            bail!("please provide at least two gdu export files");
        }
        snapshots.sort_by_key(|snapshot| snapshot.exported_at.unwrap_or(u64::MAX));
        let indices = snapshots
            .into_iter()
            .map(SnapshotIndex::from_snapshot)
            .collect::<Result<Vec<_>>>()?;
        Ok(Self { snapshots: indices })
    }

    pub fn current_root_name(&self) -> &str {
        self.snapshots
            .last()
            .map(|snapshot| snapshot.root_name.as_str())
            .unwrap_or("/")
    }

    pub fn snapshot_count(&self) -> usize {
        self.snapshots.len()
    }

    pub fn snapshot_range_label(&self) -> String {
        match (self.snapshots.first(), self.snapshots.last()) {
            (Some(first), Some(last)) if first.label == last.label => first.label.clone(),
            (Some(first), Some(last)) => format!("{} -> {}", first.label, last.label),
            _ => String::from("-"),
        }
    }

    pub fn parent_path(path: &str) -> Option<String> {
        if path.is_empty() {
            return None;
        }
        match path.rsplit_once('/') {
            Some((parent, _)) => Some(parent.to_string()),
            None => Some(String::new()),
        }
    }

    pub fn display_path(&self, path: &str) -> String {
        if path.is_empty() {
            return self.current_root_name().to_string();
        }
        format!("{}/{}", self.current_root_name(), path)
    }

    pub fn children_of(
        &self,
        parent_path: &str,
        include_files: bool,
        metric: SizeMetric,
        sort: SortMode,
    ) -> Result<Vec<RowData>> {
        let mut children = BTreeSet::new();
        for snapshot in &self.snapshots {
            for entry in snapshot.entries.values() {
                if entry.parent.as_deref() == Some(parent_path) {
                    children.insert(entry.name.clone());
                }
            }
        }

        let rows = children
            .into_iter()
            .map(|name| {
                let child_path = join_path(parent_path, &name);
                self.build_row(&child_path, metric)
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .filter(|row| include_files || row.has_children())
            .collect::<Vec<_>>();

        Ok(sort_rows(rows, sort))
    }

    pub fn row_for_path(&self, path: &str, metric: SizeMetric) -> Result<RowData> {
        if path.is_empty() {
            return self.root_row(metric);
        }

        self.build_row(path, metric)?
            .ok_or_else(|| anyhow!("path {path} does not exist in the snapshot set"))
    }

    fn build_row(&self, path: &str, metric: SizeMetric) -> Result<Option<RowData>> {
        let mut name = None;
        let mut saw_dir = false;
        let mut saw_file = false;
        let mut timeline = Vec::with_capacity(self.snapshots.len());
        let mut first_present = false;
        let mut last_present = false;

        for (index, snapshot) in self.snapshots.iter().enumerate() {
            let entry = snapshot.entries.get(path);
            if let Some(entry) = entry {
                name.get_or_insert_with(|| entry.name.clone());
                saw_dir |= entry.is_dir;
                saw_file |= !entry.is_dir;
            }
            if index == 0 {
                first_present = entry.is_some();
            }
            if index + 1 == self.snapshots.len() {
                last_present = entry.is_some();
            }

            let size = entry.map_or(0, |entry| entry.sizes.value(metric));
            let parent_size = if let Some(parent) = Analysis::parent_path(path) {
                snapshot
                    .entries
                    .get(&parent)
                    .map_or(0, |entry| entry.sizes.value(metric))
            } else {
                0
            };
            let root_size = snapshot.root_sizes()?.value(metric);

            timeline.push(TimelinePoint {
                label: snapshot.label.clone(),
                size,
                local_share: share(size, parent_size),
                root_share: share(size, root_size),
            });
        }

        let Some(name) = name else {
            return Ok(None);
        };

        let kind = match (saw_dir, saw_file) {
            (true, true) => EntryKind::Mixed,
            (true, false) => EntryKind::Dir,
            (false, true) => EntryKind::File,
            (false, false) => return Ok(None),
        };

        let baseline_size = timeline.first().map_or(0, |point| point.size);
        let latest_size = timeline.last().map_or(0, |point| point.size);
        let baseline_local_share = timeline.first().map_or(0.0, |point| point.local_share);
        let latest_local_share = timeline.last().map_or(0.0, |point| point.local_share);
        let delta = latest_size as i64 - baseline_size as i64;
        let change_kind = change_kind_from_presence_and_delta(
            first_present,
            last_present,
            delta,
            baseline_local_share,
            latest_local_share,
        );

        Ok(Some(RowData {
            name,
            path: path.to_string(),
            kind,
            change_kind,
            latest_size,
            baseline_size,
            delta,
            latest_local_share,
            baseline_local_share,
            local_share_delta: latest_local_share - baseline_local_share,
            timeline,
        }))
    }

    fn root_row(&self, metric: SizeMetric) -> Result<RowData> {
        let mut timeline = Vec::with_capacity(self.snapshots.len());
        for snapshot in &self.snapshots {
            let size = snapshot.root_sizes()?.value(metric);
            timeline.push(TimelinePoint {
                label: snapshot.label.clone(),
                size,
                local_share: 1.0,
                root_share: 1.0,
            });
        }

        let baseline_size = timeline.first().map_or(0, |point| point.size);
        let latest_size = timeline.last().map_or(0, |point| point.size);
        let delta = latest_size as i64 - baseline_size as i64;

        Ok(RowData {
            name: self.current_root_name().to_string(),
            path: String::new(),
            kind: EntryKind::Dir,
            change_kind: if delta == 0 {
                ChangeKind::Unchanged
            } else {
                ChangeKind::Changed
            },
            latest_size,
            baseline_size,
            delta,
            latest_local_share: 1.0,
            baseline_local_share: 1.0,
            local_share_delta: 0.0,
            timeline,
        })
    }
}

fn change_kind_from_presence_and_delta(
    first_present: bool,
    last_present: bool,
    delta: i64,
    baseline_local_share: f64,
    latest_local_share: f64,
) -> ChangeKind {
    if !first_present && last_present {
        return ChangeKind::Added;
    }
    if first_present && !last_present {
        return ChangeKind::Removed;
    }
    if delta != 0 || (baseline_local_share - latest_local_share).abs() > f64::EPSILON {
        return ChangeKind::Changed;
    }
    ChangeKind::Unchanged
}

impl SnapshotIndex {
    fn from_snapshot(snapshot: SnapshotTree) -> Result<Self> {
        let mut entries = BTreeMap::new();
        flatten_node(&snapshot.root, String::new(), None, &mut entries);
        if entries.is_empty() {
            bail!("snapshot {} does not contain any entries", snapshot.label);
        }
        Ok(Self {
            label: snapshot.label,
            root_name: snapshot.root.name().to_string(),
            entries,
        })
    }
}

fn flatten_node(
    node: &GduNode,
    path: String,
    parent: Option<String>,
    out: &mut BTreeMap<String, IndexedEntry>,
) -> SizePair {
    match node {
        GduNode::File(file) => {
            let sizes = SizePair {
                disk: file.disk_size,
                apparent: file.apparent_size,
            };
            out.insert(
                path.clone(),
                IndexedEntry {
                    name: file.name.clone(),
                    parent,
                    is_dir: false,
                    sizes,
                },
            );
            sizes
        }
        GduNode::Dir(dir) => {
            let mut total = SizePair::default();
            for child in &dir.children {
                let child_path = join_path(&path, child.name());
                let child_sizes = flatten_node(child, child_path, Some(path.clone()), out);
                total.disk = total.disk.saturating_add(child_sizes.disk);
                total.apparent = total.apparent.saturating_add(child_sizes.apparent);
            }

            out.insert(
                path.clone(),
                IndexedEntry {
                    name: dir.name.clone(),
                    parent,
                    is_dir: true,
                    sizes: total,
                },
            );
            total
        }
    }
}

fn sort_rows(mut rows: Vec<RowData>, sort: SortMode) -> Vec<RowData> {
    rows.sort_by(|left, right| {
        let by_name = left.name.to_lowercase().cmp(&right.name.to_lowercase());
        let dirs_first = right.has_children().cmp(&left.has_children());
        match sort {
            SortMode::LatestSize => right
                .latest_size
                .cmp(&left.latest_size)
                .then(dirs_first)
                .then(by_name),
            SortMode::Delta => right.delta.cmp(&left.delta).then(dirs_first).then(by_name),
            SortMode::ShareDelta => right
                .local_share_delta
                .total_cmp(&left.local_share_delta)
                .then(dirs_first)
                .then(by_name),
            SortMode::Name => dirs_first.then(by_name),
        }
    });
    rows
}

fn join_path(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_string()
    } else {
        format!("{parent}/{name}")
    }
}

fn share(part: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        part as f64 / total as f64
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::Result;

    use crate::gdu::SnapshotTree;

    use super::{Analysis, ChangeKind, SizeMetric, SortMode};

    #[test]
    fn aggregates_sizes_and_shares() -> Result<()> {
        let first = SnapshotTree::from_json_str(
            "first".into(),
            PathBuf::from("first.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"one.bin","asize":10,"dsize":20,"mtime":1}],{"name":"b.bin","asize":30,"dsize":40,"mtime":1}]]"#,
        )?;
        let second = SnapshotTree::from_json_str(
            "second".into(),
            PathBuf::from("second.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},[{"name":"a","mtime":1},{"name":"one.bin","asize":40,"dsize":50,"mtime":1}],{"name":"b.bin","asize":10,"dsize":10,"mtime":1},[{"name":"c","mtime":1},{"name":"z.bin","asize":5,"dsize":8,"mtime":1}]]]"#,
        )?;

        let analysis = Analysis::new(vec![second, first])?;
        let rows = analysis.children_of("", true, SizeMetric::Disk, SortMode::Name)?;

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].name, "a");
        assert_eq!(rows[0].baseline_size, 20);
        assert_eq!(rows[0].latest_size, 50);
        assert_eq!(rows[0].delta, 30);
        assert_eq!(rows[0].change_kind, ChangeKind::Changed);
        assert!((rows[0].baseline_local_share - (20.0 / 60.0)).abs() < 0.0001);
        assert!((rows[0].latest_local_share - (50.0 / 68.0)).abs() < 0.0001);

        assert_eq!(rows[2].name, "b.bin");
        assert_eq!(rows[2].delta, -30);
        assert_eq!(rows[2].change_kind, ChangeKind::Changed);
        Ok(())
    }

    #[test]
    fn filters_files_when_requested() -> Result<()> {
        let first = SnapshotTree::from_json_str(
            "first".into(),
            PathBuf::from("first.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},[{"name":"dir","mtime":1},{"name":"one.bin","asize":10,"dsize":20,"mtime":1}],{"name":"plain.bin","asize":30,"dsize":40,"mtime":1}]]"#,
        )?;
        let second = SnapshotTree::from_json_str(
            "second".into(),
            PathBuf::from("second.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},[{"name":"dir","mtime":1},{"name":"one.bin","asize":40,"dsize":50,"mtime":1}],{"name":"plain.bin","asize":10,"dsize":10,"mtime":1}]]"#,
        )?;

        let analysis = Analysis::new(vec![first, second])?;
        let rows = analysis.children_of("", false, SizeMetric::Disk, SortMode::Name)?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].name, "dir");
        Ok(())
    }

    #[test]
    fn builds_root_row_for_current_directory_summary() -> Result<()> {
        let first = SnapshotTree::from_json_str(
            "first".into(),
            PathBuf::from("first.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":10},[{"name":"/root","mtime":1},{"name":"one.bin","asize":10,"dsize":20,"mtime":1}]]"#,
        )?;
        let second = SnapshotTree::from_json_str(
            "second".into(),
            PathBuf::from("second.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":20},[{"name":"/root","mtime":1},{"name":"one.bin","asize":40,"dsize":50,"mtime":1}]]"#,
        )?;

        let analysis = Analysis::new(vec![first, second])?;
        let root = analysis.row_for_path("", SizeMetric::Disk)?;

        assert_eq!(root.kind, super::EntryKind::Dir);
        assert_eq!(root.baseline_size, 20);
        assert_eq!(root.latest_size, 50);
        assert_eq!(root.delta, 30);
        assert_eq!(root.latest_root_share(), 1.0);
        Ok(())
    }
}
