//! `cargo xtask fuzz coverage-snapshot [target...]` - per-target source-code
//! coverage snapshot. Runs each target against its shared corpus and writes a
//! timestamped result directory under the shared fuzz-data root. With no args it
//! prepares and snapshots every registered target.

use super::build::{path_arg, Project};
use super::process::{CommandFailure, Output, OutputMode, Termination, QUERY_TIMEOUT};
use super::{all_target_names, corpus_digest, git_text, iso_utc, snapshot_corpus, stamp, FuzzData};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(clap::Args)]
pub(super) struct Options {
    /// Combined coverage build and replay watchdog, independent of --build-timeout
    #[arg(long, overrides_with = "timeout", default_value = "300", value_name = "SECONDS", value_parser = super::parse_seconds)]
    pub(super) timeout: Duration,
    #[arg(value_parser = super::parse_target)]
    targets: Vec<String>,
}

#[derive(Deserialize, Serialize)]
struct CoverageMetric {
    count: u64,
    covered: u64,
    percent: f64,
}

#[derive(Deserialize, Serialize)]
struct CoverageTotals {
    lines: CoverageMetric,
    functions: CoverageMetric,
    regions: CoverageMetric,
    branches: CoverageMetric,
    #[serde(flatten)]
    other: BTreeMap<String, CoverageMetric>,
}

#[derive(Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Status {
    Ok,
    Failed,
    Timeout,
    Interrupted,
}

#[derive(Serialize)]
struct TargetRecord {
    target: String,
    status: Status,
    corpus_files: Option<usize>,
    corpus_digest: Option<String>,
    totals: Option<CoverageTotals>,
    log: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    log_error: Option<String>,
}

impl TargetRecord {
    fn summary(&self) -> String {
        if let Some(totals) = &self.totals {
            format!(
                "{} ({}f): lines {:.2}%, functions {:.2}%, regions {:.2}%, branches {:.2}%",
                self.target,
                self.corpus_files
                    .expect("coverage requires a corpus snapshot"),
                totals.lines.percent,
                totals.functions.percent,
                totals.regions.percent,
                totals.branches.percent
            )
        } else {
            format!(
                "{}: {}; log={}",
                self.target,
                self.error.as_deref().unwrap_or("coverage unavailable"),
                self.log.as_deref().unwrap_or("unavailable")
            )
        }
    }
}

#[derive(Serialize)]
struct CoverageReport {
    schema: u32,
    date: String,
    commit: String,
    branch: String,
    data_root: String,
    targets: Vec<TargetRecord>,
}

impl CoverageReport {
    fn summary(&self) -> String {
        let mut text = format!(
            "# fuzz coverage snapshot\ndate: {}\ncommit: {}\nbranch: {}\n\n",
            self.date, self.commit, self.branch
        );
        for record in &self.targets {
            text.push_str(&record.summary());
            text.push('\n');
        }
        text
    }
}

