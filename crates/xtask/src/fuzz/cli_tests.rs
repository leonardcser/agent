//! Subprocess tests exercise the CLI dispatcher with real Cargo metadata and
//! controlled tools. Each test owns a repository, build directory and data root.

use super::process::{self, Output, OutputMode};
use super::TARGETS;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn cli_entry() {
    let Some(root) = std::env::var_os("FUZZ_CLI_TEST_ROOT") else {
        return;
    };
    std::fs::write(
        Path::new(&root).join("cli.pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    let args = serde_json::from_str(&std::env::var("FUZZ_CLI_TEST_ARGS").unwrap()).unwrap();
    super::run_at(root.into(), args);
}

#[cfg(target_os = "linux")]
#[test]
fn cli_reexec_survives_replacement_of_the_running_test_executable() {
    if let Some(root) = std::env::var_os("FUZZ_CLI_REEXEC_ROOT") {
        let root = PathBuf::from(root);
        let executable = std::env::current_exe().unwrap();
        assert_eq!(executable, root.join("test-launcher"));
        std::fs::remove_file(executable).unwrap();
        process::Runner::new()
            .unwrap()
            .run(
                cli_command(&root, &["--help"]),
                Some(Duration::from_secs(20)),
                OutputMode::Capture,
            )
            .unwrap()
            .success("re-execute the CLI from an unlinked test image")
            .unwrap();
        return;
    }
    let fixture = Fixture::new();
    let executable = fixture.root().join("test-launcher");
    std::fs::copy("/proc/self/exe", &executable).unwrap();
    let mut command = Command::new(executable);
    command
        .args([
            "--exact",
            "fuzz::cli_tests::cli_reexec_survives_replacement_of_the_running_test_executable",
            "--nocapture",
        ])
        .env("FUZZ_CLI_REEXEC_ROOT", fixture.root());
    fixture
        .runner
        .run(command, Some(Duration::from_secs(30)), OutputMode::Capture)
        .unwrap()
        .success("run the isolated executable-replacement probe")
        .unwrap();
}

pub(super) fn cli_command(root: &Path, args: &[&str]) -> Command {
    // /proc keeps the running image available when Cargo replaces its pathname.
    #[cfg(target_os = "linux")]
    let executable = if Path::new("/proc/self/exe").is_file() {
        PathBuf::from("/proc/self/exe")
    } else {
        std::env::current_exe().unwrap()
    };
    #[cfg(not(target_os = "linux"))]
    let executable = std::env::current_exe().unwrap();
    let mut command = Command::new(executable);
    command
        .args(["--exact", "fuzz::cli_tests::cli_entry", "--nocapture"])
        .current_dir(root.join("invocation"))
        .env("FUZZ_CLI_TEST_ROOT", root)
        .env("FUZZ_CLI_TEST_ARGS", serde_json::to_string(args).unwrap())
        .env("CARGO_TARGET_DIR", "relative build")
        .env("SMELT_FUZZ_HOME", "relative data");
    command
}

struct Fixture {
    directory: tempfile::TempDir,
    runner: process::Runner,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        for name in [
            "fuzz/fuzz_targets",
            "tools",
            "invocation",
            "llvm/lib/rustlib/test-host/bin",
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let mut manifest =
            "[package]\nname = 'smelt-fuzz'\nversion = '0.0.0'\nedition = '2021'\n[workspace]\n"
                .to_string();
        for target in TARGETS {
            manifest.push_str(&format!(
                "[[bin]]\nname = '{}'\npath = 'fuzz_targets/{}.rs'\n",
                target.name, target.name
            ));
            std::fs::write(
                root.join(format!("fuzz/fuzz_targets/{}.rs", target.name)),
                "fn main() {}\n",
            )
            .unwrap();
        }
        std::fs::write(root.join("fuzz/Cargo.toml"), manifest).unwrap();
        script(&root.join("tools/cargo"), CARGO);
        script(&root.join("tools/rustc"), RUSTC);
        script(&root.join("tools/git"), "if [ \"$FUZZ_CLI_TEST_MODE\" = metadata-fail ]; then echo fixture-metadata-failed >&2; exit 51; fi\nprintf '0123456789abcdef\\n'\n");
        script(&root.join("tools/helper"), HELPER);
        script(&root.join("tools/target"), TARGET);
        script(
            &root.join("tools/descendants"),
            "sleep 60 &\nprintf '%s' \"$!\" > \"$FUZZ_CLI_TEST_ROOT/grandchild.pid\"\nwait\n",
        );
        script(&root.join("llvm/lib/rustlib/test-host/bin/llvm-cov"), LLVM);
        Self {
            directory,
            runner: process::Runner::new().unwrap(),
        }
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = cli_command(self.root(), args);
        let path = std::env::join_paths(
            std::iter::once(self.root().join("tools"))
                .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
        )
        .unwrap();
        command
            .env("REAL_CARGO", env!("CARGO"))
            .env("PATH", path)
            .env("FUZZ_CLI_TEST_MODE", "ok");
        command
    }

    fn run(&self, args: &[&str], mode: &str) -> Output {
        let mut command = self.command(args);
        command.env("FUZZ_CLI_TEST_MODE", mode);
        self.runner
            .run(command, Some(Duration::from_secs(20)), OutputMode::Capture)
            .unwrap()
    }

    fn seed(&self, target: &str, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self
            .root()
            .join("fuzz/seeds")
            .join(target)
            .join("regression")
            .join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn artifact(&self) -> PathBuf {
        let path = self.root().join("invocation/crash with spaces");
        std::fs::write(&path, b"original").unwrap();
        path
    }

    fn coverage_results(&self) -> Vec<PathBuf> {
        let history = self
            .root()
            .join("invocation/relative data/coverage-history");
        let mut paths: Vec<_> = std::fs::read_dir(history)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                !path
                    .file_name()
                    .unwrap()
                    .as_encoded_bytes()
                    .starts_with(b".")
            })
            .collect();
        paths.sort();
        paths
    }

    fn metadata(&self) -> Vec<serde_json::Value> {
        self.coverage_results()
            .iter()
            .map(|path| {
                serde_json::from_slice(&std::fs::read(path.join("metadata.json")).unwrap()).unwrap()
            })
            .collect()
    }

    fn triage_results(&self) -> Vec<PathBuf> {
        let mut results: Vec<_> = std::fs::read_dir(self.root().join("invocation"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("crash with spaces.triage-")
            })
            .collect();
        results.sort();
        results
    }

    fn assert_descendants_stopped(&self) {
        for name in ["child.pid", "grandchild.pid"] {
            let pid: i32 = std::fs::read_to_string(self.root().join(name))
                .unwrap()
                .parse()
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while live(pid) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!live(pid), "descendant {pid} ({name}) survived CLI exit");
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for name in ["child.pid", "grandchild.pid"] {
            if let Ok(text) = std::fs::read_to_string(self.root().join(name)) {
                if let Ok(pid) = text.parse::<i32>() {
                    if live(pid) {
                        // SAFETY: the fixture records only its own subprocess IDs.
                        unsafe {
                            libc::kill(pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
}

fn live(pid: i32) -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        if stat
            .rsplit_once(')')
            .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
        {
            return false;
        }
    }
    // SAFETY: signal zero only tests existence; it does not alter the process.
    unsafe { libc::kill(pid, 0) == 0 }
}

fn script(path: &Path, body: &str) {
    std::fs::write(path, format!("#!/bin/sh\nset -eu\n{body}")).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn triage_uses_cargo_paths_and_independently_checks_failure_identity() {
    let fixture = Fixture::new();
    fixture.artifact();
    let args = ["triage", "smelt_loop", "crash with spaces"];
    fixture
        .run(&args, "ok")
        .success("structured triage")
        .unwrap();
    let results = fixture.triage_results();
    assert_eq!(results.len(), 1);
    let minimized = results[0].join("minimized.json");
    assert_eq!(std::fs::read(&minimized).unwrap(), b"original");
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(results[0].join("metadata.json")).unwrap()).unwrap();
    assert_eq!(metadata["minimized"], "minimized.json");
    assert_eq!(std::fs::read_dir(&results[0]).unwrap().count(), 2);
    assert!(metadata["failure_fingerprint"]
        .as_str()
        .unwrap()
        .contains("example.rs:42:7"));
    let output = fixture.run(&args, "changed-identity");
    assert_eq!(output.termination.code(), 2);
    assert!(output.stderr.text().contains("changed failure identity"));
    assert_eq!(std::fs::read(&minimized).unwrap(), b"original");
    assert!(fixture
        .root()
        .join("invocation/relative build/smelt-fuzz/tools/test-host/debug/replay_scenario")
        .is_file());
}

#[test]
fn byte_triage_rejects_build_failures_and_stale_minimized_artifacts() {
    let fixture = Fixture::new();
    let artifact = fixture.artifact();
    let minimized = artifact.with_file_name("crash with spaces.min");
    std::fs::write(&minimized, b"stale").unwrap();
    let output = fixture.run(&["triage", "text_ops", "crash with spaces"], "build-fail");
    assert_eq!(output.termination.code(), 41);
    assert!(!fixture.root().join("replays").exists());
    let output = fixture.run(
        &["triage", "text_ops", "crash with spaces"],
        "missing-minimized",
    );
    assert_eq!(output.termination.code(), 2);
    assert!(output.stderr.text().contains("minimizer did not write"));
    assert_eq!(std::fs::read(&minimized).unwrap(), b"stale");
}

#[test]
fn replay_continues_after_failure_and_times_out_each_nested_seed() {
    let fixture = Fixture::new();
    fixture.seed("text_ops", "a-fail", b"fail");
    fixture.seed("text_ops", "nested/b-hang", b"hang");
    fixture.seed("text_ops", "nested/c-pass", b"pass");
    let output = fixture.run(&["replay-regression", "--timeout", "1", "text_ops"], "ok");
    assert_eq!(output.termination.code(), 77);
    assert!(output.stdout.text().contains("nested/c-pass"));
    assert!(output.stderr.text().contains("nested/b-hang: timed out"));
    let replays = std::fs::read_to_string(fixture.root().join("replays")).unwrap();
    assert!(
        replays.contains("a-fail")
            && replays.contains("nested/b-hang")
            && replays.contains("nested/c-pass")
    );
}

#[test]
fn coverage_timeout_kills_descendants_and_publishes_failure_metadata() {
    let fixture = Fixture::new();
    let stale = fixture.root().join("fuzz/coverage/text_ops");
    std::fs::create_dir_all(&stale).unwrap();
    std::fs::write(stale.join("coverage.profdata"), b"stale").unwrap();
    let unrelated = fixture.root().join("crash-unrelated");
    std::fs::write(&unrelated, b"other target").unwrap();
    let output = fixture.run(&["coverage-snapshot", "--timeout", "1", "text_ops"], "hang");
    assert_eq!(output.termination.code(), 124);
    fixture.assert_descendants_stopped();
    assert!(!stale.join("coverage.profdata").exists());
    assert_eq!(std::fs::read(unrelated).unwrap(), b"other target");
    let metadata = fixture.metadata();
    assert_eq!(metadata.len(), 1);
    assert_eq!(metadata[0]["targets"][0]["status"], "timeout");
}

#[test]
fn coverage_honors_cargo_config_and_keeps_same_second_reports_distinct() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.root().join(".cargo")).unwrap();
    std::fs::write(
        fixture.root().join(".cargo/config.toml"),
        "[build]\ntarget-dir = 'configured build'\ntarget = 'deliberately-not-the-host'\n",
    )
    .unwrap();
    for _ in 0..2 {
        let mut command = fixture.command(&["coverage-snapshot", "text_ops"]);
        command.env_remove("CARGO_TARGET_DIR");
        fixture
            .runner
            .run(command, Some(Duration::from_secs(20)), OutputMode::Capture)
            .unwrap()
            .success("coverage")
            .unwrap();
    }
    assert!(fixture
        .root()
        .join("configured build/smelt-fuzz/coverage/test-host/release/text_ops")
        .is_file());
    assert_eq!(fixture.metadata().len(), 2);
}

#[test]
fn coverage_renders_and_publishes_one_typed_result_per_target() {
    let fixture = Fixture::new();
    let output = fixture
        .run(&["coverage-snapshot", "text_ops", "text_ops"], "ok")
        .success("coverage report")
        .unwrap();
    let results = fixture.coverage_results();
    assert_eq!(results.len(), 1);
    let result = &results[0];
    assert_eq!(std::fs::read_dir(result).unwrap().count(), 3);
    let metadata = fixture.metadata();
    assert_eq!(metadata[0]["schema"], 4);
    assert_eq!(metadata[0]["targets"].as_array().unwrap().len(), 1);
    let record = &metadata[0]["targets"][0];
    assert_eq!(record["corpus_files"], 1);
    assert_eq!(record["totals"]["lines"]["covered"], 95);
    assert_eq!(record["totals"]["instantiations"]["count"], 10);
    assert_eq!(record["log"], "text_ops.log");
    let summary = std::fs::read_to_string(result.join("summary.txt")).unwrap();
    let expected = "text_ops (1f): lines 95.00%, functions 80.00%, regions 50.00%, branches 0.00%";
    assert!(summary.contains(expected), "{summary}");
    assert!(output.stdout.text().contains(expected));
    let commands = std::fs::read_to_string(fixture.root().join("llvm-commands")).unwrap();
    assert_eq!(commands.lines().count(), 1);
    assert!(commands.starts_with("export --summary-only "));
    let status = fixture
        .run(&["status"], "ok")
        .success("coverage status")
        .unwrap();
    assert!(status.stdout.text().contains(expected));
}

#[test]
fn coverage_status_reads_nested_imported_history() {
    let fixture = Fixture::new();
    let legacy = fixture.root().join("legacy");
    let nested = legacy.join("coverage-history/archived");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(
        nested.join("snapshot.txt"),
        "# fuzz coverage snapshot\ndate: fixture\ncommit: fixture\nbranch: fixture\n\ntext_ops: imported coverage\n",
    )
    .unwrap();
    fixture
        .run(&["import-data", legacy.to_str().unwrap()], "ok")
        .success("import nested history")
        .unwrap();
    let staging = fixture
        .root()
        .join("invocation/relative data/coverage-history/.pending/result");
    std::fs::create_dir_all(&staging).unwrap();
    std::os::unix::fs::symlink(staging.join("not-ready"), staging.join("summary.txt")).unwrap();
    let output = fixture
        .run(&["status"], "ok")
        .success("read imported history")
        .unwrap();
    assert!(output.stdout.text().contains("text_ops: imported coverage"));
}

#[test]
fn coverage_status_rejects_symlinked_reports() {
    for relative in [
        "coverage-history",
        "coverage-history/completed",
        "coverage-history/completed/summary.txt",
    ] {
        let fixture = Fixture::new();
        let outside = fixture.root().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let summary = outside.join("summary.txt");
        std::fs::write(
            &summary,
            "header\ndate\ncommit\nbranch\n\nunrelated report\n",
        )
        .unwrap();
        let link = fixture
            .root()
            .join("invocation/relative data")
            .join(relative);
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(
            if relative.ends_with(".txt") {
                &summary
            } else {
                &outside
            },
            &link,
        )
        .unwrap();
        let output = fixture.run(&["status"], "ok");
        assert_eq!(
            output.termination.code(),
            2,
            "accepted {relative}: {}",
            output.stdout.text()
        );
        assert!(!output.stdout.text().contains("unrelated report"));
    }
}

#[test]
fn coverage_records_invalid_llvm_output_as_a_failure() {
    let fixture = Fixture::new();
    let output = fixture.run(&["coverage-snapshot", "text_ops"], "llvm-invalid");
    assert_eq!(output.termination.code(), 2);
    let metadata = fixture.metadata();
    let record = &metadata[0]["targets"][0];
    assert_eq!(record["status"], "failed");
    assert!(record["totals"].is_null());
    assert_eq!(record["corpus_files"], 1);
    assert!(record["error"]
        .as_str()
        .unwrap()
        .contains("parse llvm-cov export"));
}

#[test]
fn signal_cancellation_cleans_up_the_tree_and_preserves_exit_code() {
    let fixture = Fixture::new();
    let mut command = fixture.command(&["coverage-snapshot", "text_ops"]);
    command.env("FUZZ_CLI_TEST_MODE", "hang");
    let ready = fixture.root().join("grandchild.pid");
    let worker = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(ready.exists(), "coverage did not start");
        let history = ready
            .parent()
            .unwrap()
            .join("invocation/relative data/coverage-history");
        let entries: Vec<_> = std::fs::read_dir(history).unwrap().collect();
        assert_eq!(entries.len(), 1);
        assert!(
            entries[0]
                .as_ref()
                .unwrap()
                .file_name()
                .as_encoded_bytes()
                .starts_with(b"."),
            "in-progress coverage must not be published"
        );
        let pid: i32 = std::fs::read_to_string(ready.with_file_name("cli.pid"))
            .unwrap()
            .parse()
            .unwrap();
        // SAFETY: signal the isolated CLI fixture, never the test runner.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    });
    let output = fixture
        .runner
        .run(command, Some(Duration::from_secs(15)), OutputMode::Capture)
        .unwrap();
    worker.join().unwrap();
    assert_eq!(output.termination.code(), 143);
    fixture.assert_descendants_stopped();
    assert_eq!(fixture.metadata()[0]["targets"][0]["status"], "interrupted");
}

#[test]
fn successful_launcher_cannot_leave_descendants_holding_the_pipes() {
    let fixture = Fixture::new();
    let output = fixture.run(
        &["coverage-snapshot", "--timeout", "10", "text_ops"],
        "parent-exits",
    );
    assert_eq!(output.termination.code(), 2);
    assert!(output
        .stderr
        .text()
        .contains("did not produce a binary and fresh profile"));
    fixture.assert_descendants_stopped();
}

#[test]
fn run_stops_at_preflight_failure_and_preserves_the_direct_fuzzer_exit() {
    let fixture = Fixture::new();
    let args = [
        "run",
        "text_ops",
        "--sanitizer",
        "none",
        "-max_total_time=1",
    ];
    let output = fixture.run(&args, "preflight-fail");
    assert_eq!(output.termination.code(), 77);
    assert_eq!(
        std::fs::read_to_string(fixture.root().join("replays")).unwrap(),
        "preflight\n"
    );
    let output = fixture.run(&args, "ok");
    assert_eq!(output.termination.code(), 43);
    assert_eq!(
        std::fs::read_to_string(fixture.root().join("replays")).unwrap(),
        "preflight\npreflight\nfuzz\n"
    );
}

#[test]
fn cmin_preserves_inputs_on_failure_and_concurrent_corpus_updates() {
    let fixture = Fixture::new();
    let corpus = fixture
        .root()
        .join("invocation/relative data/corpus/text_ops");
    std::fs::create_dir_all(&corpus).unwrap();
    std::fs::write(corpus.join("a"), b"seed").unwrap();
    std::fs::write(corpus.join("b"), b"redundant").unwrap();
    let args = [
        "run",
        "text_ops",
        "--cmin",
        "--sanitizer",
        "none",
        "-max_total_time=1",
    ];
    let output = fixture.run(&args, "merge-fail");
    assert_eq!(output.termination.code(), 64, "{}", output.stderr.text());
    assert_eq!(std::fs::read(corpus.join("a")).unwrap(), b"seed");
    assert_eq!(std::fs::read(corpus.join("b")).unwrap(), b"redundant");
    assert!(!std::fs::read_to_string(fixture.root().join("replays"))
        .unwrap()
        .contains("fuzz"));

    let output = fixture.run(&args, "merge-updates");
    assert_eq!(output.termination.code(), 43, "{}", output.stderr.text());
    assert!(!corpus.join("a").exists());
    assert_eq!(std::fs::read(corpus.join("selected")).unwrap(), b"seed");
    assert_eq!(std::fs::read(corpus.join("b")).unwrap(), b"changed");
    assert_eq!(std::fs::read(corpus.join("new")).unwrap(), b"new seed");
    assert!(
        !std::fs::read_dir(corpus.parent().unwrap().parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".cmin-"))
    );
}

#[test]
fn cmin_rejects_competing_minimizers_empty_results_and_publication_conflicts() {
    let fixture = Fixture::new();
    let corpus = fixture
        .root()
        .join("invocation/relative data/corpus/text_ops");
    std::fs::create_dir_all(&corpus).unwrap();
    std::fs::write(corpus.join("a"), b"seed").unwrap();
    std::fs::write(corpus.join("selected"), b"other input").unwrap();
    let args = ["run", "text_ops", "--cmin", "--sanitizer", "none"];
    let lock = std::fs::File::create(
        corpus
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .join(".text_ops.cmin.lock"),
    )
    .unwrap();
    lock.try_lock().unwrap();
    let output = fixture.run(&args, "ok");
    assert_eq!(output.termination.code(), 2);
    assert!(output
        .stderr
        .text()
        .contains("minimization already running"));
    assert!(!fixture.root().join("replays").exists());
    drop(lock);
    for mode in ["merge-empty", "ok"] {
        let output = fixture.run(&args, mode);
        assert_eq!(output.termination.code(), 2, "{}", output.stderr.text());
        assert_eq!(std::fs::read(corpus.join("a")).unwrap(), b"seed");
        assert_eq!(
            std::fs::read(corpus.join("selected")).unwrap(),
            b"other input"
        );
    }
}

#[test]
fn import_is_idempotent_and_rejects_conflicts_and_overlap() {
    let fixture = Fixture::new();
    let source = fixture.root().join("legacy/corpus/text_ops");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("seed"), b"seed").unwrap();
    let args = ["import-data", "../legacy"];
    fixture.run(&args, "ok").success("first import").unwrap();
    let output = fixture.run(&args, "ok").success("repeat import").unwrap();
    assert!(output.stdout.text().contains("already present: 1"));
    std::fs::write(source.join("seed"), b"different").unwrap();
    assert_eq!(fixture.run(&args, "ok").termination.code(), 2);
    let destination = fixture
        .root()
        .join("invocation/relative data/corpus/text_ops/seed");
    assert_eq!(std::fs::read(destination).unwrap(), b"seed");
    assert_eq!(
        fixture
            .run(&["import-data", "relative data"], "ok")
            .termination
            .code(),
        2
    );
}

#[test]
fn run_rejects_worker_counts_that_overflow_libfuzzer() {
    let fixture = Fixture::new();
    let output = fixture.run(&["run", "text_ops", "--fork", "4294967295"], "ok");
    assert_eq!(output.termination.code(), 2);
    assert!(!fixture.root().join("replays").exists());
}

#[test]
fn import_rejects_symlinked_destination_directories() {
    let fixture = Fixture::new();
    let source = fixture.root().join("legacy/corpus/text_ops/nested");
    let destination = fixture.root().join("invocation/relative data/corpus");
    let outside = fixture.root().join("outside");
    for path in [&source, &destination, &outside] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(source.join("seed"), b"seed").unwrap();
    std::os::unix::fs::symlink(&outside, destination.join("text_ops")).unwrap();
    let output = fixture.run(&["import-data", "../legacy"], "ok");
    assert_eq!(output.termination.code(), 2, "{}", output.stderr.text());
    assert!(!outside.join("nested/seed").exists());
}

#[test]
fn default_build_path_and_missing_llvm_are_reported_correctly() {
    let fixture = Fixture::new();
    let mut command = fixture.command(&["build", "text_ops"]);
    command.env_remove("CARGO_TARGET_DIR");
    fixture
        .runner
        .run(command, Some(Duration::from_secs(20)), OutputMode::Capture)
        .unwrap()
        .success("default build")
        .unwrap();
    assert!(fixture
        .root()
        .join("fuzz/target/smelt-fuzz/none/test-host/release/text_ops")
        .is_file());
    std::fs::remove_file(
        fixture
            .root()
            .join("llvm/lib/rustlib/test-host/bin/llvm-cov"),
    )
    .unwrap();
    let output = fixture.run(&["coverage-snapshot", "text_ops"], "ok");
    assert_eq!(output.termination.code(), 2);
    assert!(output
        .stderr
        .text()
        .contains("rustup component add llvm-tools-preview --toolchain nightly"));
}

#[test]
fn build_timeout_override_bounds_target_and_helper_builds() {
    for (args, mode) in [
        (
            vec!["build", "text_ops", "--build-timeout", "1"],
            "slow-target-build",
        ),
        (vec!["build", "--build-timeout=1"], "slow-target-build"),
        (
            vec!["replay-regression", "smelt_loop", "--build-timeout", "1"],
            "slow-helper-build",
        ),
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&args, mode);
        assert_eq!(output.termination.code(), 124, "{}", output.stderr.text());
        assert!(output.stderr.text().contains("1s build watchdog"));
        assert!(output.stderr.text().contains("--build-timeout"));
    }
    let fixture = Fixture::new();
    fixture
        .run(
            &["--build-timeout", "10", "build", "text_ops"],
            "slow-target-build",
        )
        .success("build with extended watchdog")
        .unwrap();
    fixture
        .run(&["build", "text_ops"], "slow-target-build")
        .success("build with default watchdog")
        .unwrap();
}

#[test]
fn invalid_build_timeouts_fail_before_tool_execution() {
    let fixture = Fixture::new();
    for args in [
        vec!["build", "--build-timeout"],
        vec!["build", "--build-timeout=0"],
        vec!["build", "--build-timeout", "invalid"],
        vec!["build", "--build-timeout", "4294967296"],
        vec!["--build-timeout=0", "build", "--build-timeout=2"],
    ] {
        let mut command = fixture.command(&args);
        command.env("PATH", fixture.root().join("no-tools"));
        let output = fixture
            .runner
            .run(command, Some(Duration::from_secs(20)), OutputMode::Capture)
            .unwrap();
        assert_eq!(output.termination.code(), 2, "{}", output.stderr.text());
        assert!(output.stderr.text().contains("--build-timeout"));
        assert!(!output.stderr.text().contains("spawn cargo"));
        assert!(!fixture.root().join("invocation/relative build").exists());
    }
}

#[test]
fn help_uses_the_full_cargo_invocation_without_loading_the_project() {
    let fixture = Fixture::new();
    for (args, usage) in [
        (
            vec!["--help"],
            "Usage: cargo xtask fuzz [OPTIONS] <COMMAND>",
        ),
        (vec!["run", "--help"], "Usage: cargo xtask fuzz run"),
        (vec!["help", "triage"], "Usage: cargo xtask fuzz triage"),
    ] {
        let mut command = fixture.command(&args);
        command.env("PATH", fixture.root().join("no-tools"));
        let output = fixture
            .runner
            .run(command, Some(Duration::from_secs(20)), OutputMode::Capture)
            .unwrap()
            .success("show fuzz help without tools")
            .unwrap();
        assert!(
            output.stdout.text().contains(usage),
            "{}",
            output.stdout.text()
        );
    }
}

#[test]
fn all_commands_validate_arguments_before_loading_cargo_metadata() {
    let fixture = Fixture::new();
    for args in [
        vec!["build", "unknown"],
        vec!["prepare", "unexpected"],
        vec!["status", "unexpected"],
        vec!["run", "text_ops", "--fork=0"],
        vec!["run", "text_ops", "-error_exitcode=0"],
        vec!["triage", "text_ops", "missing-artifact", "--timeout=0"],
        vec!["replay-regression", "text_ops", "--timeout=0"],
        vec!["verify", "unknown"],
        vec!["coverage-snapshot", "text_ops", "--timeout=0"],
    ] {
        let mut command = fixture.command(&args);
        command.env("PATH", fixture.root().join("no-tools"));
        let output = fixture
            .runner
            .run(command, Some(Duration::from_secs(20)), OutputMode::Capture)
            .unwrap();
        assert_eq!(output.termination.code(), 2, "{}", output.stderr.text());
        assert!(
            !output.stderr.text().contains("spawn cargo"),
            "{}",
            output.stderr.text()
        );
        assert!(!fixture.root().join("invocation/relative build").exists());
    }
}

#[test]
fn build_timeout_does_not_replace_replay_or_coverage_deadlines() {
    let fixture = Fixture::new();
    fixture.seed("text_ops", "hang", b"hang");
    let output = fixture.run(
        &[
            "replay-regression",
            "text_ops",
            "--build-timeout",
            "10",
            "--timeout",
            "1",
        ],
        "ok",
    );
    assert_eq!(output.termination.code(), 124, "{}", output.stderr.text());
    assert!(output.stderr.text().contains("hang: timed out"));
    fixture
        .run(
            &[
                "coverage-snapshot",
                "text_ops",
                "--build-timeout",
                "1",
                "--timeout",
                "10",
            ],
            "slow-coverage",
        )
        .success("coverage has its own combined deadline")
        .unwrap();
}

#[test]
fn byte_triage_bounds_the_entire_shrink_loop_and_rejects_nonshrinking_outputs() {
    let fixture = Fixture::new();
    let artifact = fixture.artifact();
    let args = ["triage", "text_ops", "crash with spaces", "--timeout", "3"];
    let output = fixture.run(&args, "slow-minimize");
    assert_eq!(output.termination.code(), 124, "{}", output.stderr.text());
    assert!(output
        .stderr
        .text()
        .contains("minimize byte artifact: timed out"));
    let output = fixture.run(&args, "non-shrinking");
    assert_eq!(output.termination.code(), 2, "{}", output.stderr.text());
    assert!(output.stderr.text().contains("minimizer did not shrink"));
    assert!(fixture.triage_results().is_empty());
    assert_eq!(std::fs::read(artifact).unwrap(), b"original");
}

#[test]
fn cancellation_is_preserved_during_filesystem_only_import() {
    let fixture = Fixture::new();
    let source = fixture.root().join("legacy/corpus/text_ops");
    std::fs::create_dir_all(&source).unwrap();
    for index in 0..10000 {
        std::fs::write(source.join(format!("{index:05}")), b"seed").unwrap();
    }
    let root = fixture.root().to_path_buf();
    let worker = std::thread::spawn(move || {
        let ready = root.join("invocation/relative data/corpus/text_ops/00000");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(ready.exists(), "import did not start");
        let pid: i32 = std::fs::read_to_string(root.join("cli.pid"))
            .unwrap()
            .parse()
            .unwrap();
        // SAFETY: this is the isolated CLI fixture's PID.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGTERM) }, 0);
    });
    let output = fixture.run(&["import-data", "../legacy"], "ok");
    worker.join().unwrap();
    assert_eq!(output.termination.code(), 143, "{}", output.stderr.text());
}

#[test]
fn campaign_rejects_success_exit_codes_for_failures() {
    let fixture = Fixture::new();
    for flag in [
        "-error_exitcode=0",
        "-timeout_exitcode=0",
        "-ignore_remaining_args=1",
        "-set_cover_merge=1",
        "-close_fd_mask=3",
        "-future_unknown_flag=1",
    ] {
        let output = fixture.run(&["run", "text_ops", "--sanitizer", "none", flag], "ok");
        assert_eq!(output.termination.code(), 2, "accepted {flag}");
    }
}

#[test]
fn coverage_preserves_results_when_a_later_corpus_is_invalid() {
    let fixture = Fixture::new();
    let corpus = fixture
        .root()
        .join("invocation/relative data/corpus/text_ops");
    std::fs::create_dir_all(&corpus).unwrap();
    std::os::unix::fs::symlink("missing", corpus.join("invalid-seed")).unwrap();
    let output = fixture.run(&["coverage-snapshot", "ansi_parser", "text_ops"], "ok");
    assert_eq!(output.termination.code(), 2);
    let metadata = fixture.metadata();
    assert_eq!(metadata.len(), 1, "successful target report was lost");
    assert_eq!(metadata[0]["targets"][0]["status"], "ok");
    assert_eq!(metadata[0]["targets"][1]["status"], "failed");
    assert!(metadata[0]["targets"][1]["corpus_files"].is_null());
    assert!(metadata[0]["targets"][1]["corpus_digest"].is_null());
    let log = metadata[0]["targets"][1]["log"].as_str().unwrap();
    assert!(
        std::fs::read_to_string(fixture.coverage_results()[0].join(log))
            .unwrap()
            .contains("invalid-seed")
    );
}

#[test]
fn coverage_logs_llvm_failure_diagnostics() {
    let fixture = Fixture::new();
    let output = fixture.run(&["coverage-snapshot", "text_ops"], "llvm-fail");
    assert_eq!(output.termination.code(), 52);
    let metadata = fixture.metadata();
    let log = metadata[0]["targets"][0]["log"].as_str().unwrap();
    assert!(
        std::fs::read_to_string(fixture.coverage_results()[0].join(log))
            .unwrap()
            .contains("fixture-llvm-export-failed")
    );
}

#[test]
fn triage_does_not_publish_before_metadata_succeeds() {
    let fixture = Fixture::new();
    let artifact = fixture.artifact();
    let minimized = artifact.with_file_name("crash with spaces.min.json");
    std::fs::write(&minimized, b"previous result").unwrap();
    let output = fixture.run(
        &["triage", "smelt_loop", "crash with spaces"],
        "metadata-fail",
    );
    assert_eq!(output.termination.code(), 51);
    assert_eq!(std::fs::read(minimized).unwrap(), b"previous result");
    assert!(fixture.triage_results().is_empty());
    assert!(!std::fs::read_dir(artifact.parent().unwrap())
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".triage-")));
}

