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
use tokio::sync::mpsc;
use tokio::time::{Instant, interval};

use analysis::{Analysis, SizeMetric};
use gdu::{SnapshotLoadProgress, SnapshotTree, export_snapshot_with_progress};
use store::{SnapshotStore, canonicalize_dir};
use tui::{App, LoadingState, LoadingStep, TerminalSession};

const LOADING_DRAW_THROTTLE: Duration = Duration::from_millis(80);
const SCAN_PROGRESS_TICK: Duration = Duration::from_millis(16);
const SCAN_PROGRESS_CAP: f64 = 0.95;
const SCAN_PROGRESS_SETTLE_TIME: Duration = Duration::from_secs(8);
const SNAPSHOT_LOAD_PROGRESS_CAP: f64 = 0.98;

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
    CompareFiles { references: Vec<PathBuf> },
    CompareCurrentWithFile { reference: PathBuf, target: PathBuf },
    DiffTarget { target: PathBuf },
}

enum BackgroundScanUpdate {
    Detail(String),
    Finished(Result<()>),
}

enum BackgroundSnapshotLoadUpdate {
    Progress(SnapshotLoadProgress),
    Finished(Result<SnapshotTree>),
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let action = classify_action(&cli)?;

    match action {
        Action::Shot { target } => {
            let store = SnapshotStore::new()?;
            let stored = store.save_shot(&target).await?;
            println!(
                "saved snapshot for {} to {}",
                canonicalize_dir(&target).await?.display(),
                stored.source.display()
            );
            println!("data dir: {}", store.data_dir().display());
            Ok(())
        }
        other => run_with_loading(other, &cli).await,
    }
}

async fn run_with_loading(action: Action, cli: &Cli) -> Result<()> {
    let mut session = TerminalSession::start()?;
    let loading_steps = loading_steps_for_action(&action);
    let mut loading = LoadingState::new("gdu-diff", loading_steps);
    loading.set_step(1, action_title(&action), "Preparing inputs");
    session.draw_loading(&loading)?;

    let snapshots = match action {
        Action::CompareFiles { references } => {
            load_compare_files(references, &mut session, &mut loading).await?
        }
        Action::CompareCurrentWithFile { reference, target } => {
            load_compare_current_with_file(reference, target, &mut session, &mut loading).await?
        }
        Action::DiffTarget { target } => {
            load_diff_target(target, &mut session, &mut loading).await?
        }
        Action::Shot { .. } => unreachable!(),
    };

    loading.set_step(
        loading.total_steps().saturating_sub(1),
        String::from("Build analysis"),
        "Indexing snapshot trees",
    );
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

    loading.set_step(
        loading.total_steps(),
        String::from("Prepare interface"),
        "Building initial table view",
    );
    session.draw_loading(&loading)?;

    let mut app = App::new(analysis, metric, !cli.dirs_only)?;
    session.run_app(&mut app).await
}

async fn load_compare_files(
    references: Vec<PathBuf>,
    session: &mut TerminalSession,
    loading: &mut LoadingState,
) -> Result<Vec<SnapshotTree>> {
    loading.set_step(
        1,
        String::from("Load snapshots"),
        format!("Reading {} snapshot files", references.len()),
    );
    session.draw_loading(loading)?;

    let current_dir = env::current_dir().context("failed to get current directory")?;
    let total = references.len();
    let mut snapshots = Vec::with_capacity(total);
    for (index, reference) in references.into_iter().enumerate() {
        let path = resolve_snapshot_reference(&reference, &current_dir).await?;
        let progress_base = index as f64 / total.max(1) as f64;
        let progress_span = 1.0 / total.max(1) as f64;
        let detail = format!(
            "Reading snapshot {}/{}: {}",
            index + 1,
            total,
            path.display()
        );
        snapshots.push(
            load_snapshot_with_progress(
                path,
                None,
                session,
                loading,
                detail,
                progress_base,
                progress_span,
            )
            .await?,
        );
    }
    loading.set_step_progress(1.0);
    Ok(snapshots)
}

