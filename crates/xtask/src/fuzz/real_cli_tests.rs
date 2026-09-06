//! Opt-in integration lane using real Cargo, cargo-fuzz, ASan and libFuzzer.
//! The fixture is a tiny isolated workspace, never a production fuzz target.

use super::cli_tests::cli_command;
use super::process::{Output, OutputMode, Runner};
use super::TARGETS;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const MANIFEST: &str = include_str!("../../tests/fixtures/fuzz-triage/Cargo.toml");
const LOCKFILE: &str = include_str!("../../tests/fixtures/fuzz-triage/Cargo.lock");
const TARGET: &str = include_str!("../../tests/fixtures/fuzz-triage/src/main.rs");
const FIXTURE_DIRECTORY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/fuzz-triage");
const PRIMARY: &str = "triage fixture primary failure";
const SECONDARY: &str = "triage fixture secondary failure";

fn runtime_source(directory: &Path, manifest: &str) -> PathBuf {
    let manifest: toml_edit::DocumentMut = manifest.parse().unwrap();
    directory
        .join(
            manifest["dependencies"]["libfuzzer-sys"]["path"]
                .as_str()
                .unwrap(),
        )
        .canonicalize()
        .unwrap()
}

#[test]
fn fixture_uses_production_fuzz_runtime() {
    let production = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz");
    let runtime = runtime_source(Path::new(FIXTURE_DIRECTORY), MANIFEST);
    assert_eq!(
        runtime,
        runtime_source(&production, include_str!("../../../../fuzz/Cargo.toml")),
        "the fixture and production must use the same runtime source"
    );
    let manifest: toml_edit::DocumentMut = std::fs::read_to_string(runtime.join("Cargo.toml"))
        .unwrap()
        .parse()
        .unwrap();
    for lockfile in [LOCKFILE, include_str!("../../../../fuzz/Cargo.lock")] {
        let lockfile: toml_edit::DocumentMut = lockfile.parse().unwrap();
        let packages: Vec<_> = lockfile["package"]
            .as_array_of_tables()
            .unwrap()
            .iter()
            .filter(|package| package["name"].as_str() == Some("libfuzzer-sys"))
            .collect();
        assert_eq!(packages.len(), 1, "one resolved runtime per workspace");
        assert!(
            !packages[0].contains_key("source"),
            "the runtime must resolve locally"
        );
        assert_eq!(
            packages[0]["version"].as_str(),
            manifest["package"]["version"].as_str()
        );
    }
}

