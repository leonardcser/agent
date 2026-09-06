//! Cargo-aware paths shared by builds, replay, triage, and coverage.

use super::process::{self, OutputMode, Termination, QUERY_TIMEOUT};
use super::{metadata_target_is_fuzz_bin, FuzzData, TARGETS};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::Duration;

pub(super) struct Project {
    pub root: PathBuf,
    target_dir: PathBuf,
    host: OnceLock<String>,
    pub runner: process::Runner,
    build_timeout: Duration,
}

impl Project {
    pub fn load(root: PathBuf, build_timeout: Duration) -> Result<Self> {
        let runner = process::Runner::new()?;
        let mut command = Command::new("cargo");
        command
            .args([
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--manifest-path",
            ])
            .arg(root.join("fuzz/Cargo.toml"))
            .current_dir(&root);
        // Resolve a relative override before changing the child's working directory.
        if let Some(path) = std::env::var_os("CARGO_TARGET_DIR") {
            command.env("CARGO_TARGET_DIR", std::env::current_dir()?.join(path));
        }
        let output = runner
            .run(command, Some(QUERY_TIMEOUT), OutputMode::Capture)?
            .success("read fuzz Cargo metadata")?;
        let metadata: Value = serde_json::from_slice(&output.stdout.complete()?)
            .context("parse fuzz Cargo metadata")?;
        Self::from_metadata(root, &metadata, runner, build_timeout)
    }

    fn from_metadata(
        root: PathBuf,
        metadata: &Value,
        runner: process::Runner,
        build_timeout: Duration,
    ) -> Result<Self> {
        let package = metadata["packages"]
            .as_array()
            .and_then(|packages| {
                packages
                    .iter()
                    .find(|package| package["name"] == "smelt-fuzz")
            })
            .context("Cargo metadata missing smelt-fuzz package")?;
        let targets = package["targets"]
            .as_array()
            .context("Cargo metadata missing fuzz targets")?;
        let mut manifest = targets
            .iter()
            .filter(|target| metadata_target_is_fuzz_bin(target))
            .map(|target| target["name"].as_str().context("fuzz target missing name"))
            .collect::<Result<Vec<_>>>()?;
        manifest.sort_unstable();
        let mut registered: Vec<_> = TARGETS.iter().map(|target| target.name).collect();
        registered.sort_unstable();
        if registered.windows(2).any(|pair| pair[0] == pair[1]) {
            bail!("duplicate entry in fuzz target registry");
        }
        if manifest != registered {
            bail!("fuzz target registry does not match fuzz/Cargo.toml\n  registry: {}\n  manifest: {}", registered.join(", "), manifest.join(", "));
        }
        let target_dir = PathBuf::from(
            metadata["target_directory"]
                .as_str()
                .context("Cargo metadata missing target_directory")?,
        );
        if !target_dir.is_absolute() {
            bail!(
                "Cargo metadata returned a relative target directory: {}",
                target_dir.display()
            );
        }
        Ok(Self {
            root,
            target_dir,
            host: OnceLock::new(),
            runner,
            build_timeout,
        })
    }

    pub fn host(&self) -> Result<&str> {
        if self.host.get().is_none() {
            let mut command = Command::new("rustc");
            command.args(["+nightly", "-vV"]).current_dir(&self.root);
            let output = self
                .runner
                .run(command, Some(QUERY_TIMEOUT), OutputMode::Capture)?
                .success("query nightly host")?;
            let text = String::from_utf8(output.stdout.complete()?)?;
            let host = text
                .lines()
                .find_map(|line| line.strip_prefix("host: "))
                .filter(|host| !host.is_empty())
                .context("rustc did not report its host triple")?;
            let _ = self.host.set(host.to_string());
        }
        Ok(self.host.get().expect("host initialized"))
    }

    pub fn build_dir(&self, mode: &str) -> PathBuf {
        self.target_dir.join("smelt-fuzz").join(mode)
    }

