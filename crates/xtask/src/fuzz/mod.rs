//! `cargo xtask fuzz <subcommand>` - fuzz tooling.

mod build;
mod coverage;
mod process;
mod replay_regression;
mod run;
mod status;
mod triage;

#[cfg(all(test, unix))]
mod cli_tests;
#[cfg(all(test, unix))]
mod real_cli_tests;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

pub(super) use crate::repo_root;

#[derive(Parser)]
#[command(
    bin_name = "cargo xtask fuzz",
    about = "Build, run, and maintain fuzz targets"
)]
struct Cli {
    /// Watchdog for each target or scenario-helper build
    #[arg(long, global = true, overrides_with = "build_timeout", default_value = "1800", value_name = "SECONDS", value_parser = parse_seconds)]
    build_timeout: Duration,
    #[command(subcommand)]
    command: FuzzCommand,
}

#[derive(clap::Subcommand)]
enum FuzzCommand {
    /// Fuzz until crash, OOM, timeout, or cancellation
    Run(run::Options),
    /// Summarize corpora, seeds, artifacts, and coverage
    Status,
    /// Build all targets or the selected targets
    Build {
        #[arg(value_parser = parse_target)]
        targets: Vec<String>,
    },
    /// Initialize shared data for every target
    Prepare,
    /// Prepare, build, and replay regressions
    Verify(replay_regression::Options),
    /// Import legacy corpus, artifacts, and coverage history
    ImportData { source: Option<PathBuf> },
    /// Minimize a crash and verify its failure identity
    Triage(triage::Options),
    /// Replay tracked seeds with an independent watchdog per input
    ReplayRegression(replay_regression::Options),
    /// Snapshot source coverage for each selected target
    CoverageSnapshot(coverage::Options),
}

pub fn run(args: Vec<String>) {
    run_at(repo_root(), args);
}

fn run_at(root: PathBuf, args: Vec<String>) {
    let cli = Cli::try_parse_from(std::iter::once("fuzz".to_string()).chain(args))
        .unwrap_or_else(|error| error.exit());
    if let Err(error) = dispatch(root, cli) {
        eprintln!("xtask fuzz: {error:#}");
        let code = error
            .downcast_ref::<process::CommandFailure>()
            .map_or(2, |failure| failure.termination.code());
        std::process::exit(code);
    }
}

fn dispatch(root: PathBuf, cli: Cli) -> Result<()> {
    let project = build::Project::load(root, cli.build_timeout)?;
    let result = (|| match cli.command {
        FuzzCommand::Run(options) => run::run(&project, options),
        FuzzCommand::Build { targets } => project.build_targets(&targets, "none"),
        FuzzCommand::Verify(options) => {
            prepare(&project)?;
            project.build_targets(&options.targets, "none")?;
            replay_regression::run_prebuilt(&project, options)
        }
        FuzzCommand::Triage(options) => triage::run(&project, options),
        FuzzCommand::ReplayRegression(options) => replay_regression::run(&project, options),
        FuzzCommand::CoverageSnapshot(options) => coverage::run(&project, options),
        FuzzCommand::Status => status::run(&project),
        FuzzCommand::Prepare => prepare(&project),
        FuzzCommand::ImportData { source } => import_data(&project, source),
    })();
    project.runner.check_cancelled()?;
    result
}

fn parse_seconds(value: &str) -> Result<Duration, String> {
    value
        .parse::<std::num::NonZeroU32>()
        .map(|seconds| Duration::from_secs(u64::from(seconds.get())))
        .map_err(|_| "expected a positive 32-bit integer in seconds".to_string())
}

fn parse_target(name: &str) -> Result<String, String> {
    if target_named(name).is_none() {
        return Err(format!(
            "unknown target `{name}`. Known: {}",
            all_target_names().join(", ")
        ));
    }
    Ok(name.to_string())
}

fn prepare(project: &build::Project) -> Result<()> {
    let data = FuzzData::for_repo(project)?;
    for target in TARGETS {
        project.runner.check_cancelled()?;
        data.prepare_target(target.name)?;
    }
    println!(
        "prepared {} target corpora at {}",
        TARGETS.len(),
        data.root.display()
    );
    Ok(())
}

/// Sorted recursive inputs. Only a missing root means empty; unreadable entries,
/// symlinks and special files must not silently disappear from replay or counts.
pub(super) fn input_files(dir: &Path) -> Result<Vec<PathBuf>> {
    input_files_filtered(dir, |_| true)
}

