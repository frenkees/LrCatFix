//! Schema guard (CLAUDE.md §3 bod 4, PROJECT.md §2 „Schema guard").
//!
//! Než se položí jakýkoli dotaz, ověříme, že katalog obsahuje očekávané
//! tabulky a sloupce. Při neshodě vrátíme chybu s výpisem, co přesně chybí -
//! nikdy nehádáme a nikdy nespustíme `SELECT *`.
//!
//! Rozsah je záměrně omezený na to, co EXTRACT (§3.1 PROJECT.md) skutečně
//! čte. Další tabulky (Adobe_images, ...) se do guardu přidají až s
//! milníkem, který je začne používat.

use rusqlite::Connection;
use thiserror::Error;

struct TableSpec {
    table: &'static str,
    columns: &'static [&'static str],
}

/// Tabulky a sloupce, které EXTRACT (§3.1) potřebuje. Ověřeno proti dokumentované
/// struktuře v PROJECT.md §3; přesná sada sloupců pro konkrétní verzi LrC se
/// může lišit - proto guard, ne předpoklad.
const REQUIRED: &[TableSpec] = &[
    TableSpec {
        table: "AgLibraryRootFolder",
        columns: &["id_local", "id_global", "absolutePath", "name"],
    },
    TableSpec {
        table: "AgLibraryFolder",
        columns: &["id_local", "id_global", "pathFromRoot", "rootFolder"],
    },
    TableSpec {
        table: "AgLibraryFile",
        columns: &[
            "id_local",
            "id_global",
            "folder",
            "baseName",
            "extension",
            "idx_filename",
            "lc_idx_filename",
            "externalModTime",
            "md5",
            "sidecarExtensions",
        ],
    },
];

/// Tabulky/sloupce pro vzor C (§7.3, ALBUM_SCATTERED) - NA ROZDÍL OD
/// [`REQUIRED`] nejde o povinnou část schématu. PROJECT.md §3 dokumentuje
/// tenhle tvar "podle nejlepšího vědomí, ne podle jistoty" a bez přístupu k
/// reálnému katalogu to nejde ověřit (CLAUDE.md §6). [`has_collection_schema`]
/// proto při neshodě detekci vzoru C jen tiše vypne, místo aby nad
/// neověřeným tvarem hádala.
const OPTIONAL_COLLECTIONS: &[TableSpec] = &[
    TableSpec {
        table: "AgLibraryCollection",
        columns: &["id_local", "name"],
    },
    TableSpec {
        table: "AgLibraryCollectionImage",
        columns: &["collection", "image"],
    },
    TableSpec {
        table: "Adobe_images",
        columns: &["id_local", "rootFile"],
    },
];

/// `true`, pokud katalog má schéma potřebné pro detekci vzoru C (§7.3).
/// Nikdy nevrací chybu - viz [`OPTIONAL_COLLECTIONS`].
pub fn has_collection_schema(conn: &Connection) -> bool {
    OPTIONAL_COLLECTIONS.iter().all(|spec| {
        table_columns(conn, spec.table)
            .map(|existing| spec.columns.iter().all(|c| existing.iter().any(|x| x == c)))
            .unwrap_or(false)
    })
}

/// `true`, pokud `AgLibraryFile` má sloupec `importHash` (zdroj velikosti
/// souboru, viz `extract::CatalogFile::file_size`).
///
/// Stejně jako u kolekcí jde o NEPOVINNÝ sloupec: ověřený je na LrC 13.3, ale
/// starší schéma ho mít nemusí. Kdyby se přidal do [`REQUIRED`], nástroj by
/// takové katalogy odmítl otevřít úplně - a to kvůli signálu, bez kterého se
/// dá pracovat. Proto se místo toho jen vypne (`file_size` bude `None`).
pub fn has_import_hash(conn: &Connection) -> bool {
    table_columns(conn, "AgLibraryFile")
        .map(|existing| existing.iter().any(|c| c == "importHash"))
        .unwrap_or(false)
}

