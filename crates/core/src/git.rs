//! Git snapshots and explicit file-level index operations, off the UI thread.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::diff::{Diff, DiffView};

pub struct DiffSnapshot {
    pub root: PathBuf,
    pub branch: String,
    pub view: Arc<DiffView>,
}

#[derive(Clone, Debug)]
pub(crate) struct FileStatus {
    pub index: char,
    pub worktree: char,
    pub old_path: Option<Vec<u8>>,
}

pub(crate) type Status = HashMap<Vec<u8>, FileStatus>;

#[cfg(test)]
fn file_status(file: &crate::diff::File, status: &Status) -> (char, char) {
    let mut columns = status
        .get(&file.raw_path)
        .map_or((' ', ' '), |s| (s.index, s.worktree));
    if file.raw_path != file.raw_old_path {
        if let Some(old) = status.get(&file.raw_old_path) {
            if old.index == 'D' && columns.0 == ' ' {
                columns.0 = 'R';
            }
            if old.worktree == 'D' {
                columns.1 = 'R';
            }
        }
    }
    columns
}

async fn read_status(root: &Path, cancel: &CancellationToken) -> Result<Status, String> {
    let output = checked(
        git(
            root,
            &args(&[
                "status",
                "--porcelain=v1",
                "-z",
                "--untracked-files=all",
                "--ignore-submodules=none",
            ]),
            cancel,
        )
        .await?,
    )?;
    let mut status = Status::new();
    let mut fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        if field.len() < 4 || field[2] != b' ' {
            return Err("invalid Git status record".into());
        }
        let mut index = char::from(field[0]);
        let mut worktree = char::from(field[1]);
        let old_path = if matches!(index, 'R' | 'C') || matches!(worktree, 'R' | 'C') {
            Some(fields.next().ok_or("missing Git rename source")?.to_vec())
        } else {
            None
        };
        if index == '?' {
            index = ' ';
            worktree = '?';
        }
        let entry = status.entry(field[3..].to_vec()).or_insert(FileStatus {
            index: ' ',
            worktree: ' ',
            old_path: None,
        });
        if index != ' ' {
            entry.index = index;
        }
        if worktree != ' ' {
            entry.worktree = worktree;
        }
        if old_path.is_some() {
            entry.old_path = old_path;
        }
    }
    Ok(status)
}

#[derive(Clone, Copy)]
pub(crate) enum IndexAction {
    Stage,
    Unstage,
    Toggle,
}

/// Mutate only the selected file's index entries. A toggle acts on the selected
/// section, never on a different section that also contains the same path.
pub(crate) async fn update_index(
    root: PathBuf,
    file: crate::diff::File,
    action: IndexAction,
    cancel: CancellationToken,
) -> Result<(), String> {
    let status = read_status(&root, &cancel).await?;
    let stage = match action {
        IndexAction::Stage => true,
        IndexAction::Unstage => false,
        IndexAction::Toggle => file.section == crate::diff::Section::Unstaged,
    };
    let mut paths = Vec::new();
    if let Some(current) = status.get(&file.raw_path) {
        if if stage {
            current.worktree != ' '
        } else {
            current.index != ' '
        } {
            paths.push(file.raw_path.clone());
            if !stage && current.index == 'R' {
                if let Some(old) = &current.old_path {
                    paths.push(old.clone());
                }
            }
        }
    }
    if file.raw_old_path != file.raw_path {
        if let Some(old) = status.get(&file.raw_old_path) {
            if if stage {
                old.worktree == 'D'
            } else {
                old.index == 'D'
            } {
                paths.push(file.raw_old_path.clone());
            }
        }
    }
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Ok(());
    }
    if !stage
        && status.iter().any(|(other, state)| {
            state.index != ' '
                && !paths.contains(other)
                && paths.iter().any(|path| {
                    other
                        .strip_prefix(path.as_slice())
                        .is_some_and(|rest| rest.starts_with(b"/"))
                        || (state.index != 'D'
                            && path
                                .strip_prefix(other.as_slice())
                                .is_some_and(|rest| rest.starts_with(b"/")))
                })
        })
    {
        return Err(
            "conflicting staged paths: unstage the conflicting directory changes first".into(),
        );
    }
    let input: Vec<_> = paths
        .into_iter()
        .flat_map(|path| path.into_iter().chain([0]))
        .collect();
    let argv = if stage {
        // update-index consumes exact file names, including deletions. A literal
        // `git add` pathspec would also stage an unrelated replacement directory.
        args(&["update-index", "--add", "--remove", "-z", "--stdin"])
    } else {
        let head = git(
            &root,
            &args(&["rev-parse", "--verify", "--quiet", "HEAD"]),
            &cancel,
        )
        .await?;
        let base = if head.status.success() {
            head.stdout
        } else {
            checked(
                git(
                    &root,
                    &args(&["hash-object", "-t", "tree", "--stdin"]),
                    &cancel,
                )
                .await?,
            )?
        };
        let mut argv = args(&["reset", "--quiet"]);
        argv.push(OsString::from(String::from_utf8_lossy(&base).trim()));
        argv.extend(args(&["--pathspec-from-file=-", "--pathspec-file-nul"]));
        argv
    };
    checked(git_input(&root, &argv, Some(&input), None, &cancel).await?)?;
    Ok(())
}

