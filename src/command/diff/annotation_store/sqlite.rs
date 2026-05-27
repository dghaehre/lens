use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, params_from_iter, Connection};

use super::super::types::DiffPanelFocus;
use super::{
    is_local_origin, Annotation, AnnotationError, AnnotationId, AnnotationStore, AnnotationTarget,
    Author, Capabilities, Comment, NewAnnotation, NewReply,
};

/// Identifier the trait reports for this store. It's deliberately *not* the
/// origin of any particular annotation — local annotations carry one of
/// "ui", "cli", or "agent" depending on where they were created.
const STORE_ID: &str = "local";

/// Bump when the on-disk schema changes. A DB stamped with a different
/// version triggers a hard error in `open` — see the version check there.
const SCHEMA_VERSION: u32 = 2;

const SCHEMA_V2: &str = "
CREATE TABLE IF NOT EXISTS annotations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    filename    TEXT    NOT NULL,
    target_kind TEXT    NOT NULL CHECK (target_kind IN ('file', 'line_range')),
    panel       TEXT             CHECK (panel IS NULL OR panel IN ('old', 'new')),
    start_line  INTEGER,
    end_line    INTEGER,
    resolved    INTEGER NOT NULL DEFAULT 0,
    CHECK (
        (target_kind = 'file'       AND panel IS NULL     AND start_line IS NULL AND end_line IS NULL) OR
        (target_kind = 'line_range' AND panel IS NOT NULL AND start_line IS NOT NULL AND end_line IS NOT NULL)
    )
);
CREATE INDEX IF NOT EXISTS idx_annotations_filename ON annotations(filename);

CREATE TABLE IF NOT EXISTS comments (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    annotation_id INTEGER NOT NULL REFERENCES annotations(id) ON DELETE CASCADE,
    content       TEXT    NOT NULL,
    author        TEXT    NOT NULL,
    origin        TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_comments_annotation_id ON comments(annotation_id);
";

pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    pub fn open(db_path: &Path) -> Result<Self, AnnotationError> {
        let mut conn = Connection::open(db_path).map_err(backend_err)?;
        conn.pragma_update(None, "foreign_keys", "ON")
            .map_err(backend_err)?;

        let db_version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(backend_err)?;

        match db_version {
            0 => {
                conn.execute_batch(SCHEMA_V2).map_err(backend_err)?;
                conn.execute(&format!("PRAGMA user_version = {}", SCHEMA_VERSION), [])
                    .map_err(backend_err)?;
            }
            1 => migrate_v1_to_v2(&mut conn)?,
            v if v == SCHEMA_VERSION => {
                conn.execute_batch(SCHEMA_V2).map_err(backend_err)?;
            }
            other => {
                return Err(AnnotationError::Backend(format!(
                    "lens.db schema version mismatch at {}: db reports v{}, this build expects v{}. \
                     Refusing to continue to avoid data corruption.",
                    db_path.display(),
                    other,
                    SCHEMA_VERSION,
                )));
            }
        }

        Ok(Self { conn })
    }
}

