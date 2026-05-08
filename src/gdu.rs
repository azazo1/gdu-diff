use std::fs::File;
use std::io::{BufReader as StdBufReader, BufWriter, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

pub const COMPRESSED_SNAPSHOT_EXTENSION: &str = "zst";
pub const SNAPSHOT_FILE_SUFFIX: &str = ".json.zst";
const GDU_EXPORT_EXTENSION: &str = "json";
const SHOT_ZSTD_COMPRESSION_LEVEL: i32 = 9;

#[derive(Clone, Debug)]
pub struct SnapshotTree {
    pub label: String,
    pub exported_at: Option<u64>,
    pub root: GduNode,
}

impl SnapshotTree {
    pub async fn load_with_progress<F>(path: PathBuf, on_progress: F) -> Result<Self>
    where
        F: FnMut() + Send + 'static,
    {
        let label = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(strip_snapshot_suffix)
            .map_or_else(|| path.display().to_string(), str::to_owned);
        Self::load_with_progress_and_label(path, label, on_progress).await
    }

    pub async fn load_with_label(path: PathBuf, label: String) -> Result<Self> {
        Self::load_with_progress_and_label(path, label, || {}).await
    }

    pub async fn load_with_progress_and_label<F>(
        path: PathBuf,
        label: String,
        _on_progress: F,
    ) -> Result<Self>
    where
        F: FnMut() + Send + 'static,
    {
        tokio::task::spawn_blocking(move || load_snapshot_tree(label, path))
            .await
            .context("background snapshot loader task panicked")?
    }

    pub fn from_json_str(label: String, source: PathBuf, content: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(content)
            .with_context(|| format!("failed to parse JSON from {}", source.display()))?;
        let top = value
            .as_array()
            .with_context(|| format!("{} is not a valid gdu export array", source.display()))?;
        if top.len() < 4 {
            bail!("{} is too short to be a valid gdu export", source.display());
        }

        let exported_at = top
            .get(2)
            .and_then(Value::as_object)
            .and_then(|meta| meta.get("timestamp"))
            .and_then(Value::as_u64);
        let root_value = top
            .get(3)
            .with_context(|| format!("{} is missing the root tree", source.display()))?;
        let root = parse_node(root_value)?;
        if !matches!(root, GduNode::Dir(_)) {
            bail!("{} root node is not a directory", source.display());
        }
        Ok(Self {
            label,
            exported_at,
            root,
        })
    }
}

fn load_snapshot_tree(label: String, path: PathBuf) -> Result<SnapshotTree> {
    let compressed = is_zstd_snapshot_path(&path);
    let file =
        File::open(&path).with_context(|| format!("failed to read gdu export file {}", path.display()))?;
    let content = if compressed {
        let decoder = zstd::stream::Decoder::new(file)
            .with_context(|| format!("failed to initialize zstd decoder for {}", path.display()))?;
        read_utf8(decoder, &path)?
    } else {
        read_utf8(file, &path)?
    };
    SnapshotTree::from_json_str(label, path, &content)
}

fn read_utf8(reader: impl Read, path: &Path) -> Result<String> {
    let mut content = String::new();
    let mut reader = StdBufReader::new(reader);
    reader
        .read_to_string(&mut content)
        .with_context(|| format!("failed to read gdu export file {}", path.display()))?;
    Ok(content)
}

#[derive(Clone, Debug)]
pub enum GduNode {
    File(GduFile),
    Dir(GduDir),
}

impl GduNode {
    pub fn name(&self) -> &str {
        match self {
            Self::File(file) => &file.name,
            Self::Dir(dir) => &dir.name,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GduFile {
    pub name: String,
    pub apparent_size: u64,
    pub disk_size: u64,
}

#[derive(Clone, Debug)]
pub struct GduDir {
    pub name: String,
    pub children: Vec<GduNode>,
}

fn parse_node(value: &Value) -> Result<GduNode> {
    match value {
        Value::Object(object) => Ok(GduNode::File(GduFile {
            name: read_name(object)?,
            apparent_size: object.get("asize").and_then(Value::as_u64).unwrap_or(0),
            disk_size: object.get("dsize").and_then(Value::as_u64).unwrap_or(0),
        })),
        Value::Array(items) => parse_dir(items),
        _ => bail!("encountered an unexpected gdu node value"),
    }
}

fn parse_dir(items: &[Value]) -> Result<GduNode> {
    let Some((head, tail)) = items.split_first() else {
        bail!("encountered an empty directory entry in gdu export");
    };
    let head = head
        .as_object()
        .context("directory head is not an object in gdu export")?;
    let children = tail.iter().map(parse_node).collect::<Result<Vec<_>>>()?;
    Ok(GduNode::Dir(GduDir {
        name: read_name(head)?,
        children,
    }))
}

fn read_name(object: &serde_json::Map<String, Value>) -> Result<String> {
    object
        .get("name")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .context("gdu node is missing its name field")
}

pub async fn export_snapshot(target: &Path, output: &Path) -> Result<()> {
    export_snapshot_with_progress(target, output, |_| {}).await
}

pub async fn export_snapshot_with_progress<F>(
    target: &Path,
    output: &Path,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(&str),
{
    let output_kind = snapshot_output_kind(output).with_context(|| {
        format!(
            "snapshot output must use either .json or {}: {}",
            SNAPSHOT_FILE_SUFFIX,
            output.display()
        )
    })?;
    let temp_output = match output_kind {
        SnapshotOutputKind::PlainJson => output.to_path_buf(),
        SnapshotOutputKind::CompressedZstd => raw_snapshot_temp_path(output),
    };

    if matches!(output_kind, SnapshotOutputKind::CompressedZstd)
        && output.extension().and_then(|extension| extension.to_str()) != Some(COMPRESSED_SNAPSHOT_EXTENSION)
    {
        bail!(
            "snapshot output must use the {} suffix: {}",
            SNAPSHOT_FILE_SUFFIX,
            output.display()
        );
    }
    let candidates = ["gdu-go", "gdu"];
    let mut not_found = Vec::new();

    for candidate in candidates {
        match Command::new(candidate)
            .arg("--output-file")
            .arg(&temp_output)
            .arg(target)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(mut child) => {
                let mut last_progress = None;

                if let Some(stderr) = child.stderr.take() {
                    let reader = BufReader::new(stderr);
                    let mut segments = reader.split(b'\r');
                    loop {
                        let next = segments.next_segment().await.with_context(|| {
                            format!("failed to read progress output from {}", candidate)
                        })?;
                        let Some(chunk) = next else {
                            break;
                        };
                        let text = String::from_utf8_lossy(&chunk).trim().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        last_progress = Some(text.clone());
                        on_progress(&text);
                    }
                }

                let result = child.wait_with_output().await.with_context(|| {
                    format!(
                        "failed to wait for {} while exporting {}",
                        candidate,
                        target.display()
                    )
                })?;
                if result.status.success() {
                    if matches!(output_kind, SnapshotOutputKind::CompressedZstd) {
                        compress_snapshot_file(&temp_output, output).await?;
                        let _ = fs::remove_file(&temp_output).await;
                    }
                    return Ok(());
                }
                let stderr = String::from_utf8_lossy(&result.stderr).trim().to_string();
                let stdout = String::from_utf8_lossy(&result.stdout).trim().to_string();
                let detail = if !stderr.is_empty() {
                    stderr
                } else if !stdout.is_empty() {
                    stdout
                } else if let Some(progress) = last_progress {
                    progress
                } else {
                    format!("exit status {}", result.status)
                };
                bail!(
                    "{} failed to export {}: {}",
                    candidate,
                    target.display(),
                    detail
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                not_found.push(candidate);
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to run {} to export {}", candidate, target.display())
                });
            }
        }
    }

    bail!(
        "failed to find a gdu executable, tried: {}",
        not_found.join(", ")
    )
}

pub fn is_zstd_snapshot_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case(COMPRESSED_SNAPSHOT_EXTENSION))
}

enum SnapshotOutputKind {
    PlainJson,
    CompressedZstd,
}

fn snapshot_output_kind(path: &Path) -> Result<SnapshotOutputKind> {
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or_default();
    if file_name.ends_with(SNAPSHOT_FILE_SUFFIX) {
        return Ok(SnapshotOutputKind::CompressedZstd);
    }
    if file_name.ends_with(".json") {
        return Ok(SnapshotOutputKind::PlainJson);
    }
    bail!("unsupported snapshot output path")
}

fn strip_snapshot_suffix(name: &str) -> &str {
    name.strip_suffix(SNAPSHOT_FILE_SUFFIX)
        .or_else(|| name.strip_suffix(".json"))
        .unwrap_or(name)
}

fn raw_snapshot_temp_path(output: &Path) -> PathBuf {
    output.with_extension(GDU_EXPORT_EXTENSION)
}

async fn compress_snapshot_file(input: &Path, output: &Path) -> Result<()> {
    let input = input.to_path_buf();
    let output = output.to_path_buf();
    tokio::task::spawn_blocking(move || compress_snapshot_file_blocking(&input, &output))
        .await
        .context("background snapshot compression task panicked")?
}

pub(crate) fn compress_snapshot_file_blocking(input: &Path, output: &Path) -> Result<()> {
    let source =
        File::open(input).with_context(|| format!("failed to read gdu export file {}", input.display()))?;
    let destination = File::create(output)
        .with_context(|| format!("failed to create snapshot file {}", output.display()))?;
    let mut reader = StdBufReader::new(source);
    let level = snapshot_compression_level();
    let mut encoder = zstd::stream::Encoder::new(BufWriter::new(destination), level)
        .with_context(|| format!("failed to initialize zstd encoder for {}", output.display()))?;
    std::io::copy(&mut reader, &mut encoder).with_context(|| {
        format!(
            "failed to compress gdu export {} into {}",
            input.display(),
            output.display()
        )
    })?;
    encoder
        .finish()
        .with_context(|| format!("failed to finalize snapshot file {}", output.display()))?;
    Ok(())
}

fn snapshot_compression_level() -> i32 {
    let range = zstd::compression_level_range();
    SHOT_ZSTD_COMPRESSION_LEVEL.clamp(*range.start(), *range.end())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use anyhow::Result;
    use tempfile::tempdir;

    use super::{
        COMPRESSED_SNAPSHOT_EXTENSION, GDU_EXPORT_EXTENSION, GduNode, SNAPSHOT_FILE_SUFFIX,
        SnapshotTree, compress_snapshot_file_blocking, is_zstd_snapshot_path,
    };

    #[test]
    fn parses_export_tree() -> Result<()> {
        let snapshot = SnapshotTree::from_json_str(
            "sample".into(),
            PathBuf::from("sample.json"),
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":42},[{"name":"/root","mtime":1},{"name":"a.bin","asize":10,"dsize":20,"mtime":1},[{"name":"dir","mtime":1},{"name":"b.bin","asize":30,"dsize":40,"mtime":1}]]]"#,
        )?;

        assert_eq!(snapshot.exported_at, Some(42));
        match snapshot.root {
            GduNode::Dir(root) => {
                assert_eq!(root.name, "/root");
                assert_eq!(root.children.len(), 2);
            }
            GduNode::File(_) => panic!("root must be a directory"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn loads_zstd_snapshot() -> Result<()> {
        let dir = tempdir()?;
        let raw_path = dir.path().join("sample.json");
        let compressed_path = dir.path().join("sample.json.zst");
        fs::write(
            &raw_path,
            r#"[1,2,{"progname":"gdu","progver":"v0","timestamp":42},[{"name":"/root","mtime":1},{"name":"a.bin","asize":10,"dsize":20,"mtime":1}]]"#,
        )?;
        compress_snapshot_file_blocking(&raw_path, &compressed_path)?;

        let snapshot = SnapshotTree::load_with_progress(compressed_path, || {}).await?;
        assert_eq!(snapshot.label, "sample");
        assert_eq!(snapshot.exported_at, Some(42));
        Ok(())
    }

    #[test]
    fn detects_zstd_snapshot_path() {
        assert!(is_zstd_snapshot_path(Path::new("a.json.zst")));
        assert!(!is_zstd_snapshot_path(Path::new("a.json")));
    }

    #[test]
    fn compressed_snapshot_suffix_is_stable() {
        assert_eq!(
            Path::new("/tmp/current.json")
                .with_extension(format!("{GDU_EXPORT_EXTENSION}.{COMPRESSED_SNAPSHOT_EXTENSION}")),
            PathBuf::from("/tmp/current.json.zst")
        );
        assert_eq!(SNAPSHOT_FILE_SUFFIX, ".json.zst");
    }
}