async fn git(
    root: &Path,
    args: &[OsString],
    cancel: &CancellationToken,
) -> Result<std::process::Output, String> {
    git_input(root, args, None, None, cancel).await
}

async fn git_input(
    root: &Path,
    args: &[OsString],
    input: Option<&[u8]>,
    index: Option<&PrivateIndex>,
    cancel: &CancellationToken,
) -> Result<std::process::Output, String> {
    if cancel.is_cancelled() {
        return Err("cancelled".into());
    }
    let mut command = Command::new("git");
    command.current_dir(root).args([
        "--no-pager",
        "--literal-pathspecs",
        "-c",
        "core.quotePath=true",
        "-c",
        "diff.suppressBlankEmpty=false",
    ]);
    if let Some(index) = index {
        command
            .args(["-c", "core.splitIndex=false", "-c", "core.fsmonitor=false"])
            .env("GIT_INDEX_FILE", index.directory.path().join("index"))
            .env(
                "GIT_OBJECT_DIRECTORY",
                index.directory.path().join("objects"),
            )
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", &index.alternates);
    }
    command
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = async {
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let write = async {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                stdin.write_all(input).await?;
            }
            Ok::<_, std::io::Error>(())
        };
        let (_, output) = tokio::try_join!(write, child.wait_with_output())?;
        Ok::<_, std::io::Error>(output)
    };
    tokio::select! {
        _ = cancel.cancelled() => Err("cancelled".into()),
        result = output => result.map_err(|error| format!("git: {error}")),
    }
}

fn args(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

fn checked(output: std::process::Output) -> Result<Vec<u8>, String> {
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

fn path_bytes(bytes: Vec<u8>) -> OsString {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(bytes)
    }
    #[cfg(not(unix))]
    {
        OsString::from(String::from_utf8_lossy(&bytes).as_ref())
    }
}

/// Intent-to-add entries let one Git diff include every untracked file, with
/// Git's own attributes, filters, encodings and binary handling. Both index and
/// object writes are isolated; the repository's index and objects are read-only.
struct PrivateIndex {
    directory: tempfile::TempDir,
    alternates: OsString,
}

impl PrivateIndex {
    async fn new(root: &Path, cancel: &CancellationToken) -> Result<Self, String> {
        async fn git_path(
            root: &Path,
            name: &str,
            cancel: &CancellationToken,
        ) -> Result<PathBuf, String> {
            let mut bytes = checked(
                git(
                    root,
                    &args(&["rev-parse", "--path-format=absolute", "--git-path", name]),
                    cancel,
                )
                .await?,
            )?;
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            Ok(PathBuf::from(path_bytes(bytes)))
        }
        let (index, objects) = tokio::try_join!(
            git_path(root, "index", cancel),
            git_path(root, "objects", cancel),
        )?;
        tokio::task::spawn_blocking(move || -> std::io::Result<Self> {
            use std::fmt::Write;
            let directory = tempfile::tempdir()?;
            std::fs::create_dir(directory.path().join("objects"))?;
            match std::fs::copy(index, directory.path().join("index")) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            // Git accepts C-quoted alternate paths. Octal quoting also handles
            // path-list separators, newlines and non-UTF-8 Unix directory names.
            let mut quoted = String::from("\"");
            for byte in objects.as_os_str().as_encoded_bytes() {
                write!(quoted, "\\{byte:03o}").unwrap();
            }
            quoted.push('"');
            let mut alternates = OsString::from(quoted);
            if let Some(existing) = std::env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
                alternates.push(if cfg!(windows) { ";" } else { ":" });
                alternates.push(existing);
            }
            Ok(Self {
                directory,
                alternates,
            })
        })
        .await
        .map_err(|error| format!("diff temporary index: {error}"))?
        .map_err(|error| format!("diff temporary index: {error}"))
    }
}

