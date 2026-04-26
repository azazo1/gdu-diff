mod analysis;
mod gdu;
mod store;
mod tui;

use std::path::PathBuf;
use std::{env, path::Path};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tempfile::tempdir;

use analysis::{Analysis, SizeMetric};
use gdu::{SnapshotTree, export_snapshot};
use store::{SnapshotStore, canonicalize_dir};
use tui::{App, run};

#[derive(Parser, Debug)]
#[command(
    name = "gdu-diff",
    version,
    about = "Browse gdu snapshots, store shots, and compare current disk usage against history."
)]
struct Cli {
    #[arg(short = 'a', long = "show-apparent-size", global = true)]
    show_apparent_size: bool,
    #[arg(long = "dirs-only", global = true)]
    dirs_only: bool,
    #[command(subcommand)]
    command: Option<CommandKind>,
    #[arg(value_name = "ARG")]
    args: Vec<PathBuf>,
}

#[derive(Subcommand, Debug)]
enum CommandKind {
    Shot {
        #[arg(value_name = "PATH")]
        path: Option<PathBuf>,
    },
}

enum Action {
    Shot { target: PathBuf },
    CompareFiles { files: Vec<PathBuf> },
    CompareCurrentWithFile { file: PathBuf, target: PathBuf },
    DiffTarget { target: PathBuf },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let action = classify_action(&cli)?;

    match action {
        Action::Shot { target } => {
            let store = SnapshotStore::new()?;
            let stored = store.save_shot(&target)?;
            println!(
                "saved snapshot for {} to {}",
                canonicalize_dir(&target)?.display(),
                stored.source.display()
            );
            println!("data dir: {}", store.data_dir().display());
            Ok(())
        }
        Action::CompareFiles { files } => {
            let snapshots = files
                .into_iter()
                .map(SnapshotTree::load)
                .collect::<Result<Vec<_>>>()?;
            run_tui(snapshots, &cli)
        }
        Action::CompareCurrentWithFile { file, target } => {
            let snapshot = SnapshotTree::load(file)?;
            let canonical_target = canonicalize_dir(&target)?;

            let temp_dir = tempdir().context("failed to create temporary directory")?;
            let current_path = temp_dir.path().join("current.json");
            export_snapshot(&canonical_target, &current_path)?;
            let current = SnapshotTree::load_with_label(current_path, String::from("current"))?;

            run_tui(vec![snapshot, current], &cli)
        }
        Action::DiffTarget { target } => {
            let canonical_target = canonicalize_dir(&target)?;
            let store = SnapshotStore::new()?;
            let latest = store.find_latest_for(&canonical_target)?.with_context(|| {
                format!(
                    "no stored snapshot found for {} in {}. run `gdu-diff shot {}` first",
                    canonical_target.display(),
                    store.data_dir().display(),
                    canonical_target.display()
                )
            })?;

            let temp_dir = tempdir().context("failed to create temporary directory")?;
            let current_path = temp_dir.path().join("current.json");
            export_snapshot(&canonical_target, &current_path)?;
            let current = SnapshotTree::load_with_label(current_path, String::from("current"))?;

            run_tui(vec![latest.snapshot, current], &cli)
        }
    }
}

fn run_tui(snapshots: Vec<SnapshotTree>, cli: &Cli) -> Result<()> {
    let analysis = Analysis::new(snapshots)?;
    let metric = if cli.show_apparent_size {
        SizeMetric::Apparent
    } else {
        SizeMetric::Disk
    };
    let app = App::new(analysis, metric, !cli.dirs_only)?;
    run(app)
}

fn classify_action(cli: &Cli) -> Result<Action> {
    if let Some(command) = &cli.command {
        return match command {
            CommandKind::Shot { path } => Ok(Action::Shot {
                target: path
                    .clone()
                    .unwrap_or(env::current_dir().context("failed to get current directory")?),
            }),
        };
    }

    if cli.args.is_empty() {
        return Ok(Action::DiffTarget {
            target: env::current_dir().context("failed to get current directory")?,
        });
    }

    if cli.args.len() >= 2 && cli.args.iter().all(|x| is_json_like_path(x)) {
        return Ok(Action::CompareFiles {
            files: cli.args.clone(),
        });
    }

    if cli.args.len() == 1 {
        let target = cli.args[0].clone();
        if is_json_like_path(&target) {
            return Ok(Action::CompareCurrentWithFile {
                file: target,
                target: env::current_dir().context("failed to get current directory")?,
            });
        }
        return Ok(Action::DiffTarget { target });
    }

    if cli.args.len() == 2 {
        let target = cli.args[0].clone();
        let file = cli.args[1].clone();
        if !is_json_like_path(&target) && is_json_like_path(&file) {
            return Ok(Action::CompareCurrentWithFile { file, target });
        }
    }

    bail!(
        "use `gdu-diff [directory] snapshot.json` to compare a directory with one snapshot, or pass only JSON files when comparing snapshots directly"
    )
}

fn is_json_like_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use clap::Parser;

    use super::{Action, Cli, classify_action};

    #[test]
    fn defaults_to_diff_current_dir() {
        let cli = Cli::parse_from(["gdu-diff"]);
        assert!(matches!(
            classify_action(&cli).expect("classify"),
            Action::DiffTarget { .. }
        ));
    }

    #[test]
    fn detects_compare_files() {
        let cli = Cli::parse_from(["gdu-diff", "a.json", "b.json"]);
        match classify_action(&cli).expect("classify") {
            Action::CompareFiles { files } => {
                assert_eq!(files.len(), 2);
            }
            _ => panic!("expected compare files"),
        }
    }

    #[test]
    fn detects_shot_subcommand() {
        let cli = Cli::parse_from(["gdu-diff", "shot", "/tmp"]);
        assert!(matches!(
            classify_action(&cli).expect("classify"),
            Action::Shot { .. }
        ));
    }

    #[test]
    fn detects_single_json_as_compare_current() {
        let cli = Cli::parse_from(["gdu-diff", "base.json"]);
        assert!(matches!(
            classify_action(&cli).expect("classify"),
            Action::CompareCurrentWithFile { .. }
        ));
    }

    #[test]
    fn detects_directory_and_json_as_compare_current() {
        let cli = Cli::parse_from(["gdu-diff", "/tmp", "base.json"]);
        match classify_action(&cli).expect("classify") {
            Action::CompareCurrentWithFile { file, target } => {
                assert_eq!(file, PathBuf::from("base.json"));
                assert_eq!(target, PathBuf::from("/tmp"));
            }
            _ => panic!("expected compare current with file"),
        }
    }
}