const RUSTC: &str = r#"
case "$*" in
  *sysroot*) printf '%s\n' "$FUZZ_CLI_TEST_ROOT/llvm" ;;
  *) printf 'host: test-host\n' ;;
esac
"#;

const CARGO: &str = r#"
if [ "$1" = metadata ]; then exec "$REAL_CARGO" "$@"; fi
mode=; directory=; host=; target=; helpers=
while [ "$#" -gt 0 ]; do
  case "$1" in
    build|coverage|cmin) mode=$1 ;;
    --target-dir) shift; directory=$1 ;;
    --target) shift; host=$1 ;;
    --bin) shift; helpers="$helpers $1" ;;
    text_ops|smelt_loop|ansi_parser) target=$1 ;;
  esac
  shift
done
if [ "$FUZZ_CLI_TEST_MODE" = build-fail ]; then echo 'ERROR: build failed' >&2; exit 41; fi
if [ "$FUZZ_CLI_TEST_MODE" = slow-target-build ] && [ "$mode" = build ] && [ -z "$helpers" ]; then sleep 2; fi
if [ "$FUZZ_CLI_TEST_MODE" = slow-helper-build ] && [ -n "$helpers" ]; then sleep 2; fi
if [ "$FUZZ_CLI_TEST_MODE" = slow-coverage ] && [ "$mode" = coverage ]; then sleep 2; fi
if [ "$mode" = coverage ]; then
  case "$FUZZ_CLI_TEST_MODE" in
    hang|parent-exits)
      sh "$FUZZ_CLI_TEST_ROOT/tools/descendants" &
      printf '%s' "$!" > "$FUZZ_CLI_TEST_ROOT/child.pid"
      while [ ! -f "$FUZZ_CLI_TEST_ROOT/grandchild.pid" ]; do sleep 0.01; done
      if [ "$FUZZ_CLI_TEST_MODE" = parent-exits ]; then exit 0; fi
      wait
      exit 0 ;;
  esac
