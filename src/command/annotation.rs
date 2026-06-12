//! Scripting surface for annotations.
//!
//! These handlers open the per-repo SQLite store and emit JSON to stdout so
//! agents (and humans piping into `jq`) can read and write annotations
//! without entering the TUI.

use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notify_debouncer_mini::{new_debouncer, DebouncedEventKind};
use serde_json::{json, Value};

use crate::command::diff::annotation_store::{
    Annotation, AnnotationError, AnnotationId, AnnotationStore, AnnotationTarget, Comment,
    NewAnnotation, NewReply, SqliteStore,
};
use crate::command::diff::types::DiffPanelFocus;
use crate::config::cli::{
    AnnotationCommand, AnnotationDeleteArgs, AnnotationListArgs, AnnotationNewArgs,
    AnnotationReplyArgs, AnnotationResolveArgs, AnnotationWatchArgs, PanelArg,
};
use crate::error::LensError;
use crate::vcs::GitBackend;

pub fn run(command: AnnotationCommand, backend: &GitBackend) -> Result<(), LensError> {
    let db_path = backend.git_dir().join("lens.db");
    let mut store = open_store(&db_path)?;
    match command {
        AnnotationCommand::List(args) => list(&store, args),
        AnnotationCommand::New(args) => new(&mut store, args),
        AnnotationCommand::Reply(args) => reply(&mut store, args),
        AnnotationCommand::Resolve(args) => resolve(&mut store, args),
        AnnotationCommand::Delete(args) => delete(&mut store, args),
        AnnotationCommand::Watch(args) => watch(&db_path, args),
    }
}

fn open_store(db_path: &Path) -> Result<SqliteStore, LensError> {
    SqliteStore::open(db_path).map_err(annotation_err)
}

fn list(store: &SqliteStore, args: AnnotationListArgs) -> Result<(), LensError> {
    let annotations: Vec<Annotation> = match (&args.file, args.id) {
        (Some(files), _) => store.list_for_files(files).map_err(annotation_err)?,
        (None, _) => store.list().map_err(annotation_err)?,
    };
    let filtered: Vec<&Annotation> = match args.id {
        Some(id) => annotations
            .iter()
            .filter(|a| a.id.native == id.to_string())
            .collect(),
        None => annotations.iter().collect(),
    };
    let arr: Vec<Value> = filtered.iter().map(|a| annotation_to_json(a)).collect();
    println!("{}", serde_json::to_string_pretty(&Value::Array(arr))?);
    Ok(())
}

fn new(store: &mut SqliteStore, args: AnnotationNewArgs) -> Result<(), LensError> {
    let target = build_target(args.line, args.line_end, args.panel)?;
    let draft = NewAnnotation {
        filename: args.file,
        target,
        content: args.content,
        origin: args.source,
        created_at: SystemTime::now(),
    };
    let annotation = store.create(draft).map_err(annotation_err)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&annotation_to_json(&annotation))?
    );
    Ok(())
}

fn reply(store: &mut SqliteStore, args: AnnotationReplyArgs) -> Result<(), LensError> {
    // The CLI takes a bare integer id; we don't yet know its real origin so
    // we use "ui" — the store's WrongOrigin check accepts any local origin.
    let id = AnnotationId {
        origin: "ui".to_string(),
        native: args.id.to_string(),
    };
    let reply = NewReply {
        content: args.content,
        origin: args.source,
        created_at: SystemTime::now(),
    };
    let updated = store.reply(&id, reply).map_err(annotation_err)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&annotation_to_json(&updated))?
    );
    Ok(())
}

fn resolve(store: &mut SqliteStore, args: AnnotationResolveArgs) -> Result<(), LensError> {
    let id = AnnotationId {
        origin: "ui".to_string(),
        native: args.id.to_string(),
    };
    let resolved = !args.unresolve;
    store.set_resolved(&id, resolved).map_err(annotation_err)?;
    let updated = store
        .list()
        .map_err(annotation_err)?
        .into_iter()
        .find(|a| a.id.native == args.id.to_string());
    match updated {
        Some(a) => println!("{}", serde_json::to_string_pretty(&annotation_to_json(&a))?),
        None => println!("{}", json!({ "id": args.id, "resolved": resolved })),
    }
    Ok(())
}

fn delete(store: &mut SqliteStore, args: AnnotationDeleteArgs) -> Result<(), LensError> {
    let id = AnnotationId {
        origin: "ui".to_string(),
        native: args.id.to_string(),
    };
    store.delete(&id).map_err(annotation_err)?;
    println!("{}", json!({ "deleted": args.id }));
    Ok(())
}

