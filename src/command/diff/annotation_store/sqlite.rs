use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{params, params_from_iter, Connection};

use super::super::types::DiffPanelFocus;
use super::{
    Annotation, AnnotationError, AnnotationId, AnnotationStore, AnnotationTarget, Author,
    Capabilities, Comment, NewAnnotation,
};

const STORE_ID: &str = "local";

/// Bump when the on-disk schema changes. A DB stamped with a different
/// version triggers a hard panic in `open` — see the version check there.
const SCHEMA_VERSION: u32 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS annotations (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    filename    TEXT    NOT NULL,
    target_kind TEXT    NOT NULL CHECK (target_kind IN ('file', 'line_range')),
    panel       TEXT             CHECK (panel IS NULL OR panel IN ('old', 'new')),
    start_line  INTEGER,
    end_line    INTEGER,
    content     TEXT    NOT NULL,
    author      TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    resolved    INTEGER NOT NULL DEFAULT 0,
    CHECK (
        (target_kind = 'file'       AND panel IS NULL     AND start_line IS NULL AND end_line IS NULL) OR
        (target_kind = 'line_range' AND panel IS NOT NULL AND start_line IS NOT NULL AND end_line IS NOT NULL)
    )
);
CREATE INDEX IF NOT EXISTS idx_annotations_filename ON annotations(filename);
";

pub struct SqliteStore {
    conn: Connection,
}

impl SqliteStore {
    pub fn open(db_path: &Path) -> Result<Self, AnnotationError> {
        let conn = Connection::open(db_path).map_err(backend_err)?;
        conn.execute_batch(SCHEMA).map_err(backend_err)?;

        let db_version: u32 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(backend_err)?;

        if db_version == 0 {
            conn.execute(
                &format!("PRAGMA user_version = {}", SCHEMA_VERSION),
                [],
            )
            .map_err(backend_err)?;
        } else {
            assert!(
                db_version == SCHEMA_VERSION,
                "lens.db schema version mismatch at {}: db reports v{}, this build expects v{}. Refusing to continue to avoid data corruption.",
                db_path.display(),
                db_version,
                SCHEMA_VERSION,
            );
        }

        Ok(Self { conn })
    }
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
                "SELECT id, filename, target_kind, panel, start_line, end_line, content, \
                 author, created_at, resolved FROM annotations ORDER BY id",
            )
            .map_err(backend_err)?;
        let rows = stmt
            .query_map([], row_to_annotation)
            .map_err(backend_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(backend_err)?);
        }
        Ok(out)
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
            "SELECT id, filename, target_kind, panel, start_line, end_line, content, \
             author, created_at, resolved FROM annotations WHERE filename IN ({}) ORDER BY id",
            placeholders
        );
        let mut stmt = self.conn.prepare(&sql).map_err(backend_err)?;
        let rows = stmt
            .query_map(params_from_iter(filenames.iter()), row_to_annotation)
            .map_err(backend_err)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(backend_err)?);
        }
        Ok(out)
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
        let author = Author::local();
        let created_at_secs = duration_to_secs(draft.created_at);
        self.conn
            .execute(
                "INSERT INTO annotations (filename, target_kind, panel, start_line, end_line, \
                 content, author, created_at, resolved) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0)",
                params![
                    draft.filename,
                    target_kind,
                    panel,
                    start_line,
                    end_line,
                    draft.content,
                    author.login,
                    created_at_secs,
                ],
            )
            .map_err(backend_err)?;
        let row_id = self.conn.last_insert_rowid();
        let id = AnnotationId {
            origin: STORE_ID.to_string(),
            native: row_id.to_string(),
        };
        let root = Comment {
            author,
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
        if id.origin != STORE_ID {
            return Err(AnnotationError::WrongOrigin);
        }
        let row_id = parse_native(&id.native)?;
        let changed = self
            .conn
            .execute(
                "UPDATE annotations SET content = ? WHERE id = ?",
                params![content, row_id],
            )
            .map_err(backend_err)?;
        if changed == 0 {
            Err(AnnotationError::NotFound)
        } else {
            Ok(())
        }
    }

    fn delete(&mut self, id: &AnnotationId) -> Result<(), AnnotationError> {
        if id.origin != STORE_ID {
            return Err(AnnotationError::WrongOrigin);
        }
        let row_id = parse_native(&id.native)?;
        let changed = self
            .conn
            .execute(
                "DELETE FROM annotations WHERE id = ?",
                params![row_id],
            )
            .map_err(backend_err)?;
        if changed == 0 {
            Err(AnnotationError::NotFound)
        } else {
            Ok(())
        }
    }
}

