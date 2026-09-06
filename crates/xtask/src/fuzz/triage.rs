//! `cargo xtask fuzz triage <target> <crash-artifact>` - minimize a crash while
//! preserving its failure identity. Structured targets produce JSON scenarios;
//! byte targets retain exact libFuzzer inputs.

use super::build::{path_arg, Project};
use super::process::{OutputMode, Termination};
use super::{iso_utc, target_named, FuzzData, TargetKind};
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[derive(clap::Args)]
pub(super) struct Options {
    #[arg(value_parser = super::parse_target)]
    target: String,
    artifact: PathBuf,
    /// Watchdog for each decode, shrink, or replay step, excluding builds
    #[arg(long, overrides_with = "timeout", default_value = "300", value_name = "SECONDS", value_parser = super::parse_seconds)]
    pub(super) timeout: Duration,
}

pub fn run(project: &Project, options: Options) -> Result<()> {
    let timeout = options.timeout;
    let target = target_named(&options.target).context("unknown fuzz target")?;
    let artifact = options
        .artifact
        .canonicalize()
        .context("locate crash artifact")?;
    if !artifact.is_file() {
        bail!("artifact is not a regular file: {}", artifact.display());
    }
    let parent = artifact
        .parent()
        .context("artifact has no parent directory")?;
    let temporary = tempfile::Builder::new()
        .prefix(".triage-")
        .tempdir_in(parent)?;
    let result = temporary.path().join("result");
    std::fs::create_dir(&result)?;
    let name = if target.kind == TargetKind::Json {
        "minimized.json"
    } else {
        "minimized"
    };
    let candidate = temporary.path().join(name);
    let identity = match target.kind {
        TargetKind::Json => triage_json(project, target.name, &artifact, &candidate, timeout)?,
        TargetKind::Bytes => triage_bytes(project, target.name, &artifact, &candidate, timeout)?,
    };
    std::fs::rename(&candidate, result.join(name))?;
    let metadata = serde_json::json!({
        "schema": 3,
        "target": target.name,
        "input_kind": match target.kind { TargetKind::Json => "json", TargetKind::Bytes => "bytes" },
        "sanitizer": match target.kind { TargetKind::Json => "structured-replay", TargetKind::Bytes => "address" },
        "commit": super::git_text(project, &["rev-parse", "HEAD"] )?,
        "triaged_at": iso_utc(),
        "original": artifact.to_string_lossy(),
        "minimized": name,
        "original_bytes": std::fs::metadata(&artifact)?.len(),
        "minimized_bytes": std::fs::metadata(result.join(name))?.len(),
        "failure_fingerprint": identity,
    });
    std::fs::write(
        result.join("metadata.json"),
        serde_json::to_vec_pretty(&metadata)?,
    )?;
    project.runner.check_cancelled()?;
    // The unique staging name reserves the result identity. One rename exposes
    // the verified artifact and its metadata together, never a partial pair.
    let mut published_name = artifact
        .file_name()
        .context("artifact has no name")?
        .to_os_string();
    published_name.push(
        temporary
            .path()
            .file_name()
            .context("staging directory has no name")?,
    );
    let published = parent.join(published_name);
    std::fs::rename(&result, &published).context("publish verified triage result")?;
    eprintln!("minimized artifact: {}", published.join(name).display());
    eprintln!(
        "triage metadata: {}",
        published.join("metadata.json").display()
    );
    eprintln!("failure fingerprint: {identity}");
    eprintln!(
        "regression destination: fuzz/seeds/{}/regression/",
        target.name
    );
    Ok(())
}

