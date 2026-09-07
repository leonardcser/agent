//! Indexed diff and asynchronous Git snapshot primitives.

use super::document::LuaDocument;
use crate::diff::{Diff, DiffView};
use crate::lua::doc::{record_class, Tier};
use crate::lua::lua_type::{LuaClassDecl, LuaClassField, LuaType};
use crate::lua::module::LuaMod;
use crate::lua::LuaShared;
use mlua::prelude::*;
use std::sync::{Arc, Mutex};

struct Repository {
    root: std::path::PathBuf,
}

#[derive(Clone)]
struct LuaDiff(Arc<DiffView>, Option<Arc<Repository>>);

impl LuaType for LuaDiff {
    fn lua_type() -> String {
        "smelt.diff.Diff".into()
    }
}

impl FromLua for LuaDiff {
    fn from_lua(value: LuaValue, _: &Lua) -> LuaResult<Self> {
        match value {
            LuaValue::UserData(value) => Ok(value.borrow::<Self>()?.clone()),
            _ => Err(LuaError::external("expected a smelt.diff.Diff")),
        }
    }
}

fn file_metadata(lua: &Lua, file: &crate::diff::File) -> LuaResult<mlua::Table> {
    let row = lua.create_table()?;
    row.set("key", lua.create_string(&file.raw_path)?)?;
    row.set("path", file.path.as_str())?;
    row.set("old_path", file.old_path.as_str())?;
    row.set("status", file.status)?;
    row.set("section", file.section.name())?;
    row.set("additions", file.additions)?;
    row.set("deletions", file.deletions)?;
    row.set("binary", file.binary)?;
    row.set("hl_group", file.highlight_group())?;
    Ok(row)
}

fn section(name: &str) -> LuaResult<crate::diff::Section> {
    match name {
        "unstaged" => Ok(crate::diff::Section::Unstaged),
        "staged" => Ok(crate::diff::Section::Staged),
        _ => Err(LuaError::external("section must be unstaged or staged")),
    }
}

struct LuaDiffTree(Arc<crate::diff::tree::DiffTree>);

impl LuaType for LuaDiffTree {
    fn lua_type() -> String {
        "smelt.diff.Tree".into()
    }
}

impl mlua::UserData for LuaDiffTree {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("document", |_, this, ()| Ok(LuaDocument(this.0.clone())));
        methods.add_method("width", |_, this, width: usize| {
            this.0.set_width(width);
            Ok(())
        });
        methods.add_method("section_row", |_, this, name: String| {
            Ok(this.0.section_row(section(&name)?))
        });
        methods.add_method("collapsed", |_, this, ()| Ok(this.0.collapsed()));
        methods.add_method("toggle", |_, this, row: u64| Ok(this.0.toggle(row)));
        methods.add_method(
            "file_row",
            |_, this, (index, reveal): (usize, Option<bool>)| {
                Ok(index
                    .checked_sub(1)
                    .and_then(|index| this.0.file_row(index, reveal.unwrap_or(false))))
            },
        );
        methods.add_method("node", |lua, this, row: u64| {
            this.0
                .node(row)
                .map(|node| {
                    let value = lua.create_table()?;
                    value.set("index", node.file.map(|file| file + 1))?;
                    value.set("first", node.first + 1)?;
                    value.set("parent", node.parent)?;
                    value.set("children", node.children)?;
                    value.set("section", node.section.name())?;
                    value.set("group", node.group)?;
                    value.set("expanded", node.expanded)?;
                    Ok(value)
                })
                .transpose()
        });
    }
}

