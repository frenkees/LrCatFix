//! Undo (PROJECT.md §9): přehraje journal dané session POZPÁTKU a aplikuje
//! inverzní operaci ke každému dokončenému záznamu. Undo běží ve vlastní
//! transakci a sám se zapisuje do journalu jako nová session (na volajícím -
//! `undo_session` samo o sobě žádnou novou session nezakládá, jen vrací DB
//! a soubory do předchozího stavu).

use std::path::Path;

use rusqlite::Connection;
use thiserror::Error;

use crate::fsops::moves;
use crate::journal::write::{self, JournalEntry, JournalError, JournalOp, JournalState};

#[derive(Debug, Error)]
pub enum UndoError {
    #[error(transparent)]
    Journal(#[from] JournalError),
    #[error("chyba SQLite při undo: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("chyba při vracení přesunu souboru {file}: {source}")]
    Move {
        file: String,
        #[source]
        source: moves::MoveError,
    },
    #[error(
        "journal obsahuje neplatnou hodnotu 'before' pro {table}.{id_local} - není to JSON objekt"
    )]
    MalformedBefore { table: String, id_local: i64 },
    /// Tabulka, do které tenhle journal `db_insert` vložil řádek, není v
    /// [`REFERENCE_CHECKS`] - undo neví, jak bezpečně ověřit, že na řádek
    /// nic neukazuje, takže radši odmítne, než aby hádalo (CLAUDE.md §6).
    #[error(
        "undo pro db_insert ({table}.{id_local}) neumí ověřit, že na řádek nic \
         neukazuje - vyžaduje ruční zásah"
    )]
    UnsupportedDbInsert { table: String, id_local: i64 },
    /// §9: "DELETE jen pokud na řádek nic neukazuje". Typicky proto, že
    /// undo ještě neprošlo záznamem, který referenci ruší - v rámci JEDNÉ
    /// session by k tomu nemělo dojít, protože `undo_session` jde odzadu a
    /// operace, co referenci založila (`db_update` file→folder), má vždy
    /// vyšší `seq` než `db_insert`, co founding řádek vytvořil.
    #[error(
        "undo pro db_insert ({table}.{id_local}) nejde provést - řádek je pořád odkazovaný \
         z {referencing_table} (vyžaduje ruční zásah)"
    )]
    InsertStillReferenced {
        table: String,
        id_local: i64,
        referencing_table: String,
    },
}

/// Pro každou tabulku, do které `catalog::apply` umí `db_insert`, řekne,
/// která tabulka/sloupec na ni může odkazovat - jediné bezpečné místo, kde
/// se to smí hádat (na rozdíl od obecné introspekce cizích klíčů, která by
/// mohla minout něco, co katalog reálně používá jinak, CLAUDE.md §6).
const REFERENCE_CHECKS: &[(&str, &str, &str)] = &[("AgLibraryFolder", "AgLibraryFile", "folder")];

#[derive(Debug, Default)]
pub struct UndoReport {
    pub reverted: usize,
    pub skipped: usize,
}

/// Přehraje journal pro `session` pozpátku (od nejvyššího `seq`). Záznamy se
/// stavem jiným než `Done`/`FsDone` se přeskočí - buď nikdy neproběhly
/// (`Planned`, `FsFailed`), nebo už byly vyřešené jinak (`Compensated`,
/// `CompensationFailed`).
pub fn undo_session(
    conn: &Connection,
    journal_path: &Path,
    session: &str,
) -> Result<UndoReport, UndoError> {
    let mut entries = write::read_session(journal_path, session)?;
    entries.sort_by_key(|e| std::cmp::Reverse(e.seq));

    let mut report = UndoReport::default();
    for entry in &entries {
        if !matches!(entry.state, JournalState::Done | JournalState::FsDone) {
            report.skipped += 1;
            continue;
        }
        revert_one(conn, entry)?;
        report.reverted += 1;
    }
    Ok(report)
}

fn revert_one(conn: &Connection, entry: &JournalEntry) -> Result<(), UndoError> {
    match &entry.op {
        JournalOp::DbUpdate {
            table,
            id_local,
            before,
            ..
        } => revert_db_update(conn, table, *id_local, before),
        JournalOp::DbInsert {
            table, id_local, ..
        } => revert_db_insert(conn, table, *id_local),
        JournalOp::FileMove { src, dst, .. } => {
            moves::move_file(dst, src)
                .map(|_| ())
                .map_err(|source| UndoError::Move {
                    file: dst.display().to_string(),
                    source,
                })
        }
    }
}

