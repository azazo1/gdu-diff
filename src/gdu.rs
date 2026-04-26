use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct SnapshotTree {
    pub label: String,
    pub exported_at: Option<u64>,
    pub root: GduNode,
}

impl SnapshotTree {
    pub fn load(path: PathBuf) -> Result<Self> {
        let label = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map_or_else(|| path.display().to_string(), str::to_owned);
        Self::load_with_label(path, label)
    }

    pub fn load_with_label(path: PathBuf, label: String) -> Result<Self> {
        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read gdu export file {}", path.display()))?;
        Self::from_json_str(label, path, &content)
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

pub fn export_snapshot(target: &Path, output: &Path) -> Result<()> {
    let candidates = ["gdu-go", "gdu"];
    let mut not_found = Vec::new();

    for candidate in candidates {
        match Command::new(candidate)
            .arg("--no-progress")
            .arg("--output-file")
            .arg(output)
            .arg(target)
            .output()
        {
            Ok(result) => {
                if result.status.success() {
                    return Ok(());
                }
                let stderr = String::from_utf8_lossy(&result.stderr).trim().to_string();
                let stdout = String::from_utf8_lossy(&result.stdout).trim().to_string();
                let detail = if !stderr.is_empty() {
                    stderr
                } else if !stdout.is_empty() {
                    stdout
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use anyhow::Result;

    use super::{GduNode, SnapshotTree};

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
}