impl mlua::UserData for LuaDiff {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method("document", |_, this, ()| Ok(LuaDocument(this.0.clone())));
        methods.add_method("view", |_, this, ()| {
            Ok(LuaDiff(Arc::new(this.0.fork()), this.1.clone()))
        });
        methods.add_method("tree", |_, this, collapsed: Option<Vec<String>>| {
            let tree = this.0.tree();
            tree.restore_collapsed(&collapsed.unwrap_or_default());
            Ok(LuaDiffTree(Arc::new(tree)))
        });
        methods.add_method("files", |lua, this, ()| {
            let files = lua.create_table()?;
            for (index, file) in this.0.diff.files.iter().enumerate() {
                files.set(index + 1, file_metadata(lua, file)?)?;
            }
            Ok(files)
        });
        methods.add_method("file_count", |_, this, ()| Ok(this.0.diff.files.len()));
        methods.add_method("file", |lua, this, index: usize| {
            index
                .checked_sub(1)
                .and_then(|i| this.0.diff.files.get(i))
                .map(|file| file_metadata(lua, file))
                .transpose()
        });
        methods.add_method(
            "find_file",
            |_, this, (name, key): (String, mlua::LuaString)| {
                let section = section(&name)?;
                Ok(this
                    .0
                    .diff
                    .files
                    .iter()
                    .position(|file| {
                        file.section == section && file.raw_path == key.as_bytes().as_ref()
                    })
                    .map(|i| i + 1))
            },
        );
        methods.add_method("section_range", |_, this, name: String| {
            let section = section(&name)?;
            let files = &this.0.diff.files;
            Ok((
                files.partition_point(|file| file.section < section) + 1,
                files.partition_point(|file| file.section <= section),
            ))
        });
        methods.add_method("file_row", |_, this, index: usize| {
            Ok(index
                .checked_sub(1)
                .and_then(|index| this.0.file_row(index)))
        });
        methods.add_method("file_at", |_, this, row: u64| {
            Ok(this.0.file_at(row).map(|index| index + 1))
        });
        methods.add_method("hunk", |_, this, (row, forward): (u64, bool)| {
            Ok(this.0.hunk(row, forward))
        });
        methods.add_method("toggle_fold", |_, this, row: u64| {
            Ok(this.0.toggle_fold(row))
        });
        methods.add_method(
            "restore",
            |_, this, (previous, cursor, top): (LuaDiff, u64, u64)| {
                Ok(this.0.restore(&previous.0, cursor, top))
            },
        );
    }
}

enum GitResult {
    Snapshot(crate::git::DiffSnapshot),
    IndexUpdated {
        root: std::path::PathBuf,
        refresh: Result<crate::git::DiffSnapshot, String>,
    },
}

type DiffResult = Arc<Mutex<Option<Result<GitResult, String>>>>;
struct LuaDiffJob(DiffResult);

impl LuaType for LuaDiffJob {
    fn lua_type() -> String {
        "userdata".into()
    }
}

impl mlua::UserData for LuaDiffJob {
    fn add_methods<M: mlua::UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method(
            "take",
            |lua, this, ()| -> LuaResult<(Option<mlua::Table>, Option<String>)> {
                let result = this
                    .0
                    .lock()
                    .unwrap()
                    .take()
                    .ok_or_else(|| LuaError::external("git diff result is not available"))?;
                match result {
                    Ok(value) => {
                        let result = lua.create_table()?;
                        let snapshot = match value {
                            GitResult::Snapshot(snapshot) => Some(snapshot),
                            GitResult::IndexUpdated { root, refresh } => {
                                result.set("index_updated", true)?;
                                result.set("root", root.to_string_lossy().as_ref())?;
                                match refresh {
                                    Ok(snapshot) => Some(snapshot),
                                    Err(error) => {
                                        result.set("refresh_error", error)?;
                                        None
                                    }
                                }
                            }
                        };
                        if let Some(snapshot) = snapshot {
                            result.set("root", snapshot.root.to_string_lossy().as_ref())?;
                            result.set("branch", snapshot.branch)?;
                            result.set(
                                "diff",
                                LuaDiff(
                                    snapshot.view,
                                    Some(Arc::new(Repository {
                                        root: snapshot.root,
                                    })),
                                ),
                            )?;
                        }
                        Ok((Some(result), None))
                    }
                    Err(error) => Ok((None, Some(error))),
                }
            },
        );
    }
}