/// §9: `db_insert` → `DELETE`, ale JEN pokud na řádek nic neukazuje.
/// Tabulka/sloupec, kterým se to ověřuje, se interpoluje přímo do SQL - viz
/// [`revert_db_update`] pro stejné zdůvodnění (pochází z pevného
/// [`REFERENCE_CHECKS`] seznamu v našem vlastním kódu, nikdy od uživatele).
fn revert_db_insert(conn: &Connection, table: &str, id_local: i64) -> Result<(), UndoError> {
    let Some(&(_, ref_table, ref_column)) = REFERENCE_CHECKS.iter().find(|(t, _, _)| *t == table)
    else {
        return Err(UndoError::UnsupportedDbInsert {
            table: table.to_string(),
            id_local,
        });
    };

    let count: i64 = conn.query_row(
        &format!("SELECT count(*) FROM {ref_table} WHERE {ref_column} = ?1"),
        [id_local],
        |r| r.get(0),
    )?;
    if count > 0 {
        return Err(UndoError::InsertStillReferenced {
            table: table.to_string(),
            id_local,
            referencing_table: ref_table.to_string(),
        });
    }

    conn.execute(
        &format!("DELETE FROM {table} WHERE id_local = ?1"),
        [id_local],
    )?;
    Ok(())
}

/// Sestaví `UPDATE <table> SET col = ? [, ...] WHERE id_local = ?` ze
/// sloupců zaznamenaných v `before`. Jméno tabulky se interpoluje přímo do
/// SQL textu - bezpečné jen proto, že pochází z NAŠEHO VLASTNÍHO journalu
/// (psal ho `apply.rs` s pevně danou sadou hodnot), nikdy ne od uživatele -
/// stejné zdůvodnění jako `PRAGMA table_info` v `catalog::schema`.
fn revert_db_update(
    conn: &Connection,
    table: &str,
    id_local: i64,
    before: &serde_json::Value,
) -> Result<(), UndoError> {
    let obj = before
        .as_object()
        .ok_or_else(|| UndoError::MalformedBefore {
            table: table.to_string(),
            id_local,
        })?;
    if obj.is_empty() {
        return Ok(());
    }

    let assignments: Vec<String> = obj.keys().map(|k| format!("{k} = ?")).collect();
    let sql = format!(
        "UPDATE {table} SET {} WHERE id_local = ?",
        assignments.join(", ")
    );

    let mut params: Vec<Box<dyn rusqlite::ToSql>> = obj.values().map(json_value_to_sql).collect();
    params.push(Box::new(id_local));
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();

    conn.execute(&sql, param_refs.as_slice())?;
    Ok(())
}