/// In v1 the schema kept content/author/created_at on the `annotations` row.
/// v2 moves comment data into a separate table so threads can grow replies
/// and so each comment can record its own origin (ui/cli/agent).
fn migrate_v1_to_v2(conn: &mut Connection) -> Result<(), AnnotationError> {
    let tx = conn.transaction().map_err(backend_err)?;

    tx.execute_batch(
        "
        DROP INDEX IF EXISTS idx_annotations_filename;
        ALTER TABLE annotations RENAME TO annotations_v1;

        CREATE TABLE annotations (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            filename    TEXT    NOT NULL,
            target_kind TEXT    NOT NULL CHECK (target_kind IN ('file', 'line_range')),
            panel       TEXT             CHECK (panel IS NULL OR panel IN ('old', 'new')),
            start_line  INTEGER,
            end_line    INTEGER,
            resolved    INTEGER NOT NULL DEFAULT 0,
            CHECK (
                (target_kind = 'file'       AND panel IS NULL     AND start_line IS NULL AND end_line IS NULL) OR
                (target_kind = 'line_range' AND panel IS NOT NULL AND start_line IS NOT NULL AND end_line IS NOT NULL)
            )
        );
        CREATE INDEX idx_annotations_filename ON annotations(filename);

        CREATE TABLE comments (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            annotation_id INTEGER NOT NULL REFERENCES annotations(id) ON DELETE CASCADE,
            content       TEXT    NOT NULL,
            author        TEXT    NOT NULL,
            origin        TEXT    NOT NULL,
            created_at    INTEGER NOT NULL
        );
        CREATE INDEX idx_comments_annotation_id ON comments(annotation_id);

        INSERT INTO annotations (id, filename, target_kind, panel, start_line, end_line, resolved)
        SELECT id, filename, target_kind, panel, start_line, end_line, resolved
        FROM annotations_v1;

        INSERT INTO comments (annotation_id, content, author, origin, created_at)
        SELECT id, content, author, 'ui', created_at
        FROM annotations_v1;

        DROP TABLE annotations_v1;
        ",
    )
    .map_err(backend_err)?;

    tx.execute(&format!("PRAGMA user_version = {}", SCHEMA_VERSION), [])
        .map_err(backend_err)?;
    tx.commit().map_err(backend_err)?;
    Ok(())
}

impl AnnotationStore for SqliteStore {
    fn id(&self) -> &str {
        STORE_ID
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    fn list(&self) -> Result<Vec<Annotation>, AnnotationError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT id, filename, target_kind, panel, start_line, end_line, resolved \
                 FROM annotations ORDER BY id",
            )
            .map_err(backend_err)?;
        let metas = stmt
            .query_map([], row_to_meta)
            .map_err(backend_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend_err)?;
        load_with_comments(&self.conn, metas)
    }

    fn list_for_files(&self, filenames: &[String]) -> Result<Vec<Annotation>, AnnotationError> {
        if filenames.is_empty() {
            return Ok(Vec::new());
        }
        let placeholders = std::iter::repeat("?")
            .take(filenames.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, filename, target_kind, panel, start_line, end_line, resolved \
             FROM annotations WHERE filename IN ({}) ORDER BY id",
            placeholders
        );
        let mut stmt = self.conn.prepare(&sql).map_err(backend_err)?;
        let metas = stmt
            .query_map(params_from_iter(filenames.iter()), row_to_meta)
            .map_err(backend_err)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(backend_err)?;
        load_with_comments(&self.conn, metas)
    }

    fn create(&mut self, draft: NewAnnotation) -> Result<Annotation, AnnotationError> {
        let (target_kind, panel, start_line, end_line) = match &draft.target {
            AnnotationTarget::File => ("file", None::<&str>, None::<i64>, None::<i64>),
            AnnotationTarget::LineRange {
                panel,
                start_line,
                end_line,
            } => (
                "line_range",
                Some(panel_to_str(*panel)),
                Some(*start_line as i64),
                Some(*end_line as i64),
            ),
        };
        let author = Author::for_origin(&draft.origin);
        let created_at_secs = duration_to_secs(draft.created_at);

        let tx = self.conn.transaction().map_err(backend_err)?;

        tx.execute(
            "INSERT INTO annotations (filename, target_kind, panel, start_line, end_line, resolved) \
             VALUES (?, ?, ?, ?, ?, 0)",
            params![draft.filename, target_kind, panel, start_line, end_line],
        )
        .map_err(backend_err)?;
        let annotation_row_id = tx.last_insert_rowid();

        tx.execute(
            "INSERT INTO comments (annotation_id, content, author, origin, created_at) \
             VALUES (?, ?, ?, ?, ?)",
            params![
                annotation_row_id,
                draft.content,
                author.login,
                draft.origin,
                created_at_secs,
            ],
        )
        .map_err(backend_err)?;
        let comment_row_id = tx.last_insert_rowid();

        tx.commit().map_err(backend_err)?;

        let id = AnnotationId {
            origin: draft.origin.clone(),
            native: annotation_row_id.to_string(),
        };
        let root = Comment {
            native_id: Some(comment_row_id.to_string()),
            author,
            origin: draft.origin,
            content: draft.content,
            created_at: draft.created_at,
        };
        Ok(Annotation {
            id,
            filename: draft.filename,
            target: draft.target,
            comments: vec![root],
            resolved: false,
        })
    }

    fn update(&mut self, id: &AnnotationId, content: String) -> Result<(), AnnotationError> {
        if !is_local_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        let annotation_row_id = parse_native(&id.native)?;
        let changed = self
            .conn
            .execute(
                "UPDATE comments SET content = ? \
                 WHERE id = (SELECT id FROM comments WHERE annotation_id = ? ORDER BY id LIMIT 1)",
                params![content, annotation_row_id],
            )
            .map_err(backend_err)?;
        if changed == 0 {
            Err(AnnotationError::NotFound)
        } else {
            Ok(())
        }
    }

    fn reply(
        &mut self,
        id: &AnnotationId,
        reply: NewReply,
    ) -> Result<Annotation, AnnotationError> {
        if !is_local_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        let annotation_row_id = parse_native(&id.native)?;

        let exists: i64 = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM annotations WHERE id = ?",
                params![annotation_row_id],
                |row| row.get(0),
            )
            .map_err(backend_err)?;
        if exists == 0 {
            return Err(AnnotationError::NotFound);
        }

        let author = Author::for_origin(&reply.origin);
        let created_at_secs = duration_to_secs(reply.created_at);
        self.conn
            .execute(
                "INSERT INTO comments (annotation_id, content, author, origin, created_at) \
                 VALUES (?, ?, ?, ?, ?)",
                params![
                    annotation_row_id,
                    reply.content,
                    author.login,
                    reply.origin,
                    created_at_secs,
                ],
            )
            .map_err(backend_err)?;

        get_annotation(&self.conn, annotation_row_id)
    }

    fn set_resolved(
        &mut self,
        id: &AnnotationId,
        resolved: bool,
    ) -> Result<(), AnnotationError> {
        if !is_local_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        let row_id = parse_native(&id.native)?;
        let value: i64 = if resolved { 1 } else { 0 };
        let changed = self
            .conn
            .execute(
                "UPDATE annotations SET resolved = ? WHERE id = ?",
                params![value, row_id],
            )
            .map_err(backend_err)?;
        if changed == 0 {
            Err(AnnotationError::NotFound)
        } else {
            Ok(())
        }
    }

    fn delete(&mut self, id: &AnnotationId) -> Result<(), AnnotationError> {
        if !is_local_origin(&id.origin) {
            return Err(AnnotationError::WrongOrigin);
        }
        let row_id = parse_native(&id.native)?;
        let changed = self
            .conn
            .execute("DELETE FROM annotations WHERE id = ?", params![row_id])
            .map_err(backend_err)?;
        if changed == 0 {
            Err(AnnotationError::NotFound)
        } else {
            Ok(())
        }
    }
}

