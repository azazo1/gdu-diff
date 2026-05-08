mod analysis;
mod gdu;
mod store;
mod tui;

use std::path::PathBuf;
use std::time::Duration;
use std::{env, path::Path};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tempfile::tempdir;

use analysis::{Analysis, SizeMetric};
use gdu::{SnapshotTree, export_snapshot_with_progress};
use store::{SnapshotStore, canonicalize_dir};
use tui::{App, LoadingState, TerminalSession};

const LOADING_DRAW_THROTTLE: Duration = Duration::from_millis(80);

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
    #[arg(value_name = "ARG", allow_hyphen_values = true)]
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
        other => run_with_loading(other, &cli),
    }
}

fn run_with_loading(action: Action, cli: &Cli) -> Result<()> {
    let mut session = TerminalSession::start()?;
    let total_steps = total_steps_for_action(&action);
    let mut loading = LoadingState::new("gdu-diff", total_steps);
    loading.set_step(1, action_title(&action), "Preparing inputs");
    session.draw_loading(&loading)?;

    let snapshots = match action {
        Action::CompareFiles { files } => load_compare_files(files, &mut session, &mut loading)?,
        Action::CompareCurrentWithFile { file, target } => {
            load_compare_current_with_file(file, target, &mut session, &mut loading)?
        }
        Action::DiffTarget { target } => load_diff_target(target, &mut session, &mut loading)?,
        Action::Shot { .. } => unreachable!(),
    };

    loading.set_step(total_steps.saturating_sub(1), String::from("Build analysis"), "Indexing snapshot trees");
    session.draw_loading(&loading)?;

    let analysis = Analysis::new_with_progress(snapshots, |progress| {
        loading.set_step_progress(progress.overall_progress());
        loading.set_detail(format!(
            "Indexing snapshot {}/{}: {} ({:.0}%)\nCurrent path: {}",
            progress.snapshot_index,
            progress.snapshot_total,
            progress.snapshot_label,
            progress.snapshot_progress * 100.0,
            progress.current_path
        ));
        let _ = session.draw_loading_throttled(&loading, LOADING_DRAW_THROTTLE);
    })?;
    let metric = if cli.show_apparent_size {
        SizeMetric::Apparent
    } else {
        SizeMetric::Disk
    };

    loading.set_step(total_steps, String::from("Prepare interface"), "Building initial table view");
    session.draw_loading(&loading)?;

    let mut app = App::new(analysis, metric, !cli.dirs_only)?;
    session.run_app(&mut app)
}

fn load_compare_files(
    files: Vec<PathBuf>,
    session: &mut TerminalSession,
    loading: &mut LoadingState,
) -> Result<Vec<SnapshotTree>> {
    loading.set_step(1, String::from("Load snapshots"), format!("Reading {} snapshot files", files.len()));
    session.draw_loading(loading)?;

    let total = files.len();
    let mut snapshots = Vec::with_capacity(total);
    for (index, path) in files.into_iter().enumerate() {
        loading.set_step_progress((index as f64) / total.max(1) as f64);
        loading.set_detail(format!("Reading snapshot {}/{}: {}", index + 1, total, path.display()));
        session.draw_loading(loading)?;
        snapshots.push(SnapshotTree::load(path)?);
    }
    loading.set_step_progress(1.0);
    Ok(snapshots)
}

fn load_compare_current_with_file(
    file: PathBuf,
    target: PathBuf,
    session: &mut TerminalSession,
    loading: &mut LoadingState,
) -> Result<Vec<SnapshotTree>> {
    loading.set_step(1, String::from("Load baseline snapshot"), format!("Reading {}", file.display()));
    session.draw_loading(loading)?;
    let snapshot = SnapshotTree::load(file)?;
    loading.set_step_progress(1.0);

    loading.set_step(2, String::from("Resolve target directory"), format!("Resolving {}", target.display()));
    session.draw_loading(loading)?;
    let canonical_target = canonicalize_dir(&target)?;
    loading.set_step_progress(1.0);

    loading.set_step(3, String::from("Scan current directory"), format!("Launching gdu-go for {}", canonical_target.display()));
    session.draw_loading(loading)?;
    let temp_dir = tempdir().context("failed to create temporary directory")?;
    let current_path = temp_dir.path().join("current.json");
    export_snapshot_with_progress(&canonical_target, &current_path, |progress| {
        loading.set_detail(progress.to_string());
        let _ = session.draw_loading_throttled(loading, LOADING_DRAW_THROTTLE);
    })?;
    loading.set_step_progress(1.0);

    loading.set_step(4, String::from("Load current snapshot"), String::from("Parsing generated JSON"));
    session.draw_loading(loading)?;
    let current = SnapshotTree::load_with_label(current_path, String::from("current"))?;
    loading.set_step_progress(1.0);

    Ok(vec![snapshot, current])
}

