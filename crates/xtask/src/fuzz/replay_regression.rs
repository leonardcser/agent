//! Replay tracked regressions with an independent deadline and failure report for
//! every seed. Compilation is independent of regression seed presence.

use super::build::Project;
use super::process::{OutputMode, Termination};
use super::{FuzzData, TargetKind, TARGETS};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

#[derive(clap::Args)]
pub(super) struct Options {
    /// Watchdog for each regression seed
    #[arg(long, overrides_with = "timeout", default_value = "30", value_name = "SECONDS", value_parser = super::parse_seconds)]
    pub timeout: Duration,
    #[arg(value_parser = super::parse_target)]
    pub targets: Vec<String>,
}

pub fn run(project: &Project, options: Options) -> Result<()> {
    project.build_targets(&options.targets, "none")?;
    run_prebuilt(project, options)
}

pub(super) fn run_prebuilt(project: &Project, options: Options) -> Result<()> {
    let targets: Vec<_> = TARGETS
        .iter()
        .filter(|target| {
            options.targets.is_empty() || options.targets.iter().any(|name| name == target.name)
        })
        .collect();
    if targets.iter().any(|target| target.kind == TargetKind::Json) {
        project.build_helpers(&["replay_scenario"])?;
    }
    let data = FuzzData::for_repo(project)?;
    let mut failed = None;
    let mut passed = 0;
    for target in targets {
        let dir = project
            .root
            .join("fuzz/seeds")
            .join(target.name)
            .join("regression");
        let files = regression_files(&dir, target.kind)?;
        println!(">>> {}: {} regression seed(s)", target.name, files.len());
        if target.kind == TargetKind::Bytes && !files.is_empty() {
            data.prepare_target(target.name)?;
        }
        for seed in files {
            let mut command = match target.kind {
                TargetKind::Json => {
                    let mut command = Command::new(project.helper("replay_scenario")?);
                    command
                        .args(["--target", target.name])
                        .current_dir(&project.root);
                    command
                }
                TargetKind::Bytes => {
                    let mut command = project.target_command(target.name, "none", &data)?;
                    command.arg("-runs=1");
                    command
                }
            };
            command.arg(&seed);
            let label = format!("{}: {}", target.name, seed.strip_prefix(&dir)?.display());
            let output = project
                .runner
                .run(command, Some(options.timeout), OutputMode::Capture)?;
            let interrupted = matches!(output.termination, Termination::Interrupted(_));
            match output.success(&label) {
                Ok(_) => {
                    passed += 1;
                    println!("  ok   {label}");
                }
                Err(error) if interrupted => return Err(error),
                Err(error) => {
                    eprintln!("  FAIL {error:#}");
                    if failed.is_none() {
                        failed = Some(error);
                    }
                }
            }
        }
    }
    if let Some(error) = failed {
        return Err(error.context(format!("regression replay failed; {passed} seeds passed")));
    }
    println!("all {passed} regression seeds passed");
    Ok(())
}

pub(super) fn regression_files(dir: &Path, kind: TargetKind) -> Result<Vec<PathBuf>> {
    let files = super::input_files(dir)?;
    if kind == TargetKind::Json {
        for path in &files {
            if path.extension().is_none_or(|extension| extension != "json") {
                bail!(
                    "structured regression must be a .json file: {}",
                    path.display()
                );
            }
        }
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_nested_regressions_and_rejects_wrong_formats() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("nested")).unwrap();
        let seed = dir.path().join("nested/example.json");
        std::fs::write(&seed, b"{}").unwrap();
        assert_eq!(
            regression_files(dir.path(), TargetKind::Json).unwrap(),
            vec![seed]
        );
        std::fs::write(dir.path().join("unexpected"), b"bytes").unwrap();
        assert!(regression_files(dir.path(), TargetKind::Json).is_err());
        assert_eq!(
            regression_files(dir.path(), TargetKind::Bytes)
                .unwrap()
                .len(),
            2
        );
    }
}