    fn binary(&self, mode: &str, profile: &str, name: &str) -> Result<PathBuf> {
        Ok(self
            .build_dir(mode)
            .join(self.host()?)
            .join(profile)
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX)))
    }

    pub fn helper(&self, name: &str) -> Result<PathBuf> {
        self.binary("tools", "debug", name)
    }

    pub fn build_helpers(&self, names: &[&str]) -> Result<()> {
        let mut command = Command::new("cargo");
        command
            .args(["build", "--manifest-path"])
            .arg(self.root.join("fuzz/Cargo.toml"))
            .args([
                "--features",
                "scenario-tools",
                "--target",
                self.host()?,
                "--target-dir",
            ])
            .arg(self.build_dir("tools"))
            .current_dir(&self.root);
        for name in names {
            command.args(["--bin", name]);
        }
        self.run_build(command, "build scenario tools")
    }

    pub fn cargo_fuzz(&self, subcommand: &str, sanitizer: &str) -> Result<Command> {
        let mut command = Command::new("cargo");
        command
            .args(["+nightly", "fuzz", subcommand, "--fuzz-dir"])
            .arg(self.root.join("fuzz"))
            .args([
                "--target",
                self.host()?,
                "--sanitizer",
                sanitizer,
                "--target-dir",
            ])
            .arg(self.build_dir(if subcommand == "coverage" {
                "coverage"
            } else {
                sanitizer
            }))
            .current_dir(&self.root);
        Ok(command)
    }

    pub fn build_targets(&self, targets: &[String], sanitizer: &str) -> Result<()> {
        if targets.is_empty() {
            let command = self.cargo_fuzz("build", sanitizer)?;
            self.run_build(command, "build all fuzz targets")?;
        } else {
            for target in targets.iter().collect::<std::collections::BTreeSet<_>>() {
                let mut command = self.cargo_fuzz("build", sanitizer)?;
                command.arg(target);
                self.run_build(command, &format!("build {target}"))?;
            }
        }
        Ok(())
    }

    fn run_build(&self, command: Command, label: &str) -> Result<()> {
        let output = self
            .runner
            .run(command, Some(self.build_timeout), OutputMode::Inherit)?;
        let timed_out = matches!(output.termination, Termination::TimedOut);
        let result = output.success(label).map(|_| ());
        if timed_out {
            result.with_context(|| {
                format!(
                    "{}s build watchdog expired; increase --build-timeout SECONDS for a cold build",
                    self.build_timeout.as_secs()
                )
            })
        } else {
            result
        }
    }

    pub fn target_command(
        &self,
        target: &str,
        sanitizer: &str,
        data: &FuzzData,
    ) -> Result<Command> {
        let binary = self.binary(sanitizer, "release", target)?;
        if !binary.is_file() {
            bail!("built fuzz target not found: {}", binary.display());
        }
        let mut command = Command::new(binary);
        command.current_dir(&self.root).arg(path_arg(
            "-artifact_prefix=",
            &data.artifacts(target).join(""),
        ));
        let options = match sanitizer {
            "address" => Some(("ASAN_OPTIONS", "detect_odr_violation=0")),
            "thread" => Some(("TSAN_OPTIONS", "report_signal_unsafe=0")),
            _ => None,
        };
        if let Some((name, extra)) = options {
            let mut value = std::env::var_os(name).unwrap_or_default();
            if !value.is_empty() {
                value.push(":");
            }
            value.push(extra);
            command.env(name, value);
        }
        Ok(command)
    }

    pub fn coverage_binary(&self, target: &str) -> Result<PathBuf> {
        self.binary("coverage", "release", target)
    }

    pub fn llvm_cov(&self) -> Result<PathBuf> {
        let mut command = Command::new("rustc");
        command
            .args(["+nightly", "--print", "sysroot"])
            .current_dir(&self.root);
        let output = self
            .runner
            .run(command, Some(QUERY_TIMEOUT), OutputMode::Capture)?
            .success("query nightly sysroot")?;
        let sysroot = String::from_utf8(output.stdout.complete()?)?;
        let path = Path::new(sysroot.trim())
            .join("lib/rustlib")
            .join(self.host()?)
            .join("bin")
            .join(format!("llvm-cov{}", std::env::consts::EXE_SUFFIX));
        if !path.is_file() {
            bail!("nightly llvm-cov not found at {}; run: rustup component add llvm-tools-preview --toolchain nightly", path.display());
        }
        Ok(path)
    }
}

pub(super) fn path_arg(prefix: &str, path: &Path) -> OsString {
    let mut arg = OsString::from(prefix);
    arg.push(path);
    arg
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(target_dir: &Path) -> Value {
        serde_json::json!({
            "target_directory": target_dir,
            "packages": [{"name": "smelt-fuzz", "targets": TARGETS.iter().map(|target| {
                serde_json::json!({"name": target.name, "kind": ["bin"],
                    "src_path": format!("/repo/fuzz/fuzz_targets/{}.rs", target.name)})
            }).collect::<Vec<_>>()}]
        })
    }

    #[test]
    fn resolves_every_build_mode_from_cargo_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let configured = dir.path().join("custom build directory");
        let project = Project::from_metadata(
            dir.path().into(),
            &metadata(&configured),
            process::Runner::new().unwrap(),
            Duration::from_secs(1800),
        )
        .unwrap();
        project.host.set("test-host".into()).unwrap();
        let name = format!("replay_scenario{}", std::env::consts::EXE_SUFFIX);
        assert_eq!(
            project.helper("replay_scenario").unwrap(),
            configured
                .join("smelt-fuzz/tools/test-host/debug")
                .join(name)
        );
        assert_ne!(project.build_dir("none"), project.build_dir("address"));
        assert_ne!(project.build_dir("address"), project.build_dir("coverage"));
    }

    #[test]
    fn rejects_missing_or_duplicate_manifest_targets() {
        let dir = tempfile::tempdir().unwrap();
        let mut value = metadata(dir.path());
        value["packages"][0]["targets"]
            .as_array_mut()
            .unwrap()
            .pop();
        assert!(Project::from_metadata(
            dir.path().into(),
            &value,
            process::Runner::new().unwrap(),
            Duration::from_secs(1800)
        )
        .is_err());
        let mut value = metadata(dir.path());
        let duplicate = value["packages"][0]["targets"][0].clone();
        value["packages"][0]["targets"]
            .as_array_mut()
            .unwrap()
            .push(duplicate);
        assert!(Project::from_metadata(
            dir.path().into(),
            &value,
            process::Runner::new().unwrap(),
            Duration::from_secs(1800)
        )
        .is_err());
    }
}