fi
if [ -n "$helpers" ]; then
  directory="$directory/$host/debug"
  mkdir -p "$directory"
  for name in $helpers; do cp "$FUZZ_CLI_TEST_ROOT/tools/helper" "$directory/$name"; done
else
  directory="$directory/$host/release"
  mkdir -p "$directory"
  cp "$FUZZ_CLI_TEST_ROOT/tools/target" "$directory/$target"
fi
if [ "$mode" = coverage ]; then
  mkdir -p "$FUZZ_CLI_TEST_ROOT/fuzz/coverage/$target"
  printf fresh > "$FUZZ_CLI_TEST_ROOT/fuzz/coverage/$target/coverage.profdata"
fi
"#;

const HELPER: &str = r#"
case "${0##*/}" in
  crash_to_scenario) cp "$3" "$4" ;;
  shrink_scenario)
    if [ "$FUZZ_CLI_TEST_MODE" = changed-identity ]; then printf changed > "$4"; else cp "$3" "$4"; fi ;;
  replay_scenario)
    line=42
    if [ "$(cat "$3")" = changed ]; then line=43; fi
    printf "thread 'main' panicked at example.rs:%s:7:\nvalue 42 failed\n" "$line" >&2
    exit 77 ;;
esac
"#;

const TARGET: &str = r#"
minimize=; candidate=; last=; previous=; merge=; preflight=; fork=; error_exitcode=43
for arg in "$@"; do
  case "$arg" in
    -minimize_crash_internal_step=1) minimize=yes ;;
    -merge=1) merge=yes ;;
    -exact_artifact_path=*) candidate=${arg#*=} ;;
    -runs=0) preflight=yes ;;
    -fork=*) fork=yes ;;
    -error_exitcode=*|-timeout_exitcode=*) error_exitcode=${arg#*=} ;;
    -*) ;;
    *) previous=$last; last=$arg ;;
  esac