fn input_files_filtered(
    dir: &Path,
    include: impl Fn(&std::fs::DirEntry) -> bool,
) -> Result<Vec<PathBuf>> {
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", dir.display())),
        Ok(metadata) if !metadata.is_dir() => bail!("expected directory: {}", dir.display()),
        Ok(_) => {}
    }
    fn collect(
        dir: &Path,
        files: &mut Vec<PathBuf>,
        include: &impl Fn(&std::fs::DirEntry) -> bool,
    ) -> Result<()> {
        for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry?;
            if !include(&entry) {
                continue;
            }
            let path = entry.path();
            let kind = entry.file_type()?;
            if kind.is_dir() {
                collect(&path, files, include)?;
            } else if kind.is_file() {
                files.push(path);
            } else {
                bail!("expected regular file or directory: {}", path.display());
            }
        }
        Ok(())
    }
    let mut files = Vec::new();
    collect(dir, &mut files, &include)?;
    files.sort();
    Ok(files)
}

struct SnapshotInput {
    live: PathBuf,
    copied: PathBuf,
}

/// Copy a corpus to flat, deterministic names, retaining each source/copy pair.
fn snapshot_corpus(
    corpus: &Path,
    snapshot: &Path,
    runner: &process::Runner,
) -> Result<Vec<SnapshotInput>> {
    input_files(corpus)?
        .into_iter()
        .enumerate()
        .map(|(index, live)| {
            runner.check_cancelled()?;
            let copied = snapshot.join(format!("{index:08}"));
            std::fs::copy(&live, &copied)
                .with_context(|| format!("snapshot {}", live.display()))?;
            Ok(SnapshotInput { live, copied })
        })
        .collect()
}

fn corpus_digest(
    corpus: &Path,
    inputs: &[SnapshotInput],
    runner: &process::Runner,
) -> Result<String> {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let mut buffer = [0; 64 * 1024];
    for input in inputs {
        runner.check_cancelled()?;
        let name = input
            .live
            .strip_prefix(corpus)?
            .as_os_str()
            .as_encoded_bytes();
        let mut file = std::fs::File::open(&input.copied)?;
        // Frame names and content lengths so boundaries cannot alias one another.
        hash_bytes(&mut hash, &(name.len() as u64).to_le_bytes());
        hash_bytes(&mut hash, name);
        hash_bytes(&mut hash, &file.metadata()?.len().to_le_bytes());
        loop {
            runner.check_cancelled()?;
            let count = file.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            hash_bytes(&mut hash, &buffer[..count]);
        }
    }
    Ok(format!("fnv1a64:{hash:016x}"))
}

