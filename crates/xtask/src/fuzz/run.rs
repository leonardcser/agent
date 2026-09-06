//! `cargo xtask fuzz run <target> [--fork N] [--cmin]` - fuzz a single target.
//!
//! Builds once, then runs the target directly with the operational flags:
//!   - preflight corpus replay without `-fork`, so stale corpus crashes fail
//!     before libFuzzer's fork-mode merge can keep going after writing artifacts.
//!   - `-ignore_crashes=0`, `-ignore_ooms=0`, and `-ignore_timeouts=0` so
//!     the first new fork-worker hard failure drops an artifact and exits.
//!   - `-fork=N` for parallel workers (default 1).
//!   - optional `--cmin` minimizes an independently replayed corpus snapshot.
//!
//! No time budget - runs until crash or Ctrl-C. To bound a session, pass
//! `-max_total_time=<secs>` after the target. Only explicit tuning flags are forwarded.

use super::build::{path_arg, Project};
use super::process::OutputMode;
use super::{copy_noclobber, files_equal, input_files, snapshot_corpus, FuzzData};
use anyhow::{bail, Context, Result};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(300);
const FAIL_FAST: [&str; 3] = ["-ignore_crashes=0", "-ignore_ooms=0", "-ignore_timeouts=0"];

#[derive(clap::Args)]
pub(super) struct Options {
    #[arg(value_parser = super::parse_target)]
    target: String,
    /// Parallel worker count
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(i32).range(1..))]
    fork: i32,
    /// Minimize a corpus snapshot before fuzzing
    #[arg(long)]
    cmin: bool,
    #[arg(long, default_value = "address", value_parser = ["address", "leak", "memory", "thread", "none"])]
    sanitizer: String,
    /// Trailing libFuzzer tuning options; place wrapper options before these
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, value_name = "-flag=value", value_parser = tuning_flag)]
    extra: Vec<String>,
}

fn tuning_flag(arg: &str) -> Result<String, String> {
    let Some((flag, value)) = arg.split_once('=') else {
        return Err(format!("expected a libFuzzer -flag=value, got `{arg}`"));
    };
    // Only tuning is forwarded. Mode, parser, signal, output and exit-code
    // controls must not bypass campaign supervision, even in newer libFuzzer versions.
    let valid = match flag {
        "-runs" => value.parse::<i32>().is_ok_and(|value| value >= -1),
        "-seed" => value.parse::<u32>().is_ok(),
        "-mutate_depth" => value.parse::<i32>().is_ok_and(|value| value > 0),
        "-max_total_time" | "-max_len" | "-len_control" | "-timeout"
        | "-rss_limit_mb" | "-malloc_limit_mb" | "-reload" | "-report_slow_units"
        | "-verbosity" | "-print_funcs" | "-entropic_feature_frequency_threshold"
        | "-entropic_number_of_rarest_features" => {
            value.parse::<i32>().is_ok_and(|value| value >= 0)
        }
        "-cross_over" | "-cross_over_uniform_dist" | "-reduce_inputs" | "-keep_seed"
        | "-shuffle" | "-prefer_small" | "-only_ascii" | "-use_counters"
        | "-use_memmem" | "-use_value_profile" | "-use_cmp" | "-entropic"
        | "-entropic_scale_per_exec_time" | "-fork_corpus_groups" | "-print_pcs"
        | "-print_final_stats" | "-print_corpus_stats" | "-print_coverage"
        | "-print_full_coverage" => matches!(value, "0" | "1"),
        _ => return Err(format!("unsupported campaign tuning flag `{flag}`; use --fork for parallelism; see fuzz/README.md")),
    };
    if !valid {
        return Err(format!("invalid value for campaign tuning flag `{arg}`"));
    }
    Ok(arg.to_string())
}

pub fn run(project: &Project, options: Options) -> Result<()> {
    let Options {
        target,
        fork,
        cmin,
        sanitizer,
        extra,
    } = options;
    let data = FuzzData::for_repo(project)?;
    data.prepare_target(&target)?;
    let corpus = data.corpus(&target);
    project.build_targets(std::slice::from_ref(&target), &sanitizer)?;

    if cmin {
        minimize_corpus(project, &data, &target, &sanitizer)?;
    }
    preflight(project, &data, &target, &sanitizer, &corpus)?;

    println!(">>> fuzz {target} (fork={fork}, sanitizer={sanitizer})");
    let mut command = project.target_command(&target, &sanitizer, &data)?;
    command
        .arg(&corpus)
        .arg(format!("-fork={fork}"))
        .args(FAIL_FAST)
        .args(extra);
    project
        .runner
        .run(command, None, OutputMode::Inherit)?
        .success(&format!(
            "fuzz {target}; inspect {} for failure artifacts",
            data.artifacts(&target).display()
        ))?;
    Ok(())
}