async fn load_compare_current_with_file(
    reference: PathBuf,
    target: PathBuf,
    session: &mut TerminalSession,
    loading: &mut LoadingState,
) -> Result<Vec<SnapshotTree>> {
    let file = resolve_snapshot_reference(&reference, &target).await?;
    loading.set_step(
        1,
        String::from("Load baseline snapshot"),
        format!("Reading {}", file.display()),
    );
    let baseline_detail = format!("Reading {}", file.display());
    let snapshot =
        load_snapshot_with_progress(file, None, session, loading, baseline_detail, 0.0, 1.0)
            .await?;

    loading.set_step(
        2,
        String::from("Resolve target directory"),
        format!("Resolving {}", target.display()),
    );
    session.draw_loading(loading)?;
    let canonical_target = canonicalize_dir(&target).await?;
    loading.set_step_progress(1.0);

    loading.set_step(
        3,
        String::from("Scan current directory"),
        format!("Launching gdu-go for {}", canonical_target.display()),
    );
    session.draw_loading(loading)?;
    let temp_dir = tempdir().context("failed to create temporary directory")?;
    let current_path = temp_dir.path().join("current.json");
    export_snapshot_with_fake_progress(
        &canonical_target,
        &current_path,
        session,
        loading,
        format!("Launching gdu-go for {}", canonical_target.display()),
    )
    .await?;

    loading.set_step(
        4,
        String::from("Load current snapshot"),
        String::from("Parsing generated JSON"),
    );
    let current_detail = String::from("Parsing generated JSON");
    let current = load_snapshot_with_progress(
        current_path,
        Some(String::from("current")),
        session,
        loading,
        current_detail,
        0.0,
        1.0,
    )
    .await?;

    Ok(vec![snapshot, current])
}

async fn load_diff_target(
    target: PathBuf,
    session: &mut TerminalSession,
    loading: &mut LoadingState,
) -> Result<Vec<SnapshotTree>> {
    loading.set_step(
        1,
        String::from("Resolve target directory"),
        format!("Resolving {}", target.display()),
    );
    session.draw_loading(loading)?;
    let canonical_target = canonicalize_dir(&target).await?;
    loading.set_step_progress(1.0);

    loading.set_step(
        2,
        String::from("Load latest stored snapshot"),
        canonical_target.display().to_string(),
    );
    let store = SnapshotStore::new()?;
    let latest_path = store
        .find_nth_latest_path_for(&canonical_target, 1)
        .await?
        .with_context(|| {
            format!(
                "no stored snapshot found for {} in {}. run `gdu-diff shot {}` first",
                canonical_target.display(),
                store.data_dir().display(),
                canonical_target.display()
            )
        })?;
    let latest_detail = format!("Reading {}", latest_path.display());
    let latest_snapshot = load_snapshot_with_progress(
        latest_path,
        Some(String::from("latest")),
        session,
        loading,
        latest_detail,
        0.0,
        1.0,
    )
    .await?;
    loading.set_detail(format!(
        "Loaded latest stored snapshot for {}",
        canonical_target.display()
    ));
    loading.set_step_progress(1.0);

    loading.set_step(
        3,
        String::from("Scan current directory"),
        format!("Launching gdu-go for {}", canonical_target.display()),
    );
    session.draw_loading(loading)?;
    let temp_dir = tempdir().context("failed to create temporary directory")?;
    let current_path = temp_dir.path().join("current.json");
    export_snapshot_with_fake_progress(
        &canonical_target,
        &current_path,
        session,
        loading,
        format!("Launching gdu-go for {}", canonical_target.display()),
    )
    .await?;

    loading.set_step(
        4,
        String::from("Load current snapshot"),
        String::from("Parsing generated JSON"),
    );
    let current_detail = String::from("Parsing generated JSON");
    let current = load_snapshot_with_progress(
        current_path,
        Some(String::from("current")),
        session,
        loading,
        current_detail,
        0.0,
        1.0,
    )
    .await?;

    Ok(vec![latest_snapshot, current])
}