fn hash_bytes(hash: &mut u64, bytes: &[u8]) {
    for byte in bytes {
        *hash ^= u64::from(*byte);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

fn git_text(project: &build::Project, args: &[&str]) -> Result<String> {
    let mut command = Command::new("git");
    command.args(args).current_dir(&project.root);
    let output = project
        .runner
        .run(
            command,
            Some(process::QUERY_TIMEOUT),
            process::OutputMode::Capture,
        )?
        .success("query repository")?;
    Ok(String::from_utf8(output.stdout.complete()?)?
        .trim_end_matches(['\r', '\n'])
        .to_string())
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) enum TargetKind {
    Json,
    Bytes,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(super) struct FuzzTarget {
    pub name: &'static str,
    pub kind: TargetKind,
}

/// Operational source of truth for fuzz targets. Cargo still needs matching
/// `[[bin]]` declarations; `Project::load` verifies the two sets before any fuzz
/// command runs so target-local build, replay, status, and coverage cannot drift.
pub(super) const TARGETS: &[FuzzTarget] = &[
    FuzzTarget {
        name: "smelt_loop",
        kind: TargetKind::Json,
    },
    FuzzTarget {
        name: "lua_loop",
        kind: TargetKind::Json,
    },
    FuzzTarget {
        name: "text_ops",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "attached_ops",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "cache_invariance",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "openai_cache_invariance",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "snapshot_roundtrip",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "grid_invariants",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "ansi_parser",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "edit_ops",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "provider_body",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "transcript_render",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "transcript_scroll_ops",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "provider_stream",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "permissions_rules",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "store_state",
        kind: TargetKind::Bytes,
    },
    FuzzTarget {
        name: "engine_events",
        kind: TargetKind::Bytes,
    },
];

pub(super) fn all_targets() -> &'static [FuzzTarget] {
    TARGETS
}

pub(super) fn all_target_names() -> Vec<String> {
    all_targets()
        .iter()
        .map(|target| target.name.to_string())
        .collect()
}

pub(super) fn target_named(name: &str) -> Option<&'static FuzzTarget> {
    all_targets().iter().find(|target| target.name == name)
}

fn metadata_target_is_fuzz_bin(target: &serde_json::Value) -> bool {
    let is_bin = target
        .get("kind")
        .and_then(|v| v.as_array())
        .is_some_and(|kinds| kinds.iter().any(|kind| kind.as_str() == Some("bin")));
    if !is_bin {
        return false;
    }
    target
        .get("src_path")
        .and_then(|v| v.as_str())
        .map(|path| path.replace('\\', "/").contains("/fuzz_targets/"))
        .unwrap_or(false)
}

#[derive(Clone, Debug)]
pub(super) struct FuzzData {
    pub root: PathBuf,
}

impl FuzzData {
    fn for_repo(project: &build::Project) -> Result<Self> {
        let repo = &project.root;
        let cwd = std::env::current_dir()?;
        if let Some(path) = std::env::var_os("SMELT_FUZZ_HOME") {
            if path.is_empty() {
                bail!("SMELT_FUZZ_HOME must not be empty");
            }
            return Ok(Self {
                root: cwd.join(path),
            });
        }

        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .unwrap_or_else(std::env::temp_dir);
        let identity = git_common_dir(project)?;
        let repository = if identity.file_name().and_then(|name| name.to_str()) == Some(".git") {
            identity.parent().unwrap_or(&identity)
        } else {
            repo
        };
        let name = repository
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("repository");
        Ok(Self {
            root: cwd
                .join(cache)
                .join("smelt")
                .join("fuzz")
                .join(format!("{name}-{:016x}", stable_path_hash(&identity))),
        })
    }

    pub fn corpus(&self, target: &str) -> PathBuf {
        self.root.join("corpus").join(target)
    }

    pub fn artifacts(&self, target: &str) -> PathBuf {
        self.root.join("artifacts").join(target)
    }

    pub fn coverage_history(&self) -> PathBuf {
        self.root.join("coverage-history")
    }

    pub fn prepare_target(&self, target: &str) -> Result<()> {
        for path in [self.corpus(target), self.artifacts(target)] {
            std::fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        }
        let corpus = self.corpus(target);
        if input_files(&corpus)?.is_empty() {
            let mut seed = tempfile::NamedTempFile::new_in(&corpus)?;
            seed.write_all(&[0; 256])?;
            match seed.persist_noclobber(corpus.join("bootstrap-zeroes")) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("publish bootstrap seed"),
            }
        }
        Ok(())
    }
}

fn git_common_dir(project: &build::Project) -> Result<PathBuf> {
    let path = git_text(project, &["rev-parse", "--git-common-dir"])?;
    project
        .root
        .join(path)
        .canonicalize()
        .context("resolve repository identity")
}

fn stable_path_hash(path: &Path) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    hash_bytes(&mut hash, path.as_os_str().as_encoded_bytes());
    hash
}

fn import_data(project: &build::Project, source: Option<PathBuf>) -> Result<()> {
    let source = match source {
        Some(path) => path,
        None => git_common_dir(project)?
            .parent()
            .context("repository has no parent")?
            .join("fuzz"),
    };
    let source = if source.join("fuzz/corpus").is_dir() {
        source.join("fuzz")
    } else {
        source
    };
    let source = source
        .canonicalize()
        .context("locate legacy fuzz directory")?;
    let data = FuzzData::for_repo(project)?;
    std::fs::create_dir_all(&data.root)?;
    let destination = data.root.canonicalize()?;
    if destination.starts_with(&source) || source.starts_with(&destination) {
        bail!("import source and destination must not overlap");
    }
    let mut copied = 0;
    let mut existing = 0;
    for name in ["corpus", "artifacts", "coverage-history"] {
        merge_tree(
            &source.join(name),
            &destination.join(name),
            &mut copied,
            &mut existing,
            &project.runner,
        )?;
    }
    println!("fuzz data: {}", data.root.display());
    println!("imported: {copied}, already present: {existing}");
    Ok(())
}

