//! Scripting surface for annotations.
//!
//! These handlers open the per-repo SQLite store and emit JSON to stdout so
//! agents (and humans piping into `jq`) can read and write annotations
//! without entering the TUI.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::command::diff::annotation_store::{
    Annotation, AnnotationError, AnnotationId, AnnotationStore, AnnotationTarget, Comment,
    NewAnnotation, NewReply, SqliteStore,
};
use crate::command::diff::types::DiffPanelFocus;
use crate::config::cli::{
    AnnotationCommand, AnnotationDeleteArgs, AnnotationListArgs, AnnotationNewArgs,
    AnnotationReplyArgs, PanelArg,
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
        AnnotationCommand::Delete(args) => delete(&mut store, args),
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