async fn load_snapshot_with_progress(
    path: PathBuf,
    label: Option<String>,
    session: &mut TerminalSession,
    loading: &mut LoadingState,
    detail: String,
    progress_base: f64,
    progress_span: f64,
) -> Result<SnapshotTree> {
    let path_for_task = path.clone();
    let (sender, mut receiver) = mpsc::unbounded_channel();

    loading.set_step_progress(progress_base);
    loading.set_detail(detail.clone());
    session.draw_loading(loading)?;

    let task = tokio::spawn(async move {
        let progress_sender = sender.clone();
        let result = match label {
            Some(label) => {
                SnapshotTree::load_with_progress_and_label(path_for_task, label, move |progress| {
                    let _ = progress_sender.send(BackgroundSnapshotLoadUpdate::Progress(progress));
                })
                .await
            }
            None => SnapshotTree::load_with_progress(path_for_task, move |progress| {
                let _ = progress_sender.send(BackgroundSnapshotLoadUpdate::Progress(progress));
            })
            .await,
        };
        let _ = sender.send(BackgroundSnapshotLoadUpdate::Finished(result));
    });

    loop {
        match receiver.recv().await {
            Some(BackgroundSnapshotLoadUpdate::Progress(progress)) => {
                loading.set_step_progress(
                    (progress_base + progress_span * snapshot_display_progress(&progress))
                        .clamp(0.0, 1.0),
                );
                loading.set_detail(format_snapshot_load_detail(&detail, &path, &progress));
                let _ = session.draw_loading_throttled(loading, LOADING_DRAW_THROTTLE);
            }
            Some(BackgroundSnapshotLoadUpdate::Finished(result)) => {
                loading.set_step_progress((progress_base + progress_span).clamp(0.0, 1.0));
                session.draw_loading(loading)?;
                task.await.context("background snapshot loader task panicked")?;
                return result;
            }
            None => {
                task.await.context("background snapshot loader task panicked")?;
                bail!(
                    "background snapshot loader exited unexpectedly while reading {}",
                    path.display()
                );
            }
        }
    }
}

async fn export_snapshot_with_fake_progress(
    target: &Path,
    output: &Path,
    session: &mut TerminalSession,
    loading: &mut LoadingState,
    detail: String,
) -> Result<()> {
    let target = target.to_path_buf();
    let output = output.to_path_buf();
    let display_target = target.display().to_string();
    let (sender, mut receiver) = mpsc::unbounded_channel();

    loading.set_step_progress(0.0);
    loading.set_detail(detail);
    session.draw_loading(loading)?;

    let task = tokio::spawn(async move {
        let progress_sender = sender.clone();
        let result = export_snapshot_with_progress(&target, &output, |progress| {
            let _ = progress_sender.send(BackgroundScanUpdate::Detail(progress.to_string()));
        })
        .await;
        let _ = sender.send(BackgroundScanUpdate::Finished(result));
    });

    let started_at = Instant::now();
    let mut ticker = interval(SCAN_PROGRESS_TICK);
    loop {
        tokio::select! {
            maybe_update = receiver.recv() => {
                match maybe_update {
                    Some(BackgroundScanUpdate::Detail(detail)) => loading.set_detail(detail),
                    Some(BackgroundScanUpdate::Finished(result)) => {
                        loading.set_step_progress(1.0);
                        session.draw_loading(loading)?;
                        task.await.context("background gdu export task panicked")?;
                        return result;
                    }
                    None => {
                        task.await.context("background gdu export task panicked")?;
                        bail!("background gdu export exited unexpectedly while scanning {display_target}");
                    }
                }
            }
            _ = ticker.tick() => {
                loading.set_step_progress(fake_scan_progress(started_at.elapsed()));
                let _ = session.draw_loading_throttled(loading, LOADING_DRAW_THROTTLE);
            }
        }
    }
}

fn fake_scan_progress(elapsed: Duration) -> f64 {
    eased_fake_progress(elapsed, SCAN_PROGRESS_SETTLE_TIME, SCAN_PROGRESS_CAP)
}