/// Internal: lightweight metadata pulled from the `annotations` table; comments
/// are joined in via a second query so list operations stay O(annotations + comments).
struct AnnotationMeta {
    id: i64,
    filename: String,
    target: AnnotationTarget,
    resolved: bool,
}

fn row_to_meta(row: &rusqlite::Row) -> rusqlite::Result<AnnotationMeta> {
    let id: i64 = row.get(0)?;
    let filename: String = row.get(1)?;
    let target_kind: String = row.get(2)?;
    let panel: Option<String> = row.get(3)?;
    let start_line: Option<i64> = row.get(4)?;
    let end_line: Option<i64> = row.get(5)?;
    let resolved: i64 = row.get(6)?;

    let target = match target_kind.as_str() {
        "file" => AnnotationTarget::File,
        "line_range" => AnnotationTarget::LineRange {
            panel: panel.as_deref().map(str_to_panel).unwrap_or(DiffPanelFocus::New),
            start_line: start_line.unwrap_or(0) as usize,
            end_line: end_line.unwrap_or(0) as usize,
        },
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Text,
                format!("unknown target_kind: {}", other).into(),
            ));
        }
    };

    Ok(AnnotationMeta {
        id,
        filename,
        target,
        resolved: resolved != 0,
    })
}

/// Hydrate a batch of annotation metadata rows with their comments in a
/// single query, preserving the input order.
fn load_with_comments(
    conn: &Connection,
    metas: Vec<AnnotationMeta>,
) -> Result<Vec<Annotation>, AnnotationError> {
    if metas.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = std::iter::repeat("?")
        .take(metas.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT id, annotation_id, content, author, origin, created_at \
         FROM comments WHERE annotation_id IN ({}) ORDER BY annotation_id, id",
        placeholders
    );
    let ids: Vec<i64> = metas.iter().map(|m| m.id).collect();
    let mut stmt = conn.prepare(&sql).map_err(backend_err)?;
    let mut rows = stmt.query(params_from_iter(ids.iter())).map_err(backend_err)?;

    let mut grouped: std::collections::HashMap<i64, Vec<Comment>> = std::collections::HashMap::new();
    while let Some(row) = rows.next().map_err(backend_err)? {
        let comment_id: i64 = row.get(0).map_err(backend_err)?;
        let annotation_id: i64 = row.get(1).map_err(backend_err)?;
        let content: String = row.get(2).map_err(backend_err)?;
        let author_login: String = row.get(3).map_err(backend_err)?;
        let origin: String = row.get(4).map_err(backend_err)?;
        let created_at_secs: i64 = row.get(5).map_err(backend_err)?;
        grouped.entry(annotation_id).or_default().push(Comment {
            native_id: Some(comment_id.to_string()),
            author: Author {
                login: author_login,
                display_name: None,
            },
            origin,
            content,
            created_at: secs_to_systime(created_at_secs),
        });
    }

    let mut out = Vec::with_capacity(metas.len());
    for meta in metas {
        let comments = grouped.remove(&meta.id).unwrap_or_default();
        if comments.is_empty() {
            // Annotation row without comments is a data invariant violation,
            // but we'd rather surface it than crash.
            return Err(AnnotationError::Backend(format!(
                "annotation {} has no comments",
                meta.id
            )));
        }
        let root_origin = comments[0].origin.clone();
        out.push(Annotation {
            id: AnnotationId {
                origin: root_origin,
                native: meta.id.to_string(),
            },
            filename: meta.filename,
            target: meta.target,
            comments,
            resolved: meta.resolved,
        });
    }
    Ok(out)
}