struct Fixture {
    directory: tempfile::TempDir,
    runner: Runner,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::Builder::new()
            .prefix("smelt fuzz 'quoted';$literal ")
            .tempdir()
            .unwrap();
        let root = directory.path();
        std::fs::create_dir_all(root.join("fuzz/fuzz_targets")).unwrap();
        std::fs::create_dir(root.join("invocation")).unwrap();
        std::fs::create_dir(root.join("scratch")).unwrap();
        let mut manifest: toml_edit::DocumentMut = MANIFEST.parse().unwrap();
        manifest["dependencies"]["libfuzzer-sys"]["path"] = toml_edit::value(
            runtime_source(Path::new(FIXTURE_DIRECTORY), MANIFEST)
                .to_str()
                .unwrap(),
        );
        let mut manifest = manifest.to_string();
        for target in TARGETS {
            manifest.push_str(&format!(
                "\n[[bin]]\nname = '{}'\npath = 'fuzz_targets/{}.rs'\ntest = false\ndoc = false\nbench = false\n",
                target.name, target.name
            ));
            std::fs::write(
                root.join(format!("fuzz/fuzz_targets/{}.rs", target.name)),
                TARGET,
            )
            .unwrap();
        }
        std::fs::write(root.join("fuzz/Cargo.toml"), manifest).unwrap();
        std::fs::write(root.join("fuzz/Cargo.lock"), LOCKFILE).unwrap();
        let fixture = Self {
            directory,
            runner: Runner::new().unwrap(),
        };
        fixture
            .git(&["init", "--initial-branch=main"])
            .success("initialize fixture repository")
            .unwrap();
        fixture
            .git(&[
                "-c",
                "user.name=fuzz fixture",
                "-c",
                "user.email=fuzz-fixture@invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--allow-empty",
                "-m",
                "fixture",
            ])
            .success("create fixture commit")
            .unwrap();
        fixture
    }

    fn root(&self) -> &Path {
        self.directory.path()
    }

    fn execute(&self, mut command: Command) -> Output {
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("RUSTFLAGS")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env_remove("RUSTC_WRAPPER")
            .env_remove("RUSTC_WORKSPACE_WRAPPER")
            .env_remove("ASAN_OPTIONS")
            .env_remove("CUSTOM_LIBFUZZER_PATH")
            .env_remove("CUSTOM_LIBFUZZER_STD_CXX")
            .env("TMPDIR", self.root().join("scratch"))
            .env("RUST_BACKTRACE", "0")
            .env("CARGO_TERM_COLOR", "never");
        self.runner
            .run(command, Some(Duration::from_secs(600)), OutputMode::Capture)
            .unwrap()
    }

    fn git(&self, args: &[&str]) -> Output {
        let mut command = Command::new("git");
        command.args(args).current_dir(self.root());
        self.execute(command)
    }

    fn run(&self, args: &[&str], mode: &str) -> Output {
        let mut command = cli_command(self.root(), args);
        command.env("FUZZ_TRIAGE_FIXTURE_MODE", mode);
        self.execute(command)
    }

    fn triage(&self, artifact: &Path, mode: &str) -> Output {
        self.run(
            &[
                "triage",
                "text_ops",
                artifact.file_name().unwrap().to_str().unwrap(),
                "--build-timeout",
                "300",
                "--timeout",
                "30",
            ],
            mode,
        )
    }

    fn artifact(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.root().join("invocation").join(name);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn assert_primary_crash(&self, output: &Output) {
        assert_eq!(output.termination.code(), 77, "{}", output.stderr.text());
        assert!(
            output.stderr.text().contains(PRIMARY),
            "{}",
            output.stderr.text()
        );
        let artifacts = super::input_files(
            &self
                .root()
                .join("invocation/relative data/artifacts/text_ops"),
        )
        .unwrap();
        assert!(
            !artifacts.is_empty(),
            "the worker must publish a crash artifact"
        );
        for artifact in artifacts {
            assert_eq!(std::fs::read(artifact).unwrap(), b"A");
        }
    }

    fn results(&self, artifact: &Path) -> Vec<PathBuf> {
        let prefix = format!(
            "{}.triage-",
            artifact.file_name().unwrap().to_str().unwrap()
        );
        std::fs::read_dir(artifact.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&prefix)
            })
            .collect()
    }

    fn assert_result(&self, artifact: &Path) -> (PathBuf, serde_json::Value) {
        let results = self.results(artifact);
        assert_eq!(results.len(), 1, "one complete result must be published");
        let result = &results[0];
        assert_eq!(std::fs::read_dir(result).unwrap().count(), 2);
        let minimized = result.join("minimized");
        assert_eq!(std::fs::read(&minimized).unwrap(), b"A");
        let metadata: serde_json::Value =
            serde_json::from_slice(&std::fs::read(result.join("metadata.json")).unwrap()).unwrap();
        assert_eq!(metadata["schema"], 3);
        assert_eq!(metadata["target"], "text_ops");
        assert_eq!(metadata["input_kind"], "bytes");
        assert_eq!(metadata["sanitizer"], "address");
        assert_eq!(
            metadata["original"],
            artifact.canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(metadata["minimized"], "minimized");
        assert_eq!(
            metadata["original_bytes"],
            std::fs::metadata(artifact).unwrap().len()
        );
        assert_eq!(metadata["minimized_bytes"], 1);
        let commit = self
            .git(&["rev-parse", "HEAD"])
            .success("query fixture commit")
            .unwrap();
        assert_eq!(metadata["commit"], commit.stdout.text().trim());
        assert!(metadata["failure_fingerprint"]
            .as_str()
            .unwrap()
            .contains(PRIMARY));
        (minimized, metadata)
    }
}