fn eased_fake_progress(elapsed: Duration, settle_time: Duration, cap: f64) -> f64 {
    let cap = cap.clamp(0.0, 1.0);
    if cap <= 0.0 {
        return 0.0;
    }

    let settle_secs = settle_time.as_secs_f64();
    if settle_secs <= f64::EPSILON {
        return cap;
    }

    let ratio = (elapsed.as_secs_f64() / settle_secs).clamp(0.0, 1.0);
    let eased_ratio = 1.0 - (1.0 - ratio).powi(2);
    (cap * eased_ratio).clamp(0.0, cap)
}

fn loading_steps_for_action(action: &Action) -> Vec<LoadingStep> {
    match action {
        Action::CompareFiles { .. } => vec![
            LoadingStep::new("Load snapshots", 6.0),
            LoadingStep::new("Build analysis", 5.0),
            LoadingStep::new("Prepare interface", 1.0),
        ],
        Action::CompareCurrentWithFile { .. } => vec![
            LoadingStep::new("Load baseline snapshot", 4.0),
            LoadingStep::new("Resolve target directory", 0.5),
            LoadingStep::new("Scan current directory", 8.0),
            LoadingStep::new("Load current snapshot", 4.0),
            LoadingStep::new("Build analysis", 5.0),
            LoadingStep::new("Prepare interface", 1.0),
        ],
        Action::DiffTarget { .. } => vec![
            LoadingStep::new("Resolve target directory", 0.5),
            LoadingStep::new("Load latest stored snapshot", 4.0),
            LoadingStep::new("Scan current directory", 8.0),
            LoadingStep::new("Load current snapshot", 4.0),
            LoadingStep::new("Build analysis", 5.0),
            LoadingStep::new("Prepare interface", 1.0),
        ],
        Action::Shot { .. } => vec![LoadingStep::new("Save snapshot", 1.0)],
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
    classify_args(&cli.args, current_dir)
}

fn is_json_like_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".json") || name.ends_with(".json.zst"))
}

fn is_snapshot_reference(path: &Path) -> bool {
    is_json_like_path(path) || parse_shot_alias(path).is_some()
}