done
if [ -n "$preflight" ]; then
  printf 'preflight\n' >> "$FUZZ_CLI_TEST_ROOT/replays"
  if [ "$FUZZ_CLI_TEST_MODE" = preflight-fail ]; then exit 77; fi
  exit 0
fi
if [ -n "$merge" ]; then
  printf 'merge\n' >> "$FUZZ_CLI_TEST_ROOT/replays"
  if [ "$FUZZ_CLI_TEST_MODE" = merge-fail ]; then exit 64; fi
  if [ "$FUZZ_CLI_TEST_MODE" = merge-empty ]; then exit 0; fi
  cp "$last/00000000" "$previous/selected"
  if [ "$FUZZ_CLI_TEST_MODE" = merge-updates ]; then
    live="$FUZZ_CLI_TEST_ROOT/invocation/relative data/corpus/text_ops"
    printf changed > "$live/b"
    printf 'new seed' > "$live/new"
  fi
  exit 0
fi
if [ -n "$fork" ]; then printf 'fuzz\n' >> "$FUZZ_CLI_TEST_ROOT/replays"; exit "$error_exitcode"; fi
if [ -n "$minimize" ]; then
  case "$FUZZ_CLI_TEST_MODE" in
    missing-minimized) exit 77 ;;
    non-shrinking) cp "$last" "$candidate"; exit 77 ;;
    slow-minimize)
      sleep 2
      size=$(wc -c < "$last")
      if [ "$size" -gt 1 ]; then
        dd if="$last" of="$candidate" bs=1 count=$((size - 1)) 2>/dev/null
        exit 77
      fi ;;
  esac
  exit 0