pub(super) fn register(lua: &Lua, smelt: &mlua::Table, shared: &Arc<LuaShared>) -> LuaResult<()> {
    let m = LuaMod::advanced(lua, smelt, "diff", "Compact unified-patch indexing, metadata and expandable context folds. Rendering cost is proportional to the viewport, not patch length.", Tier::Host)?;
    record_class(LuaClassDecl {
        name: "smelt.diff.File",
        classification: crate::lua::doc::classification_for_type("smelt.diff.File"),
        doc: "One section's changed file metadata. Paths are repository-relative; partially staged files have a separate entry in each section. Status is ?, A, D, M, R, T, or U (unmerged).",
        fields: vec![
            LuaClassField {
                name: "path",
                ty: "string".into(),
                optional: false,
                doc: "New path (old path for a deletion).",
            },
            LuaClassField {
                name: "old_path",
                ty: "string".into(),
                optional: false,
                doc: "Original path, including for a rename.",
            },
            LuaClassField {
                name: "status",
                ty: "string".into(),
                optional: false,
                doc: "Git change status.",
            },
            LuaClassField {
                name: "additions",
                ty: "integer".into(),
                optional: false,
                doc: "Added lines.",
            },
            LuaClassField {
                name: "deletions",
                ty: "integer".into(),
                optional: false,
                doc: "Removed lines.",
            },
            LuaClassField {
                name: "hl_group",
                ty: "string".into(),
                optional: false,
                doc: "Theme foreground group for the file's change kind.",
            },
            LuaClassField {
                name: "key",
                ty: "string".into(),
                optional: false,
                doc: "Raw path bytes, for stable section-local identity across snapshots. Use path for display.",
            },
            LuaClassField {
                name: "section",
                ty: "'unstaged'|'staged'".into(),
                optional: false,
                doc: "Patch section. Untracked files belong to unstaged with status ?.",
            },
            LuaClassField {
                name: "binary",
                ty: "boolean".into(),
                optional: false,
                doc: "Whether Git reports a binary patch instead of line changes.",
            },
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.diff.TreeNode",
        classification: crate::lua::doc::classification_for_type("smelt.diff.TreeNode"),
        doc: "Metadata for one visible file-tree row. File indices are one-based and tree rows are zero-based.",
        fields: vec![
            LuaClassField { name: "index", ty: "integer".into(), optional: true, doc: "File index, or nil for a directory." },
            LuaClassField { name: "first", ty: "integer".into(), optional: false, doc: "First descendant file index." },
            LuaClassField { name: "parent", ty: "integer".into(), optional: true, doc: "Visible parent directory row." },
            LuaClassField { name: "children", ty: "boolean".into(), optional: false, doc: "Whether this row is an expandable directory. Section headings are fixed." },
            LuaClassField { name: "section", ty: "'unstaged'|'staged'".into(), optional: false, doc: "Section containing this row." },
            LuaClassField { name: "group", ty: "boolean".into(), optional: false, doc: "Whether this is a section header." },
            LuaClassField { name: "expanded", ty: "boolean".into(), optional: false, doc: "Whether the directory is expanded. Always true for section headings." },
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.diff.Tree",
        classification: crate::lua::doc::classification_for_type("smelt.diff.Tree"),
        doc: "Independent virtual file sidebar sharing immutable file metadata and ordering. Each tree owns its collapsed directories; each attached window supplies its own width. Only requested rows are formatted; folder toggles rebuild compact visible-node indices, not offscreen strings or spans.",
        fields: vec![
            LuaClassField { name: "document", ty: "fun(self: smelt.diff.Tree): smelt.document.Document".into(), optional: false, doc: "Row source for a window. Includes neutral filenames, colored status letters, folder triangles, and green/red counts immediately after file and section labels, separated by single spaces." },
            LuaClassField { name: "width", ty: "fun(self: smelt.diff.Tree, width: integer)".into(), optional: false, doc: "Set fallback content width for row access outside a window. Attached windows use their own content width automatically." },
            LuaClassField { name: "node", ty: "fun(self: smelt.diff.Tree, row: integer): smelt.diff.TreeNode?".into(), optional: false, doc: "Metadata for one visible row, or nil for the blank section separator or an invalid row. Constant-time lookup." },
            LuaClassField { name: "file_row", ty: "fun(self: smelt.diff.Tree, index: integer, reveal: boolean?): integer?".into(), optional: false, doc: "Visible row of a file, or nil when hidden. With reveal=true, expand its ancestors first. Logarithmic lookup when no folders change." },
            LuaClassField { name: "toggle", ty: "fun(self: smelt.diff.Tree, row: integer): integer?".into(), optional: false, doc: "Toggle a directory and return its row. Files, section headings, the blank separator and invalid rows return nil." },
            LuaClassField { name: "collapsed", ty: "fun(self: smelt.diff.Tree): string[]".into(), optional: false, doc: "Opaque collapsed directory keys for restoring a new snapshot." },
            LuaClassField { name: "section_row", ty: "fun(self: smelt.diff.Tree, section: 'unstaged'|'staged'): integer".into(), optional: false, doc: "Visible row of a section header, including an empty section." },
        ],
    });
    record_class(LuaClassDecl {
        name: "smelt.diff.Diff",
        classification: crate::lua::doc::classification_for_type("smelt.diff.Diff"),
        doc: "Indexed unified patch with a continuous foldable row projection. File indices are one-based; display rows are zero-based. No patch line tables cross into Lua.",
        fields: vec![
            LuaClassField { name: "document", ty: "fun(self: smelt.diff.Diff): smelt.document.Document".into(), optional: false, doc: "Row source for `win:document`. Native storage is shared, not copied." },
            LuaClassField { name: "view", ty: "fun(self: smelt.diff.Diff): smelt.diff.Diff".into(), optional: false, doc: "Create an independent view, initially preserving the current folds. Shares immutable patch storage, metadata and syntax caches, not subsequent fold changes. Each attached window submits independent syntax demand." },
            LuaClassField { name: "tree", ty: "fun(self: smelt.diff.Diff, collapsed: string[]?): smelt.diff.Tree".into(), optional: false, doc: "Create an independent virtual file tree. Optionally restore opaque collapsed keys from an earlier tree. Section headers include file counts and section-local line totals; attached windows provide their own width." },
            LuaClassField { name: "restore", ty: "fun(self: smelt.diff.Diff, previous: smelt.diff.Diff, cursor: integer, top: integer): integer?, integer?".into(), optional: false, doc: "Restore expanded context and return cursor/top rows for a refreshed snapshot, matching section, raw path and source line numbers rather than display offsets. A missing file returns nil for its anchor. Does not change the previous view." },
            LuaClassField { name: "files", ty: "fun(self: smelt.diff.Diff): smelt.diff.File[]".into(), optional: false, doc: "All file metadata, grouped by section and directory-first within each section. Prefer file(index) for bounded UI-thread allocation." },
            LuaClassField { name: "file_count", ty: "fun(self: smelt.diff.Diff): integer".into(), optional: false, doc: "Number of section-local file entries. A partial file counts twice." },
            LuaClassField { name: "file", ty: "fun(self: smelt.diff.Diff, index: integer): smelt.diff.File?".into(), optional: false, doc: "Metadata for one file entry. Constant-time lookup." },
            LuaClassField { name: "find_file", ty: "fun(self: smelt.diff.Diff, section: 'unstaged'|'staged', key: string): integer?".into(), optional: false, doc: "Find a file by section and raw path bytes when restoring selection in a new snapshot." },
            LuaClassField { name: "section_range", ty: "fun(self: smelt.diff.Diff, section: 'unstaged'|'staged'): integer, integer".into(), optional: false, doc: "Inclusive first and last file indices for a section. First exceeds last when the section is empty." },
            LuaClassField { name: "file_row", ty: "fun(self: smelt.diff.Diff, index: integer): integer?".into(), optional: false, doc: "Current display row of a file header. Accounts for expanded folds." },
            LuaClassField { name: "file_at", ty: "fun(self: smelt.diff.Diff, row: integer): integer?".into(), optional: false, doc: "File containing a display row. Logarithmic lookup." },
            LuaClassField { name: "hunk", ty: "fun(self: smelt.diff.Diff, row: integer, forward: boolean): integer?".into(), optional: false, doc: "Next/previous change group, strictly after/before row. Full-context Git patches retain navigation between edits separated by unchanged lines." },
            LuaClassField { name: "toggle_fold", ty: "fun(self: smelt.diff.Diff, row: integer): integer?".into(), optional: false, doc: "Toggle the context fold containing row. Returns the fold marker's row, or nil. Does not reparse the patch." },
        ],
    });
    let parse_shared = Arc::clone(shared);
    m.fn_("parse", "Index Git patch rows and folds without tokenizing source. `context` defaults to 3 visible lines at each edge of an unchanged run. Row indexing is linear and synchronous; use `smelt.git.diff` for asynchronous repository acquisition. Syntax and inline highlights are computed on demand in a background worker with bounded caches and old/new parser checkpoints. Each visible window submits its own demand; copying and searching only read cached results. Inline results publish before distant syntax seeks; large replacement blocks compare only viewport pairs. The first 128-256 rows of the previous and next file are prefetched after visible work, without keeping the UI repainting. Text and navigation are immediately available; highlights appear when ready without blocking a frame.", &["patch", "context"],
        move |_, (patch, context): (String, Option<usize>)| -> LuaResult<LuaDiff> {
            let view = Arc::new(DiffView::new(Arc::new(Diff::parse(patch)), context.unwrap_or(3)));
            view.set_wakeup(parse_shared.wakeup_tx.get().cloned());
            Ok(LuaDiff(view, None))
        })?;
    let git = LuaMod::advanced(lua, smelt, "git", "Asynchronous Git snapshots and explicit file-level staging. `git.diff(opts?)` yields separate unstaged (index to worktree, including untracked files) and staged (HEAD to index) patches. Unborn repositories use the empty tree for HEAD. `git.index(diff, file, action)` updates the selected file's real index entries, then acquires and indexes a fresh grouped snapshot off-thread. Mutation success is explicit (`index_updated=true`); if only the refresh fails, the result contains `refresh_error` and no `diff`. Retain the old snapshot as stale and refresh before another mutation. The original snapshot remains immutable.", Tier::Host)?;
    let index_shared = Arc::clone(shared);
    let shared = Arc::clone(shared);
    git.private_live_only_fn(
        "__start_diff",
        &["task_id", "opts"],
        move |_, (id, opts): (u64, Option<mlua::Table>)| -> LuaResult<LuaDiffJob> {
            let cwd = opts
                .as_ref()
                .map(|opts| opts.get::<Option<String>>("cwd"))
                .transpose()?
                .flatten()
                .map(|path| shared.resolve_project_path(path))
                .unwrap_or_else(|| shared.evaluation_cwd());
            let context = opts
                .as_ref()
                .map(|opts| opts.get::<Option<usize>>("context"))
                .transpose()?
                .flatten()
                .unwrap_or(3);
            let result: DiffResult = Arc::new(Mutex::new(None));
            let output = Arc::clone(&result);
            let cancel = crate::lua::current_task_cancel().unwrap_or_default();
            let sink = shared.resume_sink();
            let wakeup = shared.wakeup_tx.get().cloned();
            tokio::spawn(async move {
                let value = crate::git::working_diff(cwd, context, cancel).await;
                if let Ok(snapshot) = &value {
                    snapshot.view.set_wakeup(wakeup);
                }
                *output.lock().unwrap() = Some(value.map(GitResult::Snapshot));
                sink.resolve_json(id, serde_json::Value::Null);
            });
            Ok(LuaDiffJob(result))
        },
    )?;
    git.private_live_only_fn(
        "__start_index",
        &["task_id", "diff", "file", "action"],
        move |_,
              (id, diff, index, action): (u64, LuaDiff, usize, String)|
              -> LuaResult<LuaDiffJob> {
            let repository =
                Arc::clone(diff.1.as_ref().ok_or_else(|| {
                    LuaError::external("index operations require a Git snapshot")
                })?);
            let file = index
                .checked_sub(1)
                .and_then(|i| diff.0.diff.files.get(i))
                .cloned()
                .ok_or_else(|| LuaError::external("invalid diff file index"))?;
            let action = match action.as_str() {
                "stage" => crate::git::IndexAction::Stage,
                "unstage" => crate::git::IndexAction::Unstage,
                "toggle" => crate::git::IndexAction::Toggle,
                _ => {
                    return Err(LuaError::external(
                        "index action must be stage, unstage, or toggle",
                    ))
                }
            };
            let context = diff.0.context();
            let result: DiffResult = Arc::new(Mutex::new(None));
            let output = Arc::clone(&result);
            let cancel = crate::lua::current_task_cancel().unwrap_or_default();
            let sink = index_shared.resume_sink();
            let wakeup = index_shared.wakeup_tx.get().cloned();
            tokio::spawn(async move {
                let mutation = tokio::time::timeout(std::time::Duration::from_secs(120),
                    crate::git::update_index(repository.root.clone(), file, action, cancel.clone()))
                    .await.unwrap_or_else(|_| Err("git index operation timed out after 120 seconds; refresh to verify repository state".into()));
                let value = match mutation {
                    Err(error) => Err(error),
                    Ok(()) => {
                        let refresh = crate::git::working_diff(repository.root.clone(), context, cancel).await;
                        if let Ok(snapshot) = &refresh { snapshot.view.set_wakeup(wakeup); }
                        Ok(GitResult::IndexUpdated { root: repository.root.clone(), refresh })
                    }
                };
                *output.lock().unwrap() = Some(value);
                sink.resolve_json(id, serde_json::Value::Null);
            });
            Ok(LuaDiffJob(result))
        },
    )?;
    Ok(())
}
