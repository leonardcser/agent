//! `cargo xtask fuzz status` - compact status for local fuzzing state.

use super::build::Project;
use super::replay_regression::regression_files;
use super::{input_files, input_files_filtered, FuzzData, TARGETS};
use anyhow::Result;
use std::path::{Path, PathBuf};

pub fn run(project: &Project) -> Result<()> {
    let root = &project.root;
    let data = FuzzData::for_repo(project)?;
    println!("fuzz status");
    println!("repository: {}", root.display());
    println!("data: {}", data.root.display());
    println!();

    print_processes(&project.runner)?;
    println!();

    println!(
        "{:<28} {:>8} {:>8} {:>12} {:>10}",
        "target", "corpus", "size", "regressions", "artifacts"
    );
    for target in TARGETS {
        let corpus = input_files(&data.corpus(target.name))?;
        let seeds = root.join("fuzz/seeds").join(target.name).join("regression");
        let seed_files = regression_files(&seeds, target.kind)?.len();
        let artifact_files = input_files(&data.artifacts(target.name))?.len();
        println!(
            "{:<28} {:>8} {:>8} {:>12} {:>10}",
            target.name,
            corpus.len(),
            corpus_size(&corpus)?,
            seed_files,
            artifact_files
        );
    }
    println!();
    print_latest_coverage(&data)
}

fn print_processes(_runner: &super::process::Runner) -> Result<()> {
    #[cfg(unix)]
    {
        use super::process::{OutputMode, QUERY_TIMEOUT};
        let mut command = std::process::Command::new("ps");
        command.args(["-eo", "pid=,comm=,args="]);
        let output = _runner
            .run(command, Some(QUERY_TIMEOUT), OutputMode::Capture)?
            .success("list fuzz processes")?;
        let bytes = output.stdout.complete()?;
        let text = String::from_utf8_lossy(&bytes);
        let mut rows = Vec::new();
        for line in text
            .lines()
            .filter(|line| !line.contains("xtask fuzz status"))
        {
            let mut words = line.split_whitespace();
            let (Some(pid), Some(name)) = (words.next(), words.next()) else {
                continue;
            };
            if line.contains("xtask fuzz")
                || name == "cargo-fuzz"
                || line.contains("/smelt-fuzz/")
                || TARGETS.iter().any(|target| target.name == name)
            {
                // Arguments can contain unrelated sensitive data. Display identity only.
                rows.push(format!("  {pid} {name}"));
            }
        }
        if rows.is_empty() {
            println!("processes: none");
        } else {
            println!("processes (PID, executable):\n{}", rows.join("\n"));
        }
    }
    #[cfg(not(unix))]
    println!("processes: unavailable on this platform");
    Ok(())
}

fn corpus_size(files: &[PathBuf]) -> Result<String> {
    let mut bytes = 0u64;
    for path in files {
        bytes = bytes.saturating_add(std::fs::metadata(path)?.len());
    }
    let mut size = bytes as f64;
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"] {
        if size < 1024.0 || unit == "TiB" {
            return Ok(format!("{size:.0}{unit}"));
        }
        size /= 1024.0;
    }
    unreachable!()
}

fn latest_coverage(history: &Path) -> Result<Option<PathBuf>> {
    // Staging directories are private until the complete report is renamed.
    let files = input_files_filtered(history, |entry| {
        !entry.file_name().as_encoded_bytes().starts_with(b".")
    })?;
    let mut reports = Vec::new();
    for path in files {
        if path.extension().is_some_and(|extension| extension == "txt") {
            reports.push((std::fs::metadata(&path)?.modified()?, path));
        }
    }
    Ok(reports.into_iter().max().map(|(_, path)| path))
}

fn print_latest_coverage(data: &FuzzData) -> Result<()> {
    let Some(path) = latest_coverage(&data.coverage_history())? else {
        println!("coverage: no snapshots");
        return Ok(());
    };
    println!("coverage: {}", path.display());
    for line in std::fs::read_to_string(&path)?
        .lines()
        .skip(5)
        .filter(|line| !line.trim().is_empty())
    {
        println!("  {line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coverage_status_only_discovers_published_reports() {
        let history = tempfile::tempdir().unwrap();
        let staging = history.path().join(".pending/result");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("summary.txt"), "incomplete").unwrap();
        assert!(latest_coverage(history.path()).unwrap().is_none());
        let published = history.path().join("completed");
        std::fs::rename(&staging, &published).unwrap();
        assert_eq!(
            latest_coverage(history.path()).unwrap(),
            Some(published.join("summary.txt"))
        );
    }

    #[test]
    fn coverage_status_reads_standalone_text_reports() {
        let history = tempfile::tempdir().unwrap();
        let summary = history.path().join("snapshot.txt");
        std::fs::write(&summary, "report").unwrap();
        std::fs::write(history.path().join("snapshot.json"), "{}").unwrap();
        assert_eq!(latest_coverage(history.path()).unwrap(), Some(summary));
    }
}