fn get_annotation(conn: &Connection, annotation_row_id: i64) -> Result<Annotation, AnnotationError> {
    let mut stmt = conn
        .prepare(
            "SELECT id, filename, target_kind, panel, start_line, end_line, resolved \
             FROM annotations WHERE id = ?",
        )
        .map_err(backend_err)?;
    let meta = stmt
        .query_row(params![annotation_row_id], row_to_meta)
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => AnnotationError::NotFound,
            other => backend_err(other),
        })?;
    let mut anns = load_with_comments(conn, vec![meta])?;
    anns.pop().ok_or(AnnotationError::NotFound)
}

fn backend_err(e: rusqlite::Error) -> AnnotationError {
    AnnotationError::Backend(e.to_string())
}

fn parse_native(s: &str) -> Result<i64, AnnotationError> {
    s.parse::<i64>().map_err(|_| AnnotationError::NotFound)
}

fn panel_to_str(panel: DiffPanelFocus) -> &'static str {
    match panel {
        DiffPanelFocus::Old => "old",
        DiffPanelFocus::New | DiffPanelFocus::None => "new",
    }
}

fn str_to_panel(s: &str) -> DiffPanelFocus {
    match s {
        "old" => DiffPanelFocus::Old,
        _ => DiffPanelFocus::New,
    }
}