pub fn run(project: &Project, options: Options) -> Result<()> {
    let mut targets = if options.targets.is_empty() {
        all_target_names()
    } else {
        options.targets
    };
    targets.sort();
    targets.dedup();
    let llvm_cov = project.llvm_cov()?;
    let data = FuzzData::for_repo(project)?;
    let history = data.coverage_history();
    std::fs::create_dir_all(&history)?;
    // cargo-fuzz uses checkout-local profile files, while Cargo can share builds
    // between checkouts. Hold both locks until the matching report is exported.
    let _profiles = coverage_lock(&project.root.join("fuzz/coverage"))?;
    let _build = coverage_lock(&project.build_dir("coverage"))?;
    let mut report = CoverageReport {
        schema: 4,
        date: iso_utc(),
        commit: git_text(project, &["rev-parse", "HEAD"])?,
        branch: git_text(project, &["rev-parse", "--abbrev-ref", "HEAD"])?,
        data_root: data.root.to_string_lossy().into_owned(),
        targets: Vec::new(),
    };
    let prefix = format!(
        ".{}-{}-",
        stamp(),
        report.commit.get(..9).unwrap_or(&report.commit)
    );
    let staging = tempfile::Builder::new()
        .prefix(&prefix)
        .tempdir_in(&history)?;
    let result_dir = staging.path().join("result");
    std::fs::create_dir(&result_dir)?;
    let published = history.join(
        staging
            .path()
            .file_name()
            .and_then(|name| name.to_str())
            .context("invalid coverage directory name")?
            .trim_start_matches('.'),
    );
    let mut failed = None;

    for target in targets {
        let mut log = None;
        let mut record = TargetRecord {
            target,
            status: Status::Ok,
            corpus_files: None,
            corpus_digest: None,
            totals: None,
            log: None,
            error: None,
            log_error: None,
        };
        let result: Result<()> = (|| {
            let name = format!("{}.log", record.target);
            log = Some(
                std::fs::File::create_new(result_dir.join(&name)).context("create target log")?,
            );
            record.log = Some(name);
            let log = log.as_mut().expect("target log opened");
            writeln!(log, "=== prepare corpus snapshot ===")?;
            project.runner.check_cancelled()?;
            data.prepare_target(&record.target)?;
            let snapshot = tempfile::Builder::new()
                .prefix(".coverage-corpus-")
                .tempdir_in(&data.root)?;
            let corpus = data.corpus(&record.target);
            let entries = snapshot_corpus(&corpus, snapshot.path(), &project.runner)?;
            let digest = corpus_digest(&corpus, &entries, &project.runner)?;
            record.corpus_files = Some(entries.len());
            record.corpus_digest = Some(digest);
            eprintln!(
                ">>> {}: {} corpus files (snapshot)",
                record.target,
                entries.len()
            );
            record.totals = Some(snapshot_target(
                project,
                &data,
                &record.target,
                snapshot.path(),
                &llvm_cov,
                options.timeout,
                log,
            )?);
            Ok(())
        })();
        if let Err(error) = result {
            record.status = match error
                .downcast_ref::<CommandFailure>()
                .map(|error| &error.termination)
            {
                Some(Termination::TimedOut) => Status::Timeout,
                Some(Termination::Interrupted(_)) => Status::Interrupted,
                _ => Status::Failed,
            };
            record.error = Some(format!("{error:#}"));
            if let Some(log) = &mut log {
                if let Err(error) = writeln!(log, "\n=== failed ===\n{error:#}") {
                    record.log_error = Some(error.to_string());
                }
            }
            if record.status == Status::Interrupted || failed.is_none() {
                failed = Some(error);
            }
        }
        let interrupted = record.status == Status::Interrupted;
        println!("{}", record.summary());
        report.targets.push(record);
        if interrupted {
            break;
        }
    }

    std::fs::write(
        result_dir.join("metadata.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    std::fs::write(result_dir.join("summary.txt"), report.summary())?;
    // Publish logs and both representations of the same report together, including
    // partial results collected before a failure or cancellation.
    std::fs::rename(&result_dir, &published).context("publish coverage report")?;
    eprintln!(
        "coverage summary: {}",
        published.join("summary.txt").display()
    );
    eprintln!(
        "coverage metadata: {}",
        published.join("metadata.json").display()
    );
    if let Some(error) = failed {
        return Err(error.context("one or more coverage targets failed"));
    }
    Ok(())
}

fn coverage_lock(dir: &Path) -> Result<std::fs::File> {
    std::fs::create_dir_all(dir)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join(".xtask.lock"))?;
    file.try_lock().with_context(|| {
        format!(
            "coverage already running or lock unavailable: {}",
            dir.display()
        )
    })?;
    Ok(file)
}

fn snapshot_target(
    project: &Project,
    data: &FuzzData,
    target: &str,
    corpus: &Path,
    llvm_cov: &Path,
    timeout: Duration,
    log: &mut std::fs::File,
) -> Result<CoverageTotals> {
    let profiles = project.root.join("fuzz/coverage").join(target);
    match std::fs::remove_dir_all(&profiles) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("remove stale coverage profiles"),
    }
    let mut command = project.cargo_fuzz("coverage", "none")?;
    command.arg(target).arg(corpus).arg("--").arg(path_arg(
        "-artifact_prefix=",
        &data.artifacts(target).join(""),
    ));
    logged_command(project, command, timeout, log, "cargo fuzz coverage")?;
    let binary = project.coverage_binary(target)?;
    let profdata = profiles.join("coverage.profdata");
    if !binary.is_file() || !profdata.is_file() {
        bail!("coverage command did not produce a binary and fresh profile");
    }
    let mut command = Command::new(llvm_cov);
    command
        .args(["export", "--summary-only"])
        .arg(binary)
        .arg(path_arg("-instr-profile=", &profdata))
        .arg("-ignore-filename-regex=/.cargo/|/rustc/|/.rustup/|/fuzz/")
        .current_dir(&project.root);
    let output = logged_command(project, command, QUERY_TIMEOUT, log, "llvm-cov export")?;
    parse_totals(&output.stdout.complete()?)
}