#[derive(Debug, Error)]
pub enum SchemaError {
    #[error(
        "katalog nemá očekávané schéma Lightroom Classic - chybí: {0}. \
         Nástroj podporuje jen ověřené verze katalogu a záměrně odmítá pokračovat s neznámým schématem."
    )]
    Mismatch(String),
    #[error("chyba při čtení schématu katalogu: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Ověří, že v katalogu existují všechny tabulky a sloupce z [`REQUIRED`].
/// Vrací `Err` s lidsky čitelným výpisem toho, co chybí - žádné tiché
/// pokračování s neúplným schématem.
pub fn verify_schema(conn: &Connection) -> Result<(), SchemaError> {
    let mut missing = Vec::new();

    for spec in REQUIRED {
        let table_exists: bool = conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?
            .exists([spec.table])?;

        if !table_exists {
            missing.push(format!("tabulka {} chybí celá", spec.table));
            continue;
        }

        let existing_columns = table_columns(conn, spec.table)?;
        for col in spec.columns {
            if !existing_columns.iter().any(|c| c == col) {
                missing.push(format!("{}.{}", spec.table, col));
            }
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(SchemaError::Mismatch(missing.join(", ")))
    }
}

fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>, rusqlite::Error> {
    // PRAGMA nepodporuje bind parametry pro jméno tabulky - jméno je ale
    // převzaté ze sqlite_master (ověřeno o řádek výš), ne od uživatele, takže
    // formátování do SQL řetězce je zde bezpečné.
    let sql = format!("PRAGMA table_info({table})");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_database() {
        let conn = Connection::open_in_memory().unwrap();
        let err = verify_schema(&conn).unwrap_err();
        match err {
            SchemaError::Mismatch(msg) => {
                assert!(msg.contains("AgLibraryRootFolder"));
                assert!(msg.contains("AgLibraryFolder"));
                assert!(msg.contains("AgLibraryFile"));
            }
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn accepts_minimal_valid_schema() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER, id_global TEXT, absolutePath TEXT, name TEXT);
             CREATE TABLE AgLibraryFolder (id_local INTEGER, id_global TEXT, pathFromRoot TEXT, rootFolder INTEGER);
             CREATE TABLE AgLibraryFile (id_local INTEGER, id_global TEXT, folder INTEGER, baseName TEXT,
                                          extension TEXT, idx_filename TEXT, lc_idx_filename TEXT,
                                          externalModTime TEXT, md5 TEXT, sidecarExtensions TEXT);",
        )
        .unwrap();
        assert!(verify_schema(&conn).is_ok());
    }

    #[test]
    fn reports_missing_single_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER, id_global TEXT, absolutePath TEXT, name TEXT);
             CREATE TABLE AgLibraryFolder (id_local INTEGER, id_global TEXT, pathFromRoot TEXT, rootFolder INTEGER);
             CREATE TABLE AgLibraryFile (id_local INTEGER, id_global TEXT, folder INTEGER, baseName TEXT,
                                          extension TEXT, idx_filename TEXT);",
        )
        .unwrap();
        let err = verify_schema(&conn).unwrap_err();
        match err {
            SchemaError::Mismatch(msg) => assert!(msg.contains("AgLibraryFile.lc_idx_filename")),
            other => panic!("unexpected error variant: {other:?}"),
        }
    }

    #[test]
    fn has_collection_schema_false_when_tables_missing() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(!has_collection_schema(&conn));
    }

    #[test]
    fn has_collection_schema_true_when_all_columns_present() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryCollection (id_local INTEGER, name TEXT);
             CREATE TABLE AgLibraryCollectionImage (collection INTEGER, image INTEGER);
             CREATE TABLE Adobe_images (id_local INTEGER, rootFile INTEGER);",
        )
        .unwrap();
        assert!(has_collection_schema(&conn));
    }

    #[test]
    fn has_collection_schema_false_when_one_column_missing() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryCollection (id_local INTEGER, name TEXT);
             CREATE TABLE AgLibraryCollectionImage (collection INTEGER);
             CREATE TABLE Adobe_images (id_local INTEGER, rootFile INTEGER);",
        )
        .unwrap();
        assert!(!has_collection_schema(&conn));
    }
}