fn triage_json(
    project: &Project,
    target: &str,
    artifact: &Path,
    candidate: &Path,
    timeout: Duration,
) -> Result<String> {
    project.build_helpers(&["crash_to_scenario", "shrink_scenario", "replay_scenario"])?;
    let raw = candidate.with_extension("raw.json");
    let mut decode = Command::new(project.helper("crash_to_scenario")?);
    decode
        .args(["--target", target])
        .arg(artifact)
        .arg(&raw)
        .current_dir(&project.root);
    project
        .runner
        .run(decode, Some(timeout), OutputMode::Capture)?
        .success("decode crash scenario")?;
    let replay = |path: &Path| -> Result<Command> {
        let mut command = Command::new(project.helper("replay_scenario")?);
        command
            .args(["--target", target])
            .arg(path)
            .current_dir(&project.root);
        Ok(command)
    };
    let original = replay_fingerprint(project, replay(&raw)?, timeout)?;
    let mut shrink = Command::new(project.helper("shrink_scenario")?);
    shrink
        .args(["--target", target])
        .arg(&raw)
        .arg(candidate)
        .current_dir(&project.root);
    project
        .runner
        .run(shrink, Some(timeout), OutputMode::Capture)?
        .success("shrink crash scenario")?;
    verify_identity(
        &original,
        &replay_fingerprint(project, replay(candidate)?, timeout)?,
    )?;
    Ok(original)
}

fn triage_bytes(
    project: &Project,
    target: &str,
    artifact: &Path,
    candidate: &Path,
    timeout: Duration,
) -> Result<String> {
    project.build_targets(&[target.to_string()], "address")?;
    let data = FuzzData::for_repo(project)?;
    data.prepare_target(target)?;
    let replay = |path: &Path| -> Result<Command> {
        let mut command = project.target_command(target, "address", &data)?;
        command.arg("-runs=1").arg(path);
        Ok(command)
    };
    let original = replay_fingerprint(project, replay(artifact)?, timeout)?;
    // Supervise each mutation step and replay to bound diagnostics, preserve the
    // original failure identity, and share one deadline across the shrink loop.
    let started = Instant::now();
    let next = candidate.with_extension("next");
    let mut current = artifact;
    loop {
        let mut minimize = project.target_command(target, "address", &data)?;
        minimize
            .args(["-minimize_crash_internal_step=1", "-runs=100000"])
            .arg(path_arg("-exact_artifact_path=", &next))
            .arg(current);
        let output = project.runner.run(
            minimize,
            Some(timeout.saturating_sub(started.elapsed())),
            OutputMode::Capture,
        )?;
        if !matches!(output.termination, Termination::Exited(_)) {
            output.success("minimize byte artifact")?;
            unreachable!();
        }
        // A successful step found no smaller crash, including a one-byte input.
        if output.termination.success() {
            if current != candidate {
                std::fs::copy(current, candidate)?;
            }
            break;
        }
        if !next.is_file() {
            output.print_diagnostics();
            bail!("minimizer did not write {}", next.display());
        }
        if std::fs::metadata(&next)?.len() >= std::fs::metadata(current)?.len() {
            output.print_diagnostics();
            bail!("minimizer did not shrink the input");
        }
        verify_identity(
            &original,
            &replay_fingerprint(
                project,
                replay(&next)?,
                timeout.saturating_sub(started.elapsed()),
            )?,
        )?;
        std::fs::rename(&next, candidate)?;
        current = candidate;
    }
    verify_identity(
        &original,
        &replay_fingerprint(project, replay(candidate)?, timeout)?,
    )?;
    Ok(original)
}

fn verify_identity(original: &str, minimized: &str) -> Result<()> {
    if original != minimized {
        bail!("minimized artifact changed failure identity\noriginal: {original}\nminimized: {minimized}");
    }
    Ok(())
}

fn replay_fingerprint(project: &Project, command: Command, timeout: Duration) -> Result<String> {
    let output = project
        .runner
        .run(command, Some(timeout), OutputMode::Capture)?;
    if !matches!(output.termination, Termination::Exited(_)) {
        output.success("replay crash artifact")?;
        unreachable!();
    }
    if output.termination.success() {
        bail!("artifact does not fail when replayed");
    }
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout.complete()?),
        String::from_utf8_lossy(&output.stderr.complete()?)
    );
    failure_fingerprint(&text).with_context(|| {
        output.print_diagnostics();
        "failing replay did not contain a recognizable crash fingerprint"
    })
}