fn merge_tree(
    from: &Path,
    to: &Path,
    copied: &mut usize,
    existing: &mut usize,
    runner: &process::Runner,
) -> Result<()> {
    for source in input_files(from)? {
        runner.check_cancelled()?;
        let destination = to.join(source.strip_prefix(from)?);
        // Validate each directory before descending. create_dir_all alone follows
        // existing symlinks and could publish outside the selected data root.
        let directories: Vec<_> = destination
            .parent()
            .context("destination has no parent")?
            .ancestors()
            .take_while(|path| path.starts_with(to))
            .collect();
        for directory in directories.into_iter().rev() {
            match std::fs::create_dir(directory) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    return Err(error).with_context(|| format!("create {}", directory.display()))
                }
            }
            if !std::fs::symlink_metadata(directory)?.is_dir() {
                bail!(
                    "expected import directory, not a symlink or file: {}",
                    directory.display()
                );
            }
        }
        if copy_noclobber(&source, &destination, runner)? {
            *copied += 1;
        } else {
            *existing += 1;
        }
    }
    Ok(())
}

/// Publish a complete copy without replacing existing data. Return whether added.
fn copy_noclobber(source: &Path, destination: &Path, runner: &process::Runner) -> Result<bool> {
    runner.check_cancelled()?;
    let parent = destination.parent().context("destination has no parent")?;
    std::fs::create_dir_all(parent)?;
    let file = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::copy(source, file.path()).with_context(|| format!("copy {}", source.display()))?;
    runner.check_cancelled()?;
    match file.persist_noclobber(destination) {
        Ok(_) => Ok(true),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !files_equal(error.file.path(), destination, runner)? {
                bail!(
                    "refusing to overwrite different fuzz data: {}",
                    destination.display()
                );
            }
            Ok(false)
        }
        Err(error) => Err(error).with_context(|| format!("publish {}", destination.display())),
    }
}

fn files_equal(left: &Path, right: &Path, runner: &process::Runner) -> Result<bool> {
    match std::fs::symlink_metadata(right) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", right.display())),
        Ok(metadata) if !metadata.is_file() => return Ok(false),
        Ok(_) => {}
    }
    let mut left = std::fs::File::open(left)?;
    let mut right = std::fs::File::open(right)?;
    if left.metadata()?.len() != right.metadata()?.len() {
        return Ok(false);
    }
    let mut a = [0; 64 * 1024];
    let mut b = [0; 64 * 1024];
    loop {
        runner.check_cancelled()?;
        let count = left.read(&mut a)?;
        if count == 0 {
            return Ok(right.read(&mut b)? == 0);
        }
        if let Err(error) = right.read_exact(&mut b[..count]) {
            if error.kind() == std::io::ErrorKind::UnexpectedEof {
                return Ok(false);
            }
            return Err(error.into());
        }
        if a[..count] != b[..count] {
            return Ok(false);
        }
    }
}

/// `YYYYMMDD-HHMMSS` UTC stamp without pulling chrono into xtask.
pub(super) fn stamp() -> String {
    let (y, mo, d, h, mi, s) = unix_to_civil(unix_now());
    format!("{y:04}{mo:02}{d:02}-{h:02}{mi:02}{s:02}")
}