fn build_target(
    line: Option<usize>,
    line_end: Option<usize>,
    panel: PanelArg,
) -> Result<AnnotationTarget, LensError> {
    match (line, line_end) {
        (None, None) => Ok(AnnotationTarget::File),
        (None, Some(_)) => Err(LensError::InvalidArguments(
            "--line-end requires --line".to_string(),
        )),
        (Some(start), end_opt) => {
            let end = end_opt.unwrap_or(start);
            if end < start {
                return Err(LensError::InvalidArguments(format!(
                    "--line-end ({}) must be >= --line ({})",
                    end, start
                )));
            }
            Ok(AnnotationTarget::LineRange {
                panel: panel_to_focus(panel),
                start_line: start,
                end_line: end,
            })
        }
    }
}

fn panel_to_focus(panel: PanelArg) -> DiffPanelFocus {
    match panel {
        PanelArg::Old => DiffPanelFocus::Old,
        PanelArg::New => DiffPanelFocus::New,
    }
}

fn annotation_to_json(annotation: &Annotation) -> Value {
    json!({
        "id": annotation.id.native.parse::<i64>().ok(),
        "origin": annotation.id.origin,
        "filename": annotation.filename,
        "target": target_to_json(&annotation.target),
        "resolved": annotation.resolved,
        "created_at": secs_since_epoch(annotation.created_at()),
        "comments": annotation.comments.iter().map(comment_to_json).collect::<Vec<_>>(),
    })
}

fn comment_to_json(comment: &Comment) -> Value {
    json!({
        "id": comment.native_id.as_deref().and_then(|s| s.parse::<i64>().ok()),
        "author": comment.author.login,
        "origin": comment.origin,
        "content": comment.content,
        "created_at": secs_since_epoch(comment.created_at),
    })
}

fn target_to_json(target: &AnnotationTarget) -> Value {
    match target {
        AnnotationTarget::File => json!({ "kind": "file" }),
        AnnotationTarget::LineRange {
            panel,
            start_line,
            end_line,
        } => json!({
            "kind": "line_range",
            "panel": panel_label(*panel),
            "start_line": start_line,
            "end_line": end_line,
        }),
    }
}

fn panel_label(panel: DiffPanelFocus) -> &'static str {
    match panel {
        DiffPanelFocus::Old => "old",
        DiffPanelFocus::New | DiffPanelFocus::None => "new",
    }
}

fn secs_since_epoch(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn annotation_err(e: AnnotationError) -> LensError {
    LensError::InvalidArguments(e.to_string())
}

/// Block until at least one annotation needs the agent's attention, then
/// print those threads as JSON (same shape as `list`) and exit.
///
/// "Needs attention" = unresolved AND latest comment's `origin` is not
/// `args.agent_origin`. The agent's own replies (origin "agent" by
/// default) are skipped so the watcher doesn't re-fire on its own writes.
///
/// On startup we evaluate state once and return immediately if anything's
/// already actionable. Otherwise we fs-notify the DB's parent directory
/// (matching `lens.db` and its `-wal` / `-shm` / `-journal` sidecars) and
/// re-evaluate on every debounced change.
fn watch(db_path: &Path, args: AnnotationWatchArgs) -> Result<(), LensError> {
    let agent_origin = args.agent_origin;

    // Initial state — fire immediately if there's already work pending.
    let store = open_store(db_path)?;
    let actionable = collect_actionable(&store, &agent_origin)?;
    drop(store);
    if !actionable.is_empty() {
        emit_actionable(&actionable);
        return Ok(());
    }

    let db_dir = db_path.parent().ok_or_else(|| {
        LensError::InvalidConfiguration(format!(
            "annotation db path has no parent: {}",
            db_path.display()
        ))
    })?;
    let db_file_name = db_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| {
            LensError::InvalidConfiguration(format!(
                "annotation db path has no filename: {}",
                db_path.display()
            ))
        })?
        .to_string();

    let (tx, rx) = mpsc::channel::<()>();
    let db_file_name_for_cb = db_file_name.clone();
    let mut debouncer = new_debouncer(
        Duration::from_millis(250),
        move |res: Result<Vec<notify_debouncer_mini::DebouncedEvent>, notify::Error>| {
            let Ok(events) = res else { return };
            for event in events {
                if event.kind != DebouncedEventKind::Any {
                    continue;
                }
                if matches_db_path(&event.path, &db_file_name_for_cb) {
                    let _ = tx.send(());
                    break;
                }
            }
        },
    )
    .map_err(|e| LensError::InvalidConfiguration(format!("watch setup failed: {e}")))?;

    debouncer
        .watcher()
        .watch(db_dir, notify::RecursiveMode::NonRecursive)
        .map_err(|e| LensError::InvalidConfiguration(format!("watch start failed: {e}")))?;

    loop {
        rx.recv()
            .map_err(|e| LensError::InvalidConfiguration(format!("watch channel closed: {e}")))?;
        // Drain any additional notifications that arrived during processing
        // so we only re-query once per burst.
        while rx.try_recv().is_ok() {}
        let store = open_store(db_path)?;
        let actionable = collect_actionable(&store, &agent_origin)?;
        if !actionable.is_empty() {
            emit_actionable(&actionable);
            return Ok(());
        }
    }
}