fn classify_args(args: &[PathBuf], current_dir: PathBuf) -> Result<Action> {
    if args.len() >= 2 && args.iter().all(|arg| is_snapshot_reference(arg)) {
        return Ok(Action::CompareFiles {
            references: args.to_vec(),
        });
    }

    if args.len() == 1 {
        let arg = &args[0];
        if is_snapshot_reference(arg) {
            return Ok(Action::CompareCurrentWithFile {
                reference: arg.clone(),
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
                reference: file.clone(),
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

async fn resolve_snapshot_reference(path: &Path, target: &Path) -> Result<PathBuf> {
    let Some(ordinal) = parse_shot_alias(path) else {
        return Ok(path.to_path_buf());
    };

    let canonical_target = canonicalize_dir(target).await?;
    let store = SnapshotStore::new()?;
    let snapshot_path = store
        .find_nth_latest_path_for(&canonical_target, ordinal)
        .await?
        .with_context(|| missing_shot_message(&store, &canonical_target, ordinal))?;
    Ok(snapshot_path)
}

fn missing_shot_message(store: &SnapshotStore, target: &Path, ordinal: usize) -> String {
    format!(
        "no stored shot -{ordinal} found for {} in {}. run `gdu-diff shot {}` first",
        target.display(),
        store.data_dir().display(),
        target.display()
    )
}

fn snapshot_display_progress(progress: &SnapshotLoadProgress) -> f64 {
    match progress.ratio() {
        Some(ratio) if ratio >= 1.0 => SNAPSHOT_LOAD_PROGRESS_CAP,
        Some(ratio) => ratio.clamp(0.0, SNAPSHOT_LOAD_PROGRESS_CAP),
        None => 0.0,
    }
}

fn format_snapshot_load_detail(
    prefix: &str,
    path: &Path,
    progress: &SnapshotLoadProgress,
) -> String {
    let format_total = |total_bytes: u64| {
        format!(
            "{} / {}",
            format_size(progress.read_bytes),
            format_size(total_bytes)
        )
    };
    let progress_line = match progress.total_bytes {
        Some(total_bytes) => match progress.ratio() {
            Some(ratio) if ratio >= 1.0 => {
                format!("Read {} from {}, parsing JSON tree", format_total(total_bytes), path.display())
            }
            Some(ratio) => format!(
                "Read {} from {} ({:.0}%)",
                format_total(total_bytes),
                path.display(),
                ratio * 100.0
            ),
            None => format!("Read {} from {}", format_total(total_bytes), path.display()),
        },
        None => format!(
            "Read {} from {}",
            format_size(progress.read_bytes),
            path.display()
        ),
    };
    let mode_line = if progress.compressed {
        "Mode: streaming zstd decompression"
    } else {
        "Mode: plain JSON read"
    };
    format!("{prefix}\n{progress_line}\n{mode_line}")
}

fn format_size(size: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = size as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{size} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use clap::Parser;

    use super::{
        Action, Cli, SCAN_PROGRESS_CAP, SCAN_PROGRESS_SETTLE_TIME, SnapshotLoadProgress,
        classify_action, classify_args, fake_scan_progress, format_snapshot_load_detail,
        is_json_like_path, parse_shot_alias, snapshot_display_progress,
    };

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
            Action::CompareFiles { references } => {
                assert_eq!(references.len(), 2);
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
            Action::CompareCurrentWithFile { reference, target } => {
                assert_eq!(reference, PathBuf::from("base.json"));
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
        let action =
            classify_args(&[PathBuf::from("-3")], PathBuf::from("/cwd")).expect("classify");

        match action {
            Action::CompareCurrentWithFile { reference, target } => {
                assert_eq!(reference, PathBuf::from("-3"));
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
        )
        .expect("classify");

        match action {
            Action::CompareCurrentWithFile { reference, target } => {
                assert_eq!(reference, PathBuf::from("-2"));
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
        )
        .expect("classify");

        match action {
            Action::CompareFiles { references } => {
                assert_eq!(
                    references,
                    vec![PathBuf::from("-1"), PathBuf::from("base.json")]
                );
            }
            _ => panic!("expected compare files"),
        }
    }

    #[test]
    fn scan_fake_progress_is_monotonic_and_caps_at_ninety_five() {
        let checkpoints = [
            Duration::from_secs(0),
            Duration::from_secs(1),
            Duration::from_secs(2),
            Duration::from_secs(4),
            Duration::from_secs(8),
            Duration::from_secs(16),
        ];
        let values = checkpoints.map(fake_scan_progress);

        assert_eq!(values[0], 0.0);
        assert!(values.windows(2).all(|window| window[0] <= window[1]));
        assert!((values[4] - SCAN_PROGRESS_CAP).abs() < 1e-9);
        assert!((values[5] - SCAN_PROGRESS_CAP).abs() < 1e-9);
    }

    #[test]
    fn scan_fake_progress_uses_a_nonlinear_curve() {
        let half_time = Duration::from_secs_f64(SCAN_PROGRESS_SETTLE_TIME.as_secs_f64() / 2.0);
        let half_progress = fake_scan_progress(half_time);

        assert!(half_progress > SCAN_PROGRESS_CAP * 0.5);
        assert!(half_progress < SCAN_PROGRESS_CAP);
    }

    #[test]
    fn accepts_compressed_snapshot_paths() {
        assert!(is_json_like_path(Path::new("base.json")));
        assert!(is_json_like_path(Path::new("base.json.zst")));
        assert!(!is_json_like_path(Path::new("base.zst")));
    }

    #[test]
    fn snapshot_progress_stops_short_of_complete_until_parse_finishes() {
        let progress = SnapshotLoadProgress {
            read_bytes: 10,
            total_bytes: Some(10),
            compressed: true,
        };
        assert!(snapshot_display_progress(&progress) < 1.0);
    }

    #[test]
    fn snapshot_load_detail_mentions_streaming_decompression() {
        let detail = format_snapshot_load_detail(
            "Reading snapshot",
            Path::new("/tmp/base.json.zst"),
            &SnapshotLoadProgress {
                read_bytes: 10,
                total_bytes: Some(20),
                compressed: true,
            },
        );
        assert!(detail.contains("streaming zstd decompression"));
        assert!(detail.contains("50%"));
    }
}