fn row_to_annotation(row: &rusqlite::Row) -> rusqlite::Result<Annotation> {
    let id: i64 = row.get(0)?;
    let filename: String = row.get(1)?;
    let target_kind: String = row.get(2)?;
    let panel: Option<String> = row.get(3)?;
    let start_line: Option<i64> = row.get(4)?;
    let end_line: Option<i64> = row.get(5)?;
    let content: String = row.get(6)?;
    let author_login: String = row.get(7)?;
    let created_at_secs: i64 = row.get(8)?;
    let resolved: i64 = row.get(9)?;

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

    let created_at = secs_to_systime(created_at_secs);
    let author = Author {
        login: author_login,
        display_name: None,
    };
    let comment = Comment {
        author,
        content,
        created_at,
    };
    Ok(Annotation {
        id: AnnotationId {
            origin: STORE_ID.to_string(),
            native: id.to_string(),
        },
        filename,
        target,
        comments: vec![comment],
        resolved: resolved != 0,
    })
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

    fn draft_file(filename: &str, content: &str) -> NewAnnotation {
        NewAnnotation {
            filename: filename.to_string(),
            target: AnnotationTarget::File,
            content: content.to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        }
    }

    fn draft_line(filename: &str, content: &str) -> NewAnnotation {
        NewAnnotation {
            filename: filename.to_string(),
            target: AnnotationTarget::LineRange {
                panel: DiffPanelFocus::New,
                start_line: 10,
                end_line: 12,
            },
            content: content.to_string(),
            created_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        }
    }

    #[test]
    fn create_then_list_roundtrip() {
        let (_dir, mut store) = open_temp();
        let a = store.create(draft_file("a.rs", "hello")).unwrap();
        let b = store.create(draft_line("b.rs", "world")).unwrap();

        let all = store.list().unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, a.id);
        assert_eq!(all[0].filename, "a.rs");
        assert!(matches!(all[0].target, AnnotationTarget::File));
        assert_eq!(all[0].root().content, "hello");
        assert_eq!(all[1].id, b.id);
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
        let a = store.create(draft_file("a.rs", "first")).unwrap();
        store.update(&a.id, "second".to_string()).unwrap();
        let all = store.list().unwrap();
        assert_eq!(all[0].root().content, "second");
    }

    #[test]
    fn update_unknown_id_is_not_found() {
        let (_dir, mut store) = open_temp();
        let bogus = AnnotationId {
            origin: STORE_ID.to_string(),
            native: "9999".to_string(),
        };
        match store.update(&bogus, "x".to_string()) {
            Err(AnnotationError::NotFound) => {}
            other => panic!("expected NotFound, got {:?}", other),
        }
    }

    #[test]
    fn delete_removes_row() {
        let (_dir, mut store) = open_temp();
        let a = store.create(draft_file("a.rs", "x")).unwrap();
        store.delete(&a.id).unwrap();
        assert_eq!(store.list().unwrap().len(), 0);
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
    }

    #[test]
    fn list_for_files_filters() {
        let (_dir, mut store) = open_temp();
        store.create(draft_file("a.rs", "A")).unwrap();
        store.create(draft_file("b.rs", "B")).unwrap();
        store.create(draft_file("c.rs", "C")).unwrap();

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
            store.create(draft_file("a.rs", "persistent")).unwrap();
        }
        let store = SqliteStore::open(&path).unwrap();
        let all = store.list().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].root().content, "persistent");
    }
}