fi
printf '%s\n' "$last" >> "$FUZZ_CLI_TEST_ROOT/replays"
case "$(cat "$last")" in
  pass) exit 0 ;;
  hang) exec sleep 60 ;;
  *) printf "thread 'main' panicked at example.rs:42:7:\nvalue 42 failed\n" >&2; exit 77 ;;
esac
"#;

const LLVM: &str = r#"
printf '%s\n' "$*" >> "$FUZZ_CLI_TEST_ROOT/llvm-commands"
if [ "$1" != export ] || [ "$2" != --summary-only ]; then echo unexpected-llvm-command >&2; exit 53; fi
if [ "$FUZZ_CLI_TEST_MODE" = llvm-fail ]; then echo fixture-llvm-export-failed >&2; exit 52; fi
if [ "$FUZZ_CLI_TEST_MODE" = llvm-invalid ]; then printf '{"data":[{"totals":{}}]}\n'; exit 0; fi
printf '{"data":[{"totals":{"lines":{"count":100,"covered":95,"percent":95.0},"functions":{"count":10,"covered":8,"percent":80.0},"regions":{"count":50,"covered":25,"percent":50.0},"branches":{"count":0,"covered":0,"percent":0.0},"instantiations":{"count":10,"covered":8,"percent":80.0}}}]}\n'
"#;