fn duration_to_secs(t: SystemTime) -> i64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn secs_to_systime(secs: i64) -> SystemTime {
    if secs < 0 {
        UNIX_EPOCH
    } else {
        UNIX_EPOCH + Duration::from_secs(secs as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_temp() -> (TempDir, SqliteStore) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lens.db");
        let store = SqliteStore::open(&path).unwrap();
        (dir, store)
    }

    fn draft_file(filename: &str, content: &str, origin: &str) -> NewAnnotation {
        NewAnnotation {
            filename: filename.to_string(),
            target: AnnotationTarget::File,
            content: content.to_string(),
            origin: origin.to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        }
    }

    fn draft_line(filename: &str, content: &str, origin: &str) -> NewAnnotation {
        NewAnnotation {
            filename: filename.to_string(),
            target: AnnotationTarget::LineRange {
                panel: DiffPanelFocus::New,
                start_line: 10,
                end_line: 12,
            },
            content: content.to_string(),
            origin: origin.to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        }
    }

    #[test]
    fn create_then_list_roundtrip() {
        let (_dir, mut store) = open_temp();
        let a = store.create(draft_file("a.rs", "hello", "ui")).unwrap();
        let b = store.create(draft_line("b.rs", "world", "agent")).unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, a.id);
        assert_eq!(all[0].id.origin, "ui");
        assert_eq!(all[0].filename, "a.rs");
        assert!(matches!(all[0].target, AnnotationTarget::File));
        assert_eq!(all[0].root().content, "hello");
        assert_eq!(all[0].root().origin, "ui");
        assert_eq!(all[1].id, b.id);
        assert_eq!(all[1].id.origin, "agent");
        assert_eq!(all[1].root().origin, "agent");
        match &all[1].target {
            AnnotationTarget::LineRange {
                panel,
                start_line,
                end_line,
            } => {
                assert!(matches!(panel, DiffPanelFocus::New));
                assert_eq!(*start_line, 10);
                assert_eq!(*end_line, 12);
            }
            _ => panic!("expected LineRange"),
        }
    }

    #[test]
    fn update_changes_content() {
        let (_dir, mut store) = open_temp();
        let a = store.create(draft_file("a.rs", "first", "ui")).unwrap();
        store.update(&a.id, "second".to_string()).unwrap();
        let all = store.list().unwrap();
        assert_eq!(all[0].root().content, "second");
    }

    #[test]
    fn set_resolved_toggles_and_persists() {
        let (_dir, mut store) = open_temp();
        let a = store.create(draft_file("a.rs", "x", "ui")).unwrap();
        assert!(!store.list().unwrap()[0].resolved);

        store.set_resolved(&a.id, true).unwrap();
        assert!(store.list().unwrap()[0].resolved);

        store.set_resolved(&a.id, false).unwrap();
        assert!(!store.list().unwrap()[0].resolved);
    }

    #[test]
    fn set_resolved_unknown_id_is_not_found() {
        let (_dir, mut store) = open_temp();
        let bogus = AnnotationId {
            origin: "ui".to_string(),
            native: "9999".to_string(),
        };
        match store.set_resolved(&bogus, true) {
            Err(AnnotationError::NotFound) => {}
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn update_unknown_id_is_not_found() {
        let (_dir, mut store) = open_temp();
        let bogus = AnnotationId {
            origin: "ui".to_string(),
            native: "9999".to_string(),
        };
        match store.update(&bogus, "x".to_string()) {
            Err(AnnotationError::NotFound) => {}
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn delete_removes_row_and_cascades_comments() {
        let (_dir, mut store) = open_temp();
        let a = store.create(draft_file("a.rs", "x", "ui")).unwrap();
        store
            .reply(
                &a.id,
                NewReply {
                    content: "reply".to_string(),
                    origin: "agent".to_string(),
                    created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_010),
                },
            )
            .unwrap();
        store.delete(&a.id).unwrap();
        assert_eq!(store.list().unwrap().len(), 0);
        let orphan_count: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM comments", [], |row| row.get(0))
            .unwrap();
        assert_eq!(orphan_count, 0);
    }

    #[test]
    fn wrong_origin_rejected() {
        let (_dir, mut store) = open_temp();
        let foreign = AnnotationId {
            origin: "github".to_string(),
            native: "1".to_string(),
        };
        assert!(matches!(
            store.update(&foreign, "x".to_string()),
            Err(AnnotationError::WrongOrigin)
        ));
        assert!(matches!(
            store.delete(&foreign),
            Err(AnnotationError::WrongOrigin)
        ));
        assert!(matches!(
            store.reply(
                &foreign,
                NewReply {
                    content: "r".to_string(),
                    origin: "agent".to_string(),
                    created_at: SystemTime::UNIX_EPOCH,
                },
            ),
            Err(AnnotationError::WrongOrigin)
        ));
    }

    #[test]
    fn reply_appends_to_thread() {
        let (_dir, mut store) = open_temp();
        let a = store.create(draft_file("a.rs", "first", "ui")).unwrap();
        let updated = store
            .reply(
                &a.id,
                NewReply {
                    content: "reply!".to_string(),
                    origin: "agent".to_string(),
                    created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_100),
                },
            )
            .unwrap();
        assert_eq!(updated.comments.len(), 2);
        assert_eq!(updated.comments[0].content, "first");
        assert_eq!(updated.comments[0].origin, "ui");
        assert_eq!(updated.comments[1].content, "reply!");
        assert_eq!(updated.comments[1].origin, "agent");
        assert_eq!(updated.id.origin, "ui");
    }

    #[test]
    fn reply_to_unknown_id_is_not_found() {
        let (_dir, mut store) = open_temp();
        let bogus = AnnotationId {
            origin: "ui".to_string(),
            native: "9999".to_string(),
        };
        let err = store.reply(
            &bogus,
            NewReply {
                content: "r".to_string(),
                origin: "agent".to_string(),
                created_at: SystemTime::UNIX_EPOCH,
            },
        );
        assert!(matches!(err, Err(AnnotationError::NotFound)));
    }

    #[test]
    fn list_for_files_filters() {
        let (_dir, mut store) = open_temp();
        store.create(draft_file("a.rs", "A", "ui")).unwrap();
        store.create(draft_file("b.rs", "B", "ui")).unwrap();
        store.create(draft_file("c.rs", "C", "ui")).unwrap();

        let only_b = store
            .list_for_files(&["b.rs".to_string()])
            .unwrap();
        assert_eq!(only_b.len(), 1);
        assert_eq!(only_b[0].filename, "b.rs");

        let a_and_c = store
            .list_for_files(&["a.rs".to_string(), "c.rs".to_string()])
            .unwrap();
        assert_eq!(a_and_c.len(), 2);

        let none = store.list_for_files(&[]).unwrap();
        assert_eq!(none.len(), 0);
    }

    #[test]
    fn persists_across_reopen() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lens.db");
        {
            let mut store = SqliteStore::open(&path).unwrap();
            let a = store.create(draft_file("a.rs", "persistent", "ui")).unwrap();
            store
                .reply(
                    &a.id,
                    NewReply {
                        content: "and a reply".to_string(),
                        origin: "agent".to_string(),
                        created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_100),
                    },
                )
                .unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        let all = store.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].comments.len(), 2);
        assert_eq!(all[0].root().content, "persistent");
        assert_eq!(all[0].comments[1].content, "and a reply");
        assert_eq!(all[0].comments[1].origin, "agent");
    }

    /// Simulates an existing v1 DB by writing the v1 schema + a row directly,
    /// then opens with the v2 code path and asserts the migration ran cleanly.
    #[test]
    fn migrates_v1_database_to_v2() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lens.db");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(
                "
                CREATE TABLE annotations (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT,
                    filename    TEXT    NOT NULL,
                    target_kind TEXT    NOT NULL,
                    panel       TEXT,
                    start_line  INTEGER,
                    end_line    INTEGER,
                    content     TEXT    NOT NULL,
                    author      TEXT    NOT NULL,
                    created_at  INTEGER NOT NULL,
                    resolved    INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX idx_annotations_filename ON annotations(filename);
                INSERT INTO annotations (filename, target_kind, panel, start_line, end_line, content, author, created_at, resolved)
                VALUES ('old.rs', 'file', NULL, NULL, NULL, 'pre-migration', 'you', 1700000000, 0);
                PRAGMA user_version = 1;
                ",
            )
            .unwrap();
        }

        let store = SqliteStore::open(&path).unwrap();
        let all = store.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].filename, "old.rs");
        assert_eq!(all[0].root().content, "pre-migration");
        assert_eq!(all[0].root().origin, "ui");
        assert_eq!(all[0].id.origin, "ui");
    }
}