#[test]
#[ignore = "requires a C++17 compiler; run the dedicated fuzz tooling integration lane"]
fn real_command_shell_round_trip() {
    let fixture = Fixture::new();
    let source = fixture.root().join("command.cpp");
    let executable = fixture.root().join("command-probe");
    std::fs::write(
        &source,
        r#"
#include "FuzzerCommand.h"
#include <cstdlib>

int main(int argc, char **argv) {
  fuzzer::Command command({"/bin/sh", "-c",
      "printf '%s\\000' \"$@\"; printf stderr >&2", "probe"});
  for (int i = 2; i < argc; ++i)
    command.addArgument(argv[i]);
  command.setOutputFile(argv[1]);
  command.combineOutAndErr();
  return std::system(command.toString().c_str()) != 0;
}
"#,
    )
    .unwrap();
    let mut compile = Command::new("c++");
    compile
        .args(["-std=c++17", "-Wall", "-Wextra", "-Werror", "-Wundef", "-I"])
        .arg(runtime_source(Path::new(FIXTURE_DIRECTORY), MANIFEST).join("libfuzzer"))
        .arg(source)
        .arg("-o")
        .arg(&executable);
    fixture
        .execute(compile)
        .success("compile the libFuzzer command probe")
        .unwrap();

    let arguments = [
        "",
        "plain",
        "two words",
        "'single' and \"double\"",
        "\\back\\slash",
        "$HOME $(touch argument-injected) `touch backtick-injected`",
        ";\n\t&|<>*?[]{}~#",
        "UTF-8 λ漢字",
        "-value=a b",
    ];
    let output = fixture
        .root()
        .join("captured 'output';$(touch output-injected);$literal");
    let mut command = Command::new(executable);
    command
        .arg(&output)
        .args(arguments)
        .current_dir(fixture.root());
    fixture
        .execute(command)
        .success("round-trip literal shell arguments and redirection")
        .unwrap();
    let mut expected = arguments.join("\0").into_bytes();
    expected.extend_from_slice(b"\0stderr");
    assert_eq!(std::fs::read(output).unwrap(), expected);
    for name in ["argument-injected", "backtick-injected", "output-injected"] {
        assert!(
            !fixture.root().join(name).exists(),
            "shell expansion executed: {name}"
        );
    }
}

#[test]
#[ignore = "requires nightly and cargo-fuzz; run the dedicated fuzz tooling integration lane"]
fn real_fork_campaign() {
    let fixture = Fixture::new();
    let output = fixture.run(
        &[
            "run",
            "text_ops",
            "--fork",
            "2",
            "--sanitizer",
            "none",
            "--build-timeout",
            "300",
            "-max_total_time=2",
        ],
        "same",
    );
    fixture.assert_primary_crash(&output);
}

#[test]
#[ignore = "requires nightly and cargo-fuzz; run the dedicated fuzz tooling integration lane"]
fn real_corpus_merge() {
    let fixture = Fixture::new();
    let corpus = fixture
        .root()
        .join("invocation/relative data/corpus/text_ops");
    std::fs::create_dir_all(&corpus).unwrap();
    for name in ["first", "second", "third"] {
        std::fs::write(corpus.join(name), b"Cnoncrashing").unwrap();
    }
    let output = fixture.run(
        &[
            "run",
            "text_ops",
            "--cmin",
            "--fork",
            "1",
            "--sanitizer",
            "none",
            "--build-timeout",
            "300",
            "-max_total_time=2",
        ],
        "same",
    );
    fixture.assert_primary_crash(&output);
    assert!(
        output.stdout.text().contains(">>> fuzz text_ops"),
        "{}",
        output.stdout.text()
    );
    let selected = super::input_files(&corpus).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(std::fs::read(&selected[0]).unwrap(), b"Cnoncrashing");
}