fn failure_fingerprint(output: &str) -> Option<String> {
    let lines: Vec<_> = output.lines().collect();
    let mut panic_identity = Vec::new();
    let mut sanitizer_summaries = Vec::new();
    let mut runtime_errors = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.contains("panicked at") {
            // Source line and column are part of the identity. Normalizing them
            // would collapse unrelated assertions in the same target file.
            panic_identity.push(normalize_panic_line(line));
            if let Some(message) = lines[index + 1..]
                .iter()
                .map(|line| line.trim())
                .find(|line| !line.is_empty())
                .filter(|line| !line.starts_with("stack backtrace:") && !line.starts_with("note:"))
            {
                panic_identity.push(normalize_numbers(message));
            }
        } else if line.contains("SUMMARY: AddressSanitizer:")
            || line.contains("SUMMARY: MemorySanitizer:")
            || line.contains("SUMMARY: ThreadSanitizer:")
            || line.contains("SUMMARY: LeakSanitizer:")
            || line.contains("SUMMARY: UndefinedBehaviorSanitizer:")
        {
            // Keep source positions and sanitizer categories distinct.
            sanitizer_summaries.push(line.trim().to_string());
        } else if let Some((location, message)) = line.split_once("runtime error:") {
            runtime_errors.push(format!(
                "{}runtime error:{}",
                location.trim_start(),
                normalize_numbers(message)
            ));
        }
    }
    let relevant = if !panic_identity.is_empty() {
        panic_identity
    } else if !sanitizer_summaries.is_empty() {
        sanitizer_summaries
    } else {
        runtime_errors
    };
    (!relevant.is_empty()).then(|| relevant.join(" | "))
}

fn normalize_panic_line(line: &str) -> String {
    let trimmed = line.trim();
    let Some((thread, rest)) = trimmed.split_once(" panicked at ") else {
        return trimmed.to_string();
    };
    let thread = thread
        .strip_suffix(')')
        .and_then(|prefix| prefix.rsplit_once(" (").map(|(prefix, _)| prefix))
        .unwrap_or(thread);
    format!("{thread} panicked at {rest}")
}

fn normalize_numbers(line: &str) -> String {
    let mut normalized = String::with_capacity(line.len());
    let mut in_digits = false;
    for ch in line.chars() {
        if ch.is_ascii_digit() {
            if !in_digits {
                normalized.push('#');
            }
            in_digits = true;
        } else {
            in_digits = false;
            normalized.push(ch);
        }
    }
    normalized
}

#[cfg(test)]
mod tests {
    use super::failure_fingerprint;

    #[test]
    fn panic_fingerprint_preserves_location_and_normalizes_message_numbers() {
        let first = failure_fingerprint(
            "thread '<unnamed>' (1234) panicked at fuzz_targets/example.rs:42:7:\nvalue 123 failed",
        );
        let minimized = failure_fingerprint(
            "thread '<unnamed>' (9) panicked at fuzz_targets/example.rs:42:7:\nvalue 9 failed",
        );
        let other_location = failure_fingerprint(
            "thread '<unnamed>' panicked at fuzz_targets/example.rs:43:7:\nvalue 9 failed",
        );

        assert_eq!(first, minimized);
        assert_ne!(first, other_location);
        let first = first.unwrap();
        assert!(first.contains("example.rs:42:7"));
        assert!(first.contains("value # failed"));
    }

    #[test]
    fn rejects_tool_errors_and_preserves_sanitizer_source_locations() {
        for text in [
            "ERROR: could not compile fuzz target",
            "SUMMARY: libFuzzer: timeout",
            "exit status: 1",
        ] {
            assert_eq!(failure_fingerprint(text), None);
        }
        let first = failure_fingerprint(
            "SUMMARY: AddressSanitizer: heap-buffer-overflow example.rs:42:7 in example",
        );
        let second = failure_fingerprint(
            "SUMMARY: AddressSanitizer: heap-buffer-overflow example.rs:43:7 in example",
        );
        assert!(first.is_some());
        assert_ne!(first, second);
    }

    #[test]
    fn panic_fingerprint_distinguishes_messages_at_the_same_location() {
        let first = failure_fingerprint(
            "thread '<unnamed>' panicked at fuzz_targets/example.rs:42:7:\nfirst invariant",
        );
        let second = failure_fingerprint(
            "thread '<unnamed>' panicked at fuzz_targets/example.rs:42:7:\nsecond invariant",
        );

        assert_ne!(first, second);
    }
}