fn load_diff_target(
    target: PathBuf,
    session: &mut TerminalSession,
    loading: &mut LoadingState,
) -> Result<Vec<SnapshotTree>> {
    loading.set_step(1, String::from("Resolve target directory"), format!("Resolving {}", target.display()));
    session.draw_loading(loading)?;
    let canonical_target = canonicalize_dir(&target)?;
    loading.set_step_progress(1.0);

    loading.set_step(2, String::from("Find latest stored snapshot"), canonical_target.display().to_string());
    session.draw_loading(loading)?;
    let store = SnapshotStore::new()?;
    let latest = store.find_latest_for(&canonical_target)?.with_context(|| {
        format!(
            "no stored snapshot found for {} in {}. run `gdu-diff shot {}` first",
            canonical_target.display(),
            store.data_dir().display(),
            canonical_target.display()
        )
    })?;
    loading.set_step_progress(1.0);

    loading.set_step(3, String::from("Scan current directory"), format!("Launching gdu-go for {}", canonical_target.display()));
    session.draw_loading(loading)?;
    let temp_dir = tempdir().context("failed to create temporary directory")?;
    let current_path = temp_dir.path().join("current.json");
    export_snapshot_with_progress(&canonical_target, &current_path, |progress| {
        loading.set_detail(progress.to_string());
        let _ = session.draw_loading_throttled(loading, LOADING_DRAW_THROTTLE);
    })?;
    loading.set_step_progress(1.0);

    loading.set_step(4, String::from("Load current snapshot"), String::from("Parsing generated JSON"));
    session.draw_loading(loading)?;
    let current = SnapshotTree::load_with_label(current_path, String::from("current"))?;
    loading.set_step_progress(1.0);

    Ok(vec![latest.snapshot, current])
}

fn total_steps_for_action(action: &Action) -> usize {
    match action {
        Action::CompareFiles { .. } => 3,
        Action::CompareCurrentWithFile { .. } => 6,
        Action::DiffTarget { .. } => 6,
        Action::Shot { .. } => 1,
    }
}

fn action_title(action: &Action) -> String {
    match action {
        Action::CompareFiles { .. } => String::from("Compare snapshots"),
        Action::CompareCurrentWithFile { .. } => String::from("Compare snapshot with current scan"),
        Action::DiffTarget { .. } => String::from("Compare latest shot with current scan"),
        Action::Shot { .. } => String::from("Save snapshot"),
    }
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

    let current_dir = env::current_dir().context("failed to get current directory")?;
    classify_args(&cli.args, current_dir, resolve_snapshot_reference)
}

fn is_json_like_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
}

fn is_snapshot_reference(path: &Path) -> bool {
    is_json_like_path(path) || parse_shot_alias(path).is_some()
}

fn classify_args<F>(
    args: &[PathBuf],
    current_dir: PathBuf,
    mut resolve_reference: F,
) -> Result<Action>
where
    F: FnMut(&Path, &Path) -> Result<PathBuf>,
{
    if args.len() >= 2 && args.iter().all(|arg| is_snapshot_reference(arg)) {
        let files = args
            .iter()
            .map(|arg| resolve_reference(arg, &current_dir))
            .collect::<Result<Vec<_>>>()?;
        return Ok(Action::CompareFiles { files });
    }

    if args.len() == 1 {
        let arg = &args[0];
        if is_snapshot_reference(arg) {
            return Ok(Action::CompareCurrentWithFile {
                file: resolve_reference(arg, &current_dir)?,
                target: current_dir,
            });
        }
        return Ok(Action::DiffTarget {
            target: arg.clone(),
        });
    }

    if args.len() == 2 {
        let target = args[0].clone();
        let file = &args[1];
        if !is_snapshot_reference(&target) && is_snapshot_reference(file) {
            return Ok(Action::CompareCurrentWithFile {
                file: resolve_reference(file, &target)?,
                target,
            });
        }
    }

    bail!(
        "use `gdu-diff [directory] snapshot.json` to compare a directory with one snapshot, or pass only JSON files when comparing snapshots directly"
    )
}