fn collect_actionable(
    store: &SqliteStore,
    agent_origin: &str,
) -> Result<Vec<Annotation>, LensError> {
    let all = store.list().map_err(annotation_err)?;
    Ok(all
        .into_iter()
        .filter(|a| !a.resolved)
        .filter(|a| {
            a.comments
                .last()
                .map(|c| c.origin != agent_origin)
                .unwrap_or(false)
        })
        .collect())
}

fn emit_actionable(annotations: &[Annotation]) {
    let arr: Vec<Value> = annotations.iter().map(annotation_to_json).collect();
    // `to_string_pretty` cannot fail for a `Value::Array`.
    println!(
        "{}",
        serde_json::to_string_pretty(&Value::Array(arr))
            .expect("serde_json::Value serialization is infallible")
    );
}

/// True if `path` is the annotation DB itself or one of its SQLite
/// sidecars (`-journal`, `-wal`, `-shm`). Compared by filename so the
/// `notify` event path's canonicalization doesn't cause false negatives.
fn matches_db_path(path: &Path, db_file_name: &str) -> bool {
    let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
        return false;
    };
    if name == db_file_name {
        return true;
    }
    if let Some(rest) = name.strip_prefix(db_file_name) {
        matches!(rest, "-journal" | "-wal" | "-shm")
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    use crate::command::diff::annotation_store::{
        Annotation, AnnotationId, AnnotationTarget, Author, Comment,
    };

    fn comment(origin: &str) -> Comment {
        Comment {
            native_id: None,
            author: Author::for_origin(origin),
            origin: origin.to_string(),
            content: String::new(),
            created_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn ann(id: &str, resolved: bool, comments: Vec<Comment>) -> Annotation {
        Annotation {
            id: AnnotationId {
                origin: "ui".to_string(),
                native: id.to_string(),
            },
            filename: "x.rs".to_string(),
            target: AnnotationTarget::File,
            comments,
            resolved,
        }
    }

    fn actionable(all: Vec<Annotation>, agent: &str) -> Vec<String> {
        all.into_iter()
            .filter(|a| !a.resolved)
            .filter(|a| {
                a.comments
                    .last()
                    .map(|c| c.origin != agent)
                    .unwrap_or(false)
            })
            .map(|a| a.id.native)
            .collect()
    }

    #[test]
    fn fresh_ui_thread_is_actionable() {
        let xs = vec![ann("1", false, vec![comment("ui")])];
        assert_eq!(actionable(xs, "agent"), vec!["1".to_string()]);
    }

    #[test]
    fn agent_last_reply_is_not_actionable() {
        let xs = vec![ann("1", false, vec![comment("ui"), comment("agent")])];
        assert!(actionable(xs, "agent").is_empty());
    }

    #[test]
    fn user_reply_after_agent_is_actionable() {
        let xs = vec![ann(
            "1",
            false,
            vec![comment("ui"), comment("agent"), comment("ui")],
        )];
        assert_eq!(actionable(xs, "agent"), vec!["1".to_string()]);
    }

    #[test]
    fn resolved_thread_is_skipped() {
        let xs = vec![ann("1", true, vec![comment("ui")])];
        assert!(actionable(xs, "agent").is_empty());
    }

    #[test]
    fn matches_db_sidecars() {
        assert!(matches_db_path(Path::new("/x/lens.db"), "lens.db"));
        assert!(matches_db_path(Path::new("/x/lens.db-wal"), "lens.db"));
        assert!(matches_db_path(Path::new("/x/lens.db-shm"), "lens.db"));
        assert!(matches_db_path(Path::new("/x/lens.db-journal"), "lens.db"));
        assert!(!matches_db_path(Path::new("/x/other"), "lens.db"));
        assert!(!matches_db_path(Path::new("/x/lens.db.bak"), "lens.db"));
    }
}