#[test]
#[ignore = "requires nightly and cargo-fuzz; run the dedicated fuzz tooling integration lane"]
fn real_crash_triage() {
    let fixture = Fixture::new();
    let long = fixture.artifact("crash long", &[b'A'; 128]);
    fixture
        .triage(&long, "same")
        .success("triage a real libFuzzer crash")
        .unwrap();
    let (published, metadata) = fixture.assert_result(&long);
    assert_eq!(std::fs::read(&long).unwrap(), [b'A'; 128]);

    let minimal = fixture.artifact("crash 'minimal';$literal", b"A");
    fixture
        .triage(&minimal, "same")
        .success("triage an already-minimal real crash")
        .unwrap();
    let (_, minimal_metadata) = fixture.assert_result(&minimal);
    assert_eq!(
        metadata["failure_fingerprint"],
        minimal_metadata["failure_fingerprint"]
    );

    let changing = fixture.artifact("crash changing", &[b'A'; 128]);
    let output = fixture.triage(&changing, "changed");
    assert_eq!(output.termination.code(), 2, "{}", output.stderr.text());
    let diagnostic = output.stderr.text();
    assert!(
        diagnostic.contains("changed failure identity"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains(PRIMARY) && diagnostic.contains(SECONDARY),
        "{diagnostic}"
    );
    assert!(fixture.results(&changing).is_empty());
    assert_eq!(std::fs::read(&changing).unwrap(), [b'A'; 128]);
    assert_eq!(fixture.assert_result(&long).1, metadata);
    assert_eq!(fixture.assert_result(&minimal).1, minimal_metadata);
    assert!(!std::fs::read_dir(changing.parent().unwrap())
        .unwrap()
        .any(|entry| entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".triage-")));

    let seeds = fixture.root().join("fuzz/seeds/text_ops/regression");
    std::fs::create_dir_all(&seeds).unwrap();
    std::fs::copy(&published, seeds.join("published-crash")).unwrap();
    let args = [
        "replay-regression",
        "text_ops",
        "--build-timeout",
        "300",
        "--timeout",
        "30",
    ];
    let output = fixture.run(&args, "same");
    assert_eq!(output.termination.code(), 77, "{}", output.stderr.text());
    assert!(output.stderr.text().contains(PRIMARY));
    assert!(output.stderr.text().contains("published-crash"));
    let output = fixture
        .run(&args, "fixed")
        .success("replay the published seed after fixing the fixture")
        .unwrap();
    assert!(output
        .stdout
        .text()
        .contains("all 1 regression seeds passed"));
}

#[test]
#[ignore = "requires nightly, cargo-fuzz, and llvm-tools; run the dedicated fuzz tooling integration lane"]
fn real_coverage_snapshot() {
    let fixture = Fixture::new();
    fixture
        .run(
            &["coverage-snapshot", "text_ops", "--timeout", "180"],
            "fixed",
        )
        .success("collect real LLVM coverage")
        .unwrap();
    let history = fixture
        .root()
        .join("invocation/relative data/coverage-history");
    let results: Vec<_> = std::fs::read_dir(history)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(results.len(), 1, "only the published result remains");
    let result = &results[0];
    let metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(result.join("metadata.json")).unwrap()).unwrap();
    assert_eq!(metadata["schema"], 4);
    let record = &metadata["targets"][0];
    assert_eq!(record["status"], "ok");
    assert_eq!(record["corpus_files"], 1);
    assert!(record["totals"]["lines"]["percent"].is_number());
    let log = std::fs::read_to_string(result.join(record["log"].as_str().unwrap())).unwrap();
    assert!(log.contains("=== llvm-cov export ==="));
    assert!(!log.contains("=== llvm-cov report ==="));
    let summary = std::fs::read_to_string(result.join("summary.txt")).unwrap();
    assert!(summary.contains("text_ops (1f): lines"), "{summary}");
}