fn preflight(
    project: &Project,
    data: &FuzzData,
    target: &str,
    sanitizer: &str,
    corpus: &Path,
) -> Result<()> {
    println!("xtask fuzz: preflight corpus {target}");
    let mut command = project.target_command(target, sanitizer, data)?;
    command.arg(corpus).arg("-runs=0").args(FAIL_FAST);
    project
        .runner
        .run(command, Some(MAINTENANCE_TIMEOUT), OutputMode::Inherit)?
        .success(&format!(
            "{target} corpus preflight; inspect {}",
            data.artifacts(target).display()
        ))?;
    Ok(())
}

fn minimize_corpus(
    project: &Project,
    data: &FuzzData,
    target: &str,
    sanitizer: &str,
) -> Result<()> {
    let corpus = data.corpus(target);
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data.root.join(format!(".{target}.cmin.lock")))?;
    lock.try_lock()
        .context("corpus minimization already running or lock unavailable")?;
    let temporary = tempfile::Builder::new()
        .prefix(".cmin-")
        .tempdir_in(&data.root)?;
    let snapshot = temporary.path().join("inputs");
    let minimized = temporary.path().join("minimized");
    std::fs::create_dir(&snapshot)?;
    std::fs::create_dir(&minimized)?;
    let originals = snapshot_corpus(&corpus, &snapshot, &project.runner)?;
    // libFuzzer's merge mode can skip crashing inputs. Replay exactly the snapshot
    // being minimized before allowing it to remove anything from the live corpus.
    preflight(project, data, target, sanitizer, &snapshot)?;
    println!(">>> cmin {target}");
    let mut command = project.target_command(target, sanitizer, data)?;
    command
        .arg("-merge=1")
        .arg(&minimized)
        .arg(&snapshot)
        .arg(path_arg(
            "-merge_control_file=",
            &temporary.path().join("merge-control"),
        ));
    project
        .runner
        .run(command, Some(MAINTENANCE_TIMEOUT), OutputMode::Inherit)?
        .success("minimize corpus")?;
    let selected = input_files(&minimized)?;
    if selected.is_empty() {
        bail!("corpus minimizer produced no inputs; leaving the live corpus unchanged");
    }
    let mut published = HashSet::new();
    // Publish every selected input before pruning. Interruption or a copy failure
    // can leave extra inputs, but can never remove the only copy of selected coverage.
    for source in selected {
        project.runner.check_cancelled()?;
        let destination = corpus.join(source.strip_prefix(&minimized)?);
        copy_noclobber(&source, &destination, &project.runner)?;
        published.insert(destination);
    }
    for input in originals {
        project.runner.check_cancelled()?;
        if !published.contains(&input.live)
            && files_equal(&input.copied, &input.live, &project.runner)?
        {
            std::fs::remove_file(&input.live)
                .with_context(|| format!("prune {}", input.live.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{Cli, FuzzCommand};
    use super::*;
    use clap::Parser;

    fn parse(args: Vec<String>) -> Result<Options> {
        let cli = Cli::try_parse_from(["fuzz", "run"].map(str::to_string).into_iter().chain(args))?;
        let FuzzCommand::Run(options) = cli.command else {
            unreachable!()
        };
        Ok(options)
    }

    #[test]
    fn rejects_zero_forks_and_operational_overrides() {
        for flags in [
            vec!["--fork", "0"],
            vec!["--fork", "-1"],
            vec!["--fork", "4294967295"],
            vec!["-max_len=-1"],
            vec!["-max_total_time=2147483648"],
            vec!["-seed=4294967296"],
            vec!["-mutate_depth=0"],
            vec!["-use_value_profile=2"],
            vec!["-fork=0"],
            vec!["-ignore_crashes=1"],
            vec!["--", "-artifact_prefix=/tmp/"],
            vec!["other-corpus"],
            vec!["--sanitizer", "bogus"],
        ] {
            let args = std::iter::once("text_ops")
                .chain(flags)
                .map(str::to_string)
                .collect();
            assert!(parse(args).is_err());
        }
    }

    #[test]
    fn preserves_non_operational_fuzzer_flags() {
        let options = parse(
            [
                "text_ops",
                "--fork",
                "2",
                "--sanitizer",
                "none",
                "--",
                "-max_total_time=10",
                "-max_len=512",
            ]
            .map(str::to_string)
            .to_vec(),
        )
        .unwrap();
        assert_eq!(options.fork, 2);
        assert_eq!(options.sanitizer, "none");
        assert_eq!(options.extra, ["-max_total_time=10", "-max_len=512"]);
    }
}