fn parse_shot_alias(path: &Path) -> Option<usize> {
    let value = path.to_str()?;
    let suffix = value.strip_prefix('-')?;
    if suffix.is_empty() {
        return None;
    }
    suffix.parse::<usize>().ok().filter(|index| *index > 0)
}

fn resolve_snapshot_reference(path: &Path, target: &Path) -> Result<PathBuf> {
    let Some(ordinal) = parse_shot_alias(path) else {
        return Ok(path.to_path_buf());
    };

    let canonical_target = canonicalize_dir(target)?;
    let store = SnapshotStore::new()?;
    let snapshot = store
        .find_nth_latest_for(&canonical_target, ordinal)?
        .with_context(|| missing_shot_message(&store, &canonical_target, ordinal))?;
    Ok(snapshot.source)
}

fn missing_shot_message(store: &SnapshotStore, target: &Path, ordinal: usize) -> String {
    format!(
        "no stored shot -{ordinal} found for {} in {}. run `gdu-diff shot {}` first",
        target.display(),
        store.data_dir().display(),
        target.display()
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use clap::Parser;

    use super::{Action, Cli, classify_action, classify_args, parse_shot_alias};

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

    #[test]
    fn parses_shot_alias() {
        assert_eq!(parse_shot_alias(Path::new("-1")), Some(1));
        assert_eq!(parse_shot_alias(Path::new("-3")), Some(3));
        assert_eq!(parse_shot_alias(Path::new("-0")), None);
        assert_eq!(parse_shot_alias(Path::new("base.json")), None);
    }

    #[test]
    fn detects_single_alias_as_compare_current() {
        let action = classify_args(
            &[PathBuf::from("-3")],
            PathBuf::from("/cwd"),
            |path, target| {
                assert_eq!(path, Path::new("-3"));
                assert_eq!(target, Path::new("/cwd"));
                Ok(PathBuf::from("/shots/3.json"))
            },
        )
        .expect("classify");

        match action {
            Action::CompareCurrentWithFile { file, target } => {
                assert_eq!(file, PathBuf::from("/shots/3.json"));
                assert_eq!(target, PathBuf::from("/cwd"));
            }
            _ => panic!("expected compare current with file"),
        }
    }

    #[test]
    fn detects_directory_and_alias_as_compare_current() {
        let action = classify_args(
            &[PathBuf::from("/tmp"), PathBuf::from("-2")],
            PathBuf::from("/cwd"),
            |path, target| {
                assert_eq!(path, Path::new("-2"));
                assert_eq!(target, Path::new("/tmp"));
                Ok(PathBuf::from("/shots/2.json"))
            },
        )
        .expect("classify");

        match action {
            Action::CompareCurrentWithFile { file, target } => {
                assert_eq!(file, PathBuf::from("/shots/2.json"));
                assert_eq!(target, PathBuf::from("/tmp"));
            }
            _ => panic!("expected compare current with file"),
        }
    }

    #[test]
    fn detects_alias_and_json_as_compare_files() {
        let action = classify_args(
            &[PathBuf::from("-1"), PathBuf::from("base.json")],
            PathBuf::from("/cwd"),
            |path, target| {
                assert_eq!(target, Path::new("/cwd"));
                if path == Path::new("-1") {
                    return Ok(PathBuf::from("/shots/1.json"));
                }
                Ok(path.to_path_buf())
            },
        )
        .expect("classify");

        match action {
            Action::CompareFiles { files } => {
                assert_eq!(
                    files,
                    vec![PathBuf::from("/shots/1.json"), PathBuf::from("base.json")]
                );
            }
            _ => panic!("expected compare files"),
        }
    }
}