fn parse_totals(bytes: &[u8]) -> Result<CoverageTotals> {
    #[derive(Deserialize)]
    struct Export {
        data: [ExportData; 1],
    }
    #[derive(Deserialize)]
    struct ExportData {
        totals: CoverageTotals,
    }
    let Export { data: [data] } = serde_json::from_slice(bytes).context("parse llvm-cov export")?;
    Ok(data.totals)
}

fn logged_command(
    project: &Project,
    command: Command,
    timeout: Duration,
    log: &mut std::fs::File,
    stage: &str,
) -> Result<Output> {
    writeln!(log, "\n=== {stage} ===")?;
    let output = project
        .runner
        .run(command, Some(timeout), OutputMode::Capture)?;
    writeln!(
        log,
        "--- stdout ---\n{}\n--- stderr ---\n{}\ntermination: {}",
        output.stdout.text(),
        output.stderr.text(),
        output.termination
    )?;
    output.success(stage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corpus_snapshot_is_sorted_recursive_and_independent_of_live_edits() {
        let live = tempfile::tempdir().unwrap();
        let snapshot = tempfile::tempdir().unwrap();
        std::fs::create_dir(live.path().join("nested")).unwrap();
        std::fs::write(live.path().join("nested/b"), b"second").unwrap();
        std::fs::write(live.path().join("a"), b"first").unwrap();
        let runner = super::super::process::Runner::new().unwrap();
        let inputs = snapshot_corpus(live.path(), snapshot.path(), &runner).unwrap();
        let digest = corpus_digest(live.path(), &inputs, &runner).unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].live, live.path().join("a"));
        assert_eq!(inputs[1].live, live.path().join("nested/b"));
        assert_eq!(std::fs::read(&inputs[0].copied).unwrap(), b"first");
        std::fs::write(live.path().join("a"), b"changed").unwrap();
        assert_eq!(std::fs::read(&inputs[0].copied).unwrap(), b"first");
        assert_eq!(
            corpus_digest(live.path(), &inputs, &runner).unwrap(),
            digest
        );
        let second = tempfile::tempdir().unwrap();
        let changed = snapshot_corpus(live.path(), second.path(), &runner).unwrap();
        assert_ne!(
            corpus_digest(live.path(), &changed, &runner).unwrap(),
            digest
        );
    }

    #[test]
    fn refuses_overlapping_coverage_and_releases_lock_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let lock = coverage_lock(dir.path()).unwrap();
        assert!(coverage_lock(dir.path()).is_err());
        drop(lock);
        assert!(coverage_lock(dir.path()).is_ok());
    }

    #[test]
    fn rejects_incomplete_or_ambiguous_llvm_exports() {
        for document in [
            serde_json::json!({}),
            serde_json::json!({"data": []}),
            serde_json::json!({"data": [{"totals": {}}]}),
            serde_json::json!({"data": [{"totals": {}}, {"totals": {}}]}),
            serde_json::json!({"data": [{"totals": {"lines": {"count": "invalid"}}}]}),
        ] {
            assert!(parse_totals(&serde_json::to_vec(&document).unwrap()).is_err());
        }
    }
}