pub async fn working_diff(
    cwd: PathBuf,
    context: usize,
    cancel: CancellationToken,
) -> Result<DiffSnapshot, String> {
    tokio::time::timeout(
        Duration::from_secs(120),
        load_working_diff(cwd, context, cancel),
    )
    .await
    .map_err(|_| "git diff timed out after 120 seconds".to_string())?
}

async fn load_working_diff(
    cwd: PathBuf,
    context: usize,
    cancel: CancellationToken,
) -> Result<DiffSnapshot, String> {
    let mut root = checked(git(&cwd, &args(&["rev-parse", "--show-toplevel"]), &cancel).await?)?;
    if root.last() == Some(&b'\n') {
        root.pop();
    }
    let root = PathBuf::from(path_bytes(root));
    let head = git(
        &root,
        &args(&["rev-parse", "--verify", "--quiet", "HEAD"]),
        &cancel,
    )
    .await?;
    let base = if head.status.success() {
        "HEAD".to_string()
    } else {
        String::from_utf8_lossy(&checked(
            git(
                &root,
                &args(&["hash-object", "-t", "tree", "--stdin"]),
                &cancel,
            )
            .await?,
        )?)
        .trim()
        .to_string()
    };
    let branch = git(
        &root,
        &args(&["symbolic-ref", "--quiet", "--short", "HEAD"]),
        &cancel,
    )
    .await?;
    let branch = if branch.status.success() {
        String::from_utf8_lossy(&branch.stdout)
            .trim_end_matches('\n')
            .to_string()
    } else if head.status.success() {
        format!(
            "detached {}",
            String::from_utf8_lossy(&head.stdout)
                .chars()
                .take(8)
                .collect::<String>()
        )
    } else {
        "unborn HEAD".to_string()
    };

    // Full context is indexed once, then folded without loading source blobs or
    // running Git again. Scrolling/expanding never shells out or reparses a patch.
    let options = [
        "--patch",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-relative",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        "--unified=2147483647",
        "--diff-algorithm=histogram",
        "--submodule=short",
        "--ignore-submodules=none",
    ];
    let mut diff_args = args(&["diff", "--ours"]);
    diff_args.extend(args(&options));
    diff_args.push("--".into());
    let mut staged_args = args(&["diff", "--cached"]);
    staged_args.extend(args(&options));
    staged_args.extend(args(&[&base, "--"]));
    let status = read_status(&root, &cancel).await?;
    let index = if status.values().any(|state| state.worktree == '?') {
        let index = PrivateIndex::new(&root, &cancel).await?;
        // One root pathspec avoids all-pairs matching between untracked paths
        // and directory entries. Intent-to-add honors ignore rules without
        // hashing tracked contents; all writes stay inside the private index.
        checked(
            git_input(
                &root,
                &args(&["add", "--intent-to-add", "--ignore-removal", "--", "."]),
                None,
                Some(&index),
                &cancel,
            )
            .await?,
        )?;
        Some(index)
    } else {
        None
    };
    let untracked = async {
        let Some(index) = &index else {
            return Ok(Vec::new());
        };
        let mut argv = args(&["diff", "--diff-filter=A", "--no-renames"]);
        argv.extend(args(&options));
        argv.push("--".into());
        checked(git_input(&root, &argv, None, Some(index), &cancel).await?)
    };
    // Tracked changes always use the real index. Adding an untracked directory
    // to a private index can displace its deleted tracked ancestor.
    let (unstaged, untracked, staged) = tokio::try_join!(
        git(&root, &diff_args, &cancel),
        untracked,
        git(&root, &staged_args, &cancel),
    )?;
    let (unstaged, staged) = (checked(unstaged)?, checked(staged)?);
    let index_cancel = cancel.child_token();
    let _index_guard = index_cancel.clone().drop_guard();
    let index_status = status;
    let view = tokio::task::spawn_blocking(move || {
        let mut patch = String::from_utf8_lossy(&unstaged).into_owned();
        let untracked_start = patch.len();
        patch.push_str(&String::from_utf8_lossy(&untracked));
        let staged_start = patch.len();
        patch.push_str(&String::from_utf8_lossy(&staged));
        let mut diff = Diff::parse_sections(
            patch,
            staged_start,
            || index_cancel.is_cancelled(),
            |path, offset| {
                offset < untracked_start
                    || offset >= staged_start
                    || index_status
                        .get(path)
                        .is_some_and(|status| status.worktree == '?')
            },
        )
        .ok_or_else(|| "cancelled".to_string())?;
        let mut unmerged: std::collections::HashSet<_> = index_status
            .iter()
            .filter(|(_, state)| {
                state.index == 'U'
                    || state.worktree == 'U'
                    || matches!((state.index, state.worktree), ('A', 'A') | ('D', 'D'))
            })
            .map(|(path, _)| path.clone())
            .collect();
        for file in &mut diff.files {
            if file.section == crate::diff::Section::Unstaged {
                if unmerged.remove(&file.raw_path) {
                    file.status = "U";
                } else if file.status == "A"
                    && index_status
                        .get(&file.raw_path)
                        .is_some_and(|status| status.worktree == '?')
                {
                    file.status = "?";
                }
            }
        }
        if !unmerged.is_empty() {
            for path in unmerged {
                diff.add_unmerged_file(path);
            }
            diff.sort_files(&|| index_cancel.is_cancelled())
                .ok_or("cancelled")?;
        }
        Ok::<_, String>(Arc::new(DiffView::new(Arc::new(diff), context)))
    });
    let view = tokio::select! {
        _ = cancel.cancelled() => return Err("cancelled".into()),
        view = view => view.map_err(|error| format!("diff index: {error}"))??,
    };
    Ok(DiffSnapshot { root, branch, view })
}