fn json_value_to_sql(value: &serde_json::Value) -> Box<dyn rusqlite::ToSql> {
    match value {
        serde_json::Value::Null => Box::new(Option::<String>::None),
        serde_json::Value::Bool(b) => Box::new(*b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => Box::new(i),
            None => Box::new(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => Box::new(s.clone()),
        other => Box::new(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal::write::JournalWriter;

    fn open_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, absolutePath TEXT);
             INSERT INTO AgLibraryRootFolder VALUES (1, 'F:/Foto/');",
        )
        .unwrap();
        conn
    }

    #[test]
    fn reverts_db_update_to_before_value() {
        let conn = open_test_db();
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");

        let mut w = JournalWriter::open(&journal_path, "sess1").unwrap();
        w.record(
            JournalState::Done,
            JournalOp::DbUpdate {
                table: "AgLibraryRootFolder".to_string(),
                id_local: 1,
                before: serde_json::json!({"absolutePath": "F:/Foto/"}),
                after: serde_json::json!({"absolutePath": "E:/Foto/"}),
            },
        )
        .unwrap();
        conn.execute(
            "UPDATE AgLibraryRootFolder SET absolutePath = 'E:/Foto/' WHERE id_local = 1",
            [],
        )
        .unwrap();

        let report = undo_session(&conn, &journal_path, "sess1").unwrap();
        assert_eq!(report.reverted, 1);

        let path: String = conn
            .query_row(
                "SELECT absolutePath FROM AgLibraryRootFolder WHERE id_local = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(path, "F:/Foto/");
    }

    #[test]
    fn reverts_file_move_by_moving_back() {
        let conn = open_test_db();
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");
        let src = tmp.path().join("original.CR2");
        let dst = tmp.path().join("moved.CR2");
        std::fs::write(&src, b"data").unwrap();
        std::fs::rename(&src, &dst).unwrap();

        let mut w = JournalWriter::open(&journal_path, "sess1").unwrap();
        w.record(
            JournalState::FsDone,
            JournalOp::FileMove {
                src: src.clone(),
                dst: dst.clone(),
                size: 4,
                mtime: 0,
                hash_blake3: None,
                group_id: None,
            },
        )
        .unwrap();

        let report = undo_session(&conn, &journal_path, "sess1").unwrap();
        assert_eq!(report.reverted, 1);
        assert!(src.exists());
        assert!(!dst.exists());
    }

    fn open_test_db_with_folders_and_files() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, absolutePath TEXT);
             CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, id_global TEXT, pathFromRoot TEXT, rootFolder INTEGER);
             CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, folder INTEGER);
             INSERT INTO AgLibraryRootFolder VALUES (1, 'F:/Foto/');
             INSERT INTO AgLibraryFolder VALUES (10, 'F1', '2019/', 1);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn undo_reverses_db_insert_once_no_longer_referenced() {
        let conn = open_test_db_with_folders_and_files();
        conn.execute("INSERT INTO AgLibraryFile VALUES (100, 10)", [])
            .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");

        // Přesně sled případu B z M4: nejdřív INSERT nové složky (nižší
        // seq), pak UPDATE souboru na ni (vyšší seq).
        let mut w = JournalWriter::open(&journal_path, "sess1").unwrap();
        w.record(
            JournalState::Done,
            JournalOp::DbInsert {
                table: "AgLibraryFolder".to_string(),
                id_local: 20,
                after: serde_json::json!({"pathFromRoot": "2021/Nova/", "rootFolder": 1}),
            },
        )
        .unwrap();
        w.record(
            JournalState::Done,
            JournalOp::DbUpdate {
                table: "AgLibraryFile".to_string(),
                id_local: 100,
                before: serde_json::json!({"folder": 10}),
                after: serde_json::json!({"folder": 20}),
            },
        )
        .unwrap();
        conn.execute(
            "INSERT INTO AgLibraryFolder VALUES (20, 'F2', '2021/Nova/', 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE AgLibraryFile SET folder = 20 WHERE id_local = 100",
            [],
        )
        .unwrap();

        let report = undo_session(&conn, &journal_path, "sess1").unwrap();
        assert_eq!(report.reverted, 2);

        let folder: i64 = conn
            .query_row(
                "SELECT folder FROM AgLibraryFile WHERE id_local = 100",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(folder, 10, "soubor musí ukazovat zpátky na PŮVODNÍ složku");

        let still_exists: bool = conn
            .prepare("SELECT 1 FROM AgLibraryFolder WHERE id_local = 20")
            .unwrap()
            .exists([])
            .unwrap();
        assert!(!still_exists, "nová složka musí být po undo smazaná");
    }

    #[test]
    fn undo_refuses_db_insert_still_referenced() {
        let conn = open_test_db_with_folders_and_files();
        // Jiný, nesouvisející soubor pořád ukazuje na složku 20 - undo tuhle
        // konkrétní db_insert položku nesmí smazat.
        conn.execute(
            "INSERT INTO AgLibraryFolder VALUES (20, 'F2', '2021/Nova/', 1)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO AgLibraryFile VALUES (999, 20)", [])
            .unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");
        let mut w = JournalWriter::open(&journal_path, "sess1").unwrap();
        w.record(
            JournalState::Done,
            JournalOp::DbInsert {
                table: "AgLibraryFolder".to_string(),
                id_local: 20,
                after: serde_json::json!({}),
            },
        )
        .unwrap();

        let err = undo_session(&conn, &journal_path, "sess1").unwrap_err();
        assert!(matches!(err, UndoError::InsertStillReferenced { .. }));
    }

    #[test]
    fn undo_refuses_db_insert_for_unknown_table() {
        let conn = open_test_db_with_folders_and_files();
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");
        let mut w = JournalWriter::open(&journal_path, "sess1").unwrap();
        w.record(
            JournalState::Done,
            JournalOp::DbInsert {
                table: "NejakaNeznamaTabulka".to_string(),
                id_local: 1,
                after: serde_json::json!({}),
            },
        )
        .unwrap();

        let err = undo_session(&conn, &journal_path, "sess1").unwrap_err();
        assert!(matches!(err, UndoError::UnsupportedDbInsert { .. }));
    }

    #[test]
    fn skips_planned_entries_that_never_ran() {
        let conn = open_test_db();
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");

        let mut w = JournalWriter::open(&journal_path, "sess1").unwrap();
        w.record(
            JournalState::Planned,
            JournalOp::DbUpdate {
                table: "AgLibraryRootFolder".to_string(),
                id_local: 1,
                before: serde_json::json!({"absolutePath": "F:/Foto/"}),
                after: serde_json::json!({"absolutePath": "E:/Foto/"}),
            },
        )
        .unwrap();

        let report = undo_session(&conn, &journal_path, "sess1").unwrap();
        assert_eq!(report.reverted, 0);
        assert_eq!(report.skipped, 1);
    }

    #[test]
    fn undo_replays_multiple_entries_in_reverse_order() {
        let conn = open_test_db();
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");

        let mut w = JournalWriter::open(&journal_path, "sess1").unwrap();
        w.record(
            JournalState::Done,
            JournalOp::DbUpdate {
                table: "AgLibraryRootFolder".to_string(),
                id_local: 1,
                before: serde_json::json!({"absolutePath": "F:/Foto/"}),
                after: serde_json::json!({"absolutePath": "E:/Foto/"}),
            },
        )
        .unwrap();
        w.record(
            JournalState::Done,
            JournalOp::DbUpdate {
                table: "AgLibraryRootFolder".to_string(),
                id_local: 1,
                before: serde_json::json!({"absolutePath": "E:/Foto/"}),
                after: serde_json::json!({"absolutePath": "G:/Foto/"}),
            },
        )
        .unwrap();
        conn.execute(
            "UPDATE AgLibraryRootFolder SET absolutePath = 'G:/Foto/' WHERE id_local = 1",
            [],
        )
        .unwrap();

        let report = undo_session(&conn, &journal_path, "sess1").unwrap();
        assert_eq!(report.reverted, 2);
        let path: String = conn
            .query_row(
                "SELECT absolutePath FROM AgLibraryRootFolder WHERE id_local = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(path, "F:/Foto/");
    }
}