pub(super) fn iso_utc() -> String {
    let (y, mo, d, h, mi, s) = unix_to_civil(unix_now());
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}+00:00")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Howard Hinnant's `civil_from_days` - convert UNIX seconds (UTC) into
/// `(year, month, day, hour, minute, second)` without a dependency.
fn unix_to_civil(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    let day = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400) as u32;
    let hour = tod / 3600;
    let min = (tod / 60) % 60;
    let sec = tod % 60;
    let z = day + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i32 + era as i32 * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d, hour, min, sec)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_target_bootstraps_only_an_empty_corpus() {
        let directory = tempfile::tempdir().unwrap();
        let data = FuzzData {
            root: directory.path().to_path_buf(),
        };

        data.prepare_target("example").unwrap();
        assert_eq!(
            std::fs::read(data.corpus("example").join("bootstrap-zeroes")).unwrap(),
            vec![0; 256]
        );
        assert!(data.artifacts("example").is_dir());

        std::fs::remove_file(data.corpus("example").join("bootstrap-zeroes")).unwrap();
        std::fs::write(data.corpus("example").join("existing"), b"seed").unwrap();
        data.prepare_target("example").unwrap();
        assert!(!data.corpus("example").join("bootstrap-zeroes").exists());
        assert_eq!(
            std::fs::read(data.corpus("example").join("existing")).unwrap(),
            b"seed"
        );
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("fuzz").chain(args.iter().copied()))
    }

    #[test]
    fn build_timeout_defaults_validates_values_and_respects_the_fuzzer_separator() {
        assert_eq!(
            parse(&["build", "text_ops"]).unwrap().build_timeout,
            Duration::from_secs(1800)
        );
        for value in ["0", "-1", "1.5", "invalid", "4294967296", ""] {
            assert!(
                parse(&["build", "--build-timeout", value]).is_err(),
                "accepted {value}"
            );
        }
        for args in [
            vec!["build", "--build-timeout"],
            vec!["build", "--build-timeout", "0", "--build-timeout=20"],
            vec!["--build-timeout=0", "build", "--build-timeout=20"],
            vec!["run", "text_ops", "--", "--build-timeout", "10"],
            vec!["run", "text_ops", "-runs=1", "--build-timeout=3600"],
        ] {
            assert!(parse(&args).is_err(), "accepted {args:?}");
        }
        for args in [
            vec!["build", "--build-timeout=3600", "text_ops"],
            vec!["--build-timeout", "3600", "build", "text_ops"],
            vec!["run", "text_ops", "--build-timeout=3600", "-runs=1"],
            vec!["build", "--build-timeout=10", "--build-timeout=3600"],
            vec!["--build-timeout=10", "build", "--build-timeout=3600"],
        ] {
            assert_eq!(
                parse(&args).unwrap().build_timeout,
                Duration::from_secs(3600)
            );
        }
    }

    #[test]
    fn every_timed_command_uses_the_same_validation_and_help() {
        for (command, default) in [
            ("triage", 300),
            ("coverage-snapshot", 300),
            ("replay-regression", 30),
            ("verify", 30),
        ] {
            let mut args = vec![command, "text_ops"];
            if command == "triage" {
                args.push("artifact");
            }
            let timeout = |cli: Cli| match cli.command {
                FuzzCommand::Triage(options) => options.timeout,
                FuzzCommand::CoverageSnapshot(options) => options.timeout,
                FuzzCommand::ReplayRegression(options) | FuzzCommand::Verify(options) => {
                    options.timeout
                }
                _ => unreachable!(),
            };
            assert_eq!(timeout(parse(&args).unwrap()), Duration::from_secs(default));
            let mut valid = args.clone();
            valid.push("--timeout=60");
            assert_eq!(timeout(parse(&valid).unwrap()), Duration::from_secs(60));
            valid.push("--timeout=45");
            assert_eq!(timeout(parse(&valid).unwrap()), Duration::from_secs(45));
            for values in [
                vec!["--timeout"],
                vec!["--timeout=0"],
                vec!["--timeout=4294967296"],
                vec!["--timeout=-1"],
                vec!["--timeout=0", "--timeout=2"],
            ] {
                let mut invalid = args.clone();
                invalid.extend(values);
                assert!(parse(&invalid).is_err(), "accepted {invalid:?}");
            }
            let help = parse(&[command, "--help"]).err().unwrap();
            assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
            assert!(help.to_string().contains("--timeout"));
            assert!(help.to_string().contains("--build-timeout"));
        }
    }

    #[test]
    fn inputs_distinguish_missing_directories_from_invalid_roots() {
        let directory = tempfile::tempdir().unwrap();
        assert!(input_files(&directory.path().join("missing"))
            .unwrap()
            .is_empty());
        let file = directory.path().join("seed");
        std::fs::write(&file, b"seed").unwrap();
        assert!(input_files(&file).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn inputs_reject_symlinks_and_special_files() {
        let directory = tempfile::tempdir().unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(directory.path(), &link).unwrap();
        assert!(input_files(&link).is_err());
        assert!(input_files(directory.path()).is_err());
        std::fs::remove_file(link).unwrap();
        let _socket =
            std::os::unix::net::UnixListener::bind(directory.path().join("socket")).unwrap();
        assert!(input_files(directory.path()).is_err());
    }

    #[test]
    fn snapshots_and_comparisons_handle_inputs_larger_than_the_copy_buffer() {
        let directory = tempfile::tempdir().unwrap();
        let snapshot = tempfile::tempdir().unwrap();
        let runner = process::Runner::new().unwrap();
        let source = directory.path().join("large");
        let bytes = vec![42; 131073];
        std::fs::write(&source, &bytes).unwrap();
        let inputs = snapshot_corpus(directory.path(), snapshot.path(), &runner).unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].live, source);
        let copied = &inputs[0].copied;
        assert!(files_equal(copied, &source, &runner).unwrap());
        assert_eq!(std::fs::read(copied).unwrap(), bytes);
        let mut changed = bytes;
        *changed.last_mut().unwrap() = 43;
        std::fs::write(&source, changed).unwrap();
        assert!(!files_equal(copied, &source, &runner).unwrap());
    }
}