#[cfg(test)]
mod tests {
    use super::*;
    use smelt_buffer::document::RowSource;

    fn run(root: &Path, argv: &[&str]) {
        let output = std::process::Command::new("git")
            .current_dir(root)
            .args(argv)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository_files(root: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.extend(repository_files(&path));
            } else {
                files.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
        files
    }

    #[tokio::test]
    async fn working_snapshot_includes_staged_unstaged_untracked_renames_and_binary() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.name", "test"]);
        run(root, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("tracked"), "base\n").unwrap();
        std::fs::write(root.join("rename me"), "rename body\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "base"]);
        std::fs::write(root.join("tracked"), "staged\n").unwrap();
        run(root, &["add", "tracked"]);
        std::fs::write(root.join("tracked"), "unstaged\n").unwrap();
        run(root, &["mv", "rename me", "renamed file"]);
        std::fs::write(root.join("new\t界"), "untracked\n").unwrap();
        std::fs::create_dir(root.join("space b")).unwrap();
        std::fs::write(root.join("space b/binary"), [0u8, 1, 2]).unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(snapshot.view.diff.files.len(), 5);
        assert!(snapshot
            .view
            .diff
            .files
            .iter()
            .any(|file| file.status == "R" && file.path == "renamed file"));
        assert!(snapshot
            .view
            .diff
            .files
            .iter()
            .any(|file| file.path == "new\t界"));
        assert!(
            snapshot
                .view
                .diff
                .files
                .iter()
                .any(|file| file.path == "space b/binary"),
            "{:?}",
            snapshot.view.diff.files
        );
        let rows = snapshot.view.rows(0..100, &crate::theme::Theme::default());
        assert!(rows.iter().any(|row| row.text == "+ unstaged"));
        assert!(rows.iter().any(|row| row.text == "+ staged"));
        assert!(rows.iter().any(|row| row.text == "- staged"));
        assert!(rows.iter().any(|row| row.text.contains("Binary files")));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn deletion_mode_change_symlink_and_empty_untracked_are_visible() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.name", "test"]);
        run(root, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("deleted"), "old\n").unwrap();
        std::fs::write(root.join("executable"), "script\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "base"]);
        std::fs::remove_file(root.join("deleted")).unwrap();
        std::fs::set_permissions(
            root.join("executable"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        symlink("missing", root.join("link")).unwrap();
        std::fs::write(root.join("empty"), "").unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let files = &snapshot.view.diff.files;
        assert_eq!(files.len(), 4, "{files:?}");
        assert!(files
            .iter()
            .any(|file| file.path == "deleted" && file.status == "D"));
        assert!(files
            .iter()
            .any(|file| file.path == "executable" && file.status == "T"));
        assert!(files
            .iter()
            .any(|file| file.path == "link" && file.status == "?"));
        assert!(files
            .iter()
            .any(|file| file.path == "empty" && file.status == "?"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn untracked_rows_match_git_attributes_filters_encodings_modes_and_newlines() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "core.autocrlf", "true"]);
        run(root, &["config", "filter.canonical.clean", "tr a-z A-Z"]);
        run(root, &["config", "diff.forced.binary", "true"]);
        std::fs::write(
            root.join(".gitattributes"),
            "*.force -diff\n*.text diff\n*.clean filter=canonical\n*.utf16 text working-tree-encoding=UTF-16LE\n*.driver diff=forced\n",
        ).unwrap();
        async fn compare(root: &Path, snapshot: &DiffSnapshot, path: &str) {
            let cancel = CancellationToken::new();
            let expected = git(
                root,
                &args(&[
                    "diff",
                    "--no-index",
                    "--no-color",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--src-prefix=a/",
                    "--dst-prefix=b/",
                    "--",
                    "/dev/null",
                    path,
                ]),
                &cancel,
            )
            .await
            .unwrap();
            assert!(expected.status.success() || expected.status.code() == Some(1));
            let mut expected = Diff::parse(String::from_utf8_lossy(&expected.stdout).into_owned());
            expected.files[0].status = "?";
            let expected = DiffView::new(Arc::new(expected), 3);
            let file = snapshot
                .view
                .diff
                .files
                .iter()
                .position(|file| file.path == path)
                .unwrap();
            let start = snapshot.view.file_row(file).unwrap();
            let end = snapshot
                .view
                .file_row(file + 1)
                .map_or(u64::MAX, |row| row - 2);
            let rows = |rows: Vec<smelt_buffer::document::DocumentRow>| {
                rows.into_iter()
                    .map(|row| (row.text, row.decoration.source_line))
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                rows(
                    snapshot
                        .view
                        .rows(start..end, &crate::theme::Theme::default())
                ),
                rows(expected.rows(0..u64::MAX, &crate::theme::Theme::default())),
                "{path}"
            );
        }
        let cases = [
            ("normal.rs", &b"let value = 42;\n"[..]),
            ("crlf.rs", &b"first\r\nsecond\r\n"[..]),
            ("no-newline", &b"first\nlast"[..]),
            ("empty", &b""[..]),
            ("empty.force", &b""[..]),
            ("binary", &b"a\0b"[..]),
            ("binary.text", &b"a\0b"[..]),
            ("ascii.force", &b"text\n"[..]),
            ("quoted \"\\\t界.rs", &b"source\n"[..]),
            ("invalid-utf8", &[0xff, b'\n'][..]),
            ("executable", &b"#!/bin/sh\n"[..]),
            ("filter.clean", &b"normalized\n"[..]),
            ("encoding.utf16", &b"a\0b\0\n\0"[..]),
            ("binary.driver", &b"source\n"[..]),
            (":(glob)*.[rs]", &b"literal path\n"[..]),
        ];
        for (path, data) in cases {
            std::fs::write(root.join(path), data).unwrap();
            if path == "executable" {
                std::fs::set_permissions(root.join(path), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
        }
        symlink("missing\n界", root.join("link")).unwrap();
        let before = repository_files(&root.join(".git"));
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(before, repository_files(&root.join(".git")));
        assert_eq!(snapshot.view.diff.files.len(), cases.len() + 2);
        for (path, _) in cases {
            compare(root, &snapshot, path).await;
        }
        compare(root, &snapshot, "link").await;
        let rows = snapshot.view.rows(0..200, &crate::theme::Theme::default());
        assert!(rows.iter().any(|row| row.text == "+ NORMALIZED"));
        assert!(rows.iter().any(|row| row.text == "+ ab"));
    }

    #[tokio::test]
    async fn snapshot_preserves_linked_worktree_split_index_and_keeps_index_only_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir
            .path()
            .join(if cfg!(unix) { "repo: \"界" } else { "repo" });
        std::fs::create_dir(&root).unwrap();
        run(&root, &["init", "-q"]);
        run(&root, &["config", "user.name", "test"]);
        run(&root, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("tracked"), "base\n").unwrap();
        std::fs::write(root.join("recreated"), "base\n").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored*\n").unwrap();
        run(&root, &["add", "."]);
        run(&root, &["commit", "-qm", "base"]);
        run(&root, &["worktree", "add", "-qb", "linked", "../linked"]);
        let worktree = dir.path().join("linked");
        std::fs::write(worktree.join("tracked"), "staged\n").unwrap();
        run(&worktree, &["add", "tracked"]);
        std::fs::write(worktree.join("tracked"), "base\n").unwrap();
        run(&worktree, &["rm", "-q", "recreated"]);
        std::fs::write(worktree.join("recreated"), "replacement\n").unwrap();
        std::fs::write(worktree.join("ignored-tracked"), "included\n").unwrap();
        run(&worktree, &["add", "-f", "ignored-tracked"]);
        std::fs::write(worktree.join("ignored-untracked"), "excluded\n").unwrap();
        std::fs::write(worktree.join("new"), "new\n").unwrap();
        run(&worktree, &["config", "core.splitIndex", "true"]);
        run(&worktree, &["update-index", "--split-index"]);
        let before = repository_files(&root.join(".git"));
        let snapshot = working_diff(worktree.clone(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(before, repository_files(&root.join(".git")));
        assert_eq!(snapshot.root, worktree);
        assert_eq!(snapshot.branch, "linked");
        let files = &snapshot.view.diff.files;
        assert_eq!(files.len(), 6, "{files:?}");
        for path in ["tracked", "recreated"] {
            let entries: Vec<_> = files.iter().filter(|f| f.path == path).collect();
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0].section, crate::diff::Section::Unstaged);
            assert_eq!(entries[1].section, crate::diff::Section::Staged);
        }
        assert!(files
            .iter()
            .any(|f| f.path == "recreated" && f.status == "?"));
        assert!(files
            .iter()
            .any(|f| f.path == "recreated" && f.status == "D"));
        let rows = snapshot.view.rows(0..100, &crate::theme::Theme::default());
        assert!(rows.iter().any(|row| row.text == "+ included"));
        assert!(rows.iter().any(|row| row.text == "+ replacement"));
        assert!(rows.iter().any(|row| row.text == "+ staged"));
        assert!(rows.iter().any(|row| row.text == "- staged"));
        assert!(rows.iter().any(|row| row.text == "+ base"));
        assert!(rows.iter().any(|row| row.text == "- base"));
    }

    #[tokio::test]
    async fn index_operations_handle_unborn_renames_deletions_and_cancelled_requests() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.name", "test"]);
        run(root, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("new"), "new\n").unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let file = snapshot.view.diff.files[0].clone();
        update_index(
            root.to_owned(),
            file.clone(),
            IndexAction::Stage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = read_status(root, &CancellationToken::new()).await.unwrap();
        assert_eq!(file_status(&file, &status), ('A', ' '));
        update_index(
            root.to_owned(),
            file.clone(),
            IndexAction::Unstage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = read_status(root, &CancellationToken::new()).await.unwrap();
        assert_eq!(file_status(&file, &status), (' ', '?'));
        assert_eq!(std::fs::read_to_string(root.join("new")).unwrap(), "new\n");
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "base"]);
        run(root, &["mv", "new", "renamed"]);
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let file = snapshot.view.diff.files[0].clone();
        assert_eq!(file.status, "R");
        update_index(
            root.to_owned(),
            file.clone(),
            IndexAction::Unstage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = read_status(root, &CancellationToken::new()).await.unwrap();
        assert_eq!(status[b"new".as_slice()].worktree, 'D');
        assert_eq!(status[b"renamed".as_slice()].worktree, '?');
        update_index(
            root.to_owned(),
            file.clone(),
            IndexAction::Stage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = read_status(root, &CancellationToken::new()).await.unwrap();
        assert_eq!(file_status(&file, &status), ('R', ' '));
        assert!(!root.join("new").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("renamed")).unwrap(),
            "new\n"
        );
        run(root, &["commit", "-qm", "rename"]);
        std::fs::remove_file(root.join("renamed")).unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let file = snapshot.view.diff.files[0].clone();
        let before = repository_files(&root.join(".git"));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(
            update_index(root.to_owned(), file.clone(), IndexAction::Stage, cancel)
                .await
                .is_err()
        );
        assert_eq!(before, repository_files(&root.join(".git")));
        update_index(
            root.to_owned(),
            file.clone(),
            IndexAction::Toggle,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = read_status(root, &CancellationToken::new()).await.unwrap();
        assert_eq!(file_status(&file, &status), ('D', ' '));
        let staged = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        update_index(
            root.to_owned(),
            staged.view.diff.files[0].clone(),
            IndexAction::Toggle,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = read_status(root, &CancellationToken::new()).await.unwrap();
        assert_eq!(file_status(&file, &status), (' ', 'D'));
        assert!(!root.join("renamed").exists());
        std::fs::write(root.join(".git/index.lock"), "held").unwrap();
        assert!(update_index(
            root.to_owned(),
            file,
            IndexAction::Stage,
            CancellationToken::new()
        )
        .await
        .unwrap_err()
        .contains("index.lock"));
    }

    #[tokio::test]
    async fn index_operations_preserve_replacements_and_resolve_net_zero_changes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        run(root, &["config", "user.name", "test"]);
        run(root, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(root.join("old"), "original\n").unwrap();
        std::fs::write(root.join("zero"), "base\n").unwrap();
        run(root, &["add", "."]);
        run(root, &["commit", "-qm", "base"]);
        run(root, &["mv", "old", "renamed"]);
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let renamed = snapshot
            .view
            .diff
            .files
            .iter()
            .find(|f| f.path == "renamed")
            .unwrap()
            .clone();
        std::fs::write(root.join("old"), "replacement\n").unwrap();
        run(root, &["add", "old"]);
        let old_entry = checked(
            git(root, &args(&["show", ":old"]), &CancellationToken::new())
                .await
                .unwrap(),
        )
        .unwrap();
        update_index(
            root.to_owned(),
            renamed,
            IndexAction::Unstage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            checked(
                git(root, &args(&["show", ":old"]), &CancellationToken::new())
                    .await
                    .unwrap()
            )
            .unwrap(),
            old_entry
        );
        assert_eq!(
            std::fs::read_to_string(root.join("renamed")).unwrap(),
            "original\n"
        );
        for action in [IndexAction::Stage, IndexAction::Unstage] {
            std::fs::write(root.join("zero"), "staged\n").unwrap();
            run(root, &["add", "zero"]);
            std::fs::write(root.join("zero"), "base\n").unwrap();
            let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
                .await
                .unwrap();
            let file = snapshot
                .view
                .diff
                .files
                .iter()
                .find(|f| f.path == "zero")
                .unwrap()
                .clone();
            assert_eq!(
                snapshot
                    .view
                    .diff
                    .files
                    .iter()
                    .filter(|f| f.path == "zero")
                    .count(),
                2
            );
            update_index(root.to_owned(), file, action, CancellationToken::new())
                .await
                .unwrap();
            let status = read_status(root, &CancellationToken::new()).await.unwrap();
            assert!(!status.contains_key(b"zero".as_slice()));
            assert_eq!(
                std::fs::read_to_string(root.join("zero")).unwrap(),
                "base\n"
            );
        }
        run(root, &["rm", "-q", "zero"]);
        std::fs::write(root.join("zero"), "replacement\n").unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let file = snapshot
            .view
            .diff
            .files
            .iter()
            .find(|f| f.path == "zero")
            .unwrap()
            .clone();
        assert_eq!(
            file_status(
                &file,
                &read_status(root, &CancellationToken::new()).await.unwrap()
            ),
            ('D', '?')
        );
        update_index(
            root.to_owned(),
            file.clone(),
            IndexAction::Toggle,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = read_status(root, &CancellationToken::new()).await.unwrap();
        assert_eq!(file_status(&file, &status), ('M', ' '));
        update_index(
            root.to_owned(),
            file,
            IndexAction::Unstage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("zero")).unwrap(),
            "replacement\n"
        );
        std::fs::remove_file(root.join("zero")).unwrap();
        std::fs::create_dir(root.join("zero")).unwrap();
        std::fs::write(root.join("zero/child"), "unrelated\n").unwrap();
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        let file = snapshot
            .view
            .diff
            .files
            .iter()
            .find(|f| f.path == "zero")
            .unwrap()
            .clone();
        update_index(
            root.to_owned(),
            file,
            IndexAction::Stage,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let status = read_status(root, &CancellationToken::new()).await.unwrap();
        assert_eq!(
            status[b"zero/child".as_slice()].index,
            ' ',
            "staging a deleted file staged its replacement directory"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("zero/child")).unwrap(),
            "unrelated\n"
        );
        run(root, &["add", "zero/child"]);
        let before = std::fs::read(root.join(".git/index")).unwrap();
        let file = snapshot
            .view
            .diff
            .files
            .iter()
            .find(|f| f.path == "zero")
            .unwrap()
            .clone();
        assert!(
            update_index(
                root.to_owned(),
                file,
                IndexAction::Unstage,
                CancellationToken::new()
            )
            .await
            .is_err(),
            "unstaging a file discarded staged descendants"
        );
        assert_eq!(std::fs::read(root.join(".git/index")).unwrap(), before);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn staging_uses_literal_raw_paths_not_lossy_display_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        run(root, &["init", "-q"]);
        let paths = [
            b"bad\xff.rs".to_vec(),
            b"bad\xfe.rs".to_vec(),
            b":(glob)*.rs".to_vec(),
            b"tab\tline\n.rs".to_vec(),
        ];
        for path in &paths {
            std::fs::write(root.join(path_bytes(path.clone())), b"source\n").unwrap();
        }
        let snapshot = working_diff(root.to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        for path in &paths {
            let file = snapshot
                .view
                .diff
                .files
                .iter()
                .find(|file| file.raw_path == *path)
                .unwrap()
                .clone();
            update_index(
                root.to_owned(),
                file.clone(),
                IndexAction::Stage,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let status = read_status(root, &CancellationToken::new()).await.unwrap();
            assert_eq!(
                status.values().filter(|state| state.index != ' ').count(),
                1
            );
            assert_eq!(status[path].index, 'A');
            update_index(
                root.to_owned(),
                file,
                IndexAction::Unstage,
                CancellationToken::new(),
            )
            .await
            .unwrap();
            let status = read_status(root, &CancellationToken::new()).await.unwrap();
            assert!(status.values().all(|state| state.index == ' '));
        }
    }

    #[tokio::test]
    async fn unborn_clean_non_repository_and_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            working_diff(dir.path().to_owned(), 3, CancellationToken::new())
                .await
                .is_err()
        );
        run(dir.path(), &["init", "-q"]);
        let snapshot = working_diff(dir.path().to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert!(snapshot.view.diff.files.is_empty());
        std::fs::write(dir.path().join("new"), "new\n").unwrap();
        let snapshot = working_diff(dir.path().to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(snapshot.view.diff.files[0].status, "?");
        assert_eq!(
            snapshot.view.diff.files[0].section,
            crate::diff::Section::Unstaged
        );
        run(dir.path(), &["add", "."]);
        let snapshot = working_diff(dir.path().to_owned(), 3, CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(snapshot.view.diff.files[0].status, "A");
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(working_diff(dir.path().to_owned(), 3, cancel)
            .await
            .is_err());
    }
}
