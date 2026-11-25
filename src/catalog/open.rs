//! Otevření `.lrcat`: detekce zámků LrC a otevření SQLite spojení.
//!
//! M0 otevírá katalog VÝHRADNĚ read-only (CLAUDE.md invariant 1: bez zálohy se
//! nezapisuje a M0 zálohu ani zápis vůbec neimplementuje). Detekce zámků se
//! tu provádí i tak - je to informace, kterou uživatel v CLI výstupu potřebuje
//! vidět dřív, než se pokusí (v budoucím milníku) o zápis.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use thiserror::Error;

use crate::catalog::schema::{self, SchemaError};

#[derive(Debug, Error)]
pub enum OpenError {
    #[error("katalog {0} neexistuje")]
    NotFound(PathBuf),
    #[error("nejde otevřít SQLite spojení ke katalogu: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Schema(#[from] SchemaError),
}

/// Stav pomocných souborů vedle katalogu (PROJECT.md §2, CLAUDE.md
/// invariant 2).
///
/// **Pozor na to, co který soubor doopravdy znamená.** Dřív tady stačila
/// pouhá EXISTENCE `-wal` k tomu, aby se zápis odmítl - a to bylo špatně:
///
/// * Katalog LrC běží v režimu WAL, takže `-wal` a `-shm` vedle něj existují
///   úplně běžně. SQLite je maže až při čistém zavření POSLEDNÍHO spojení, a
///   read-only spojení je smazat nemůže (nemá právo zápisu).
/// * Důsledek: náš vlastní read-only průchod (OPEN → EXTRACT → VERIFY) si
///   `-wal` vyrobil sám a od té chvíle si nástroj zakazoval zápis do
///   katalogu, který nikdo jiný otevřený neměl. Ověřeno na čisté kopii
///   katalogu 2026-08-21: po jediném `--headless` běhu přibyl `-wal` (0 B)
///   a `-shm` (32 kB).
///
/// Proto se teď rozlišuje mezi „soubor existuje" a „soubor něco obsahuje", a
/// skutečnou pojistkou proti souběžnému zápisu je zámek samotné SQLite
/// (`BEGIN IMMEDIATE` v `catalog::apply`), který se oklamat nedá.
#[derive(Debug, Default, Clone)]
pub struct LockStatus {
    /// `<katalog>.lrcat.lock` - vlastní zámek Lightroom Classic. Drží ho,
    /// dokud má katalog otevřený, a při zavření ho maže. Tohle je jediný
    /// souborový signál, který o LrC skutečně vypovídá.
    pub lock_file: bool,
    /// Velikost `<katalog>.lrcat-journal`, když existuje. Neprázdný rollback
    /// journal = přerušená transakce, do takového katalogu se nesahá.
    pub rollback_journal_bytes: Option<u64>,
    /// Velikost `<katalog>.lrcat-wal`, když existuje. Prázdný `-wal` NENÍ
    /// signál ničeho (viz doc struktury). Neprázdný znamená data, která
    /// ještě nejsou sloučená do katalogu - musí se zahrnout do zálohy.
    pub wal_bytes: Option<u64>,
}

impl LockStatus {
    /// Blokuje ZÁPIS. Vědomě NEzahrnuje pouhou existenci `-wal`/`-shm`.
    pub fn is_locked(&self) -> bool {
        self.lock_file || self.rollback_journal_bytes.is_some_and(|n| n > 0)
    }

    /// `true`, když `-wal` obsahuje data, která ještě nejsou v `.lrcat`.
    /// Nebrání zápisu, ale záloha ho musí zkopírovat s sebou, jinak by jí
    /// ta data chyběla.
    pub fn has_pending_wal(&self) -> bool {
        self.wal_bytes.is_some_and(|n| n > 0)
    }

    /// Jen pro hlášky uživateli - jestli vedle katalogu vůbec nějaký `-wal`
    /// leží, bez ohledu na obsah.
    pub fn wal_file_present(&self) -> bool {
        self.wal_bytes.is_some()
    }
}

/// Velikost souboru, nebo `None` když neexistuje. Chybu čtení metadat
/// hlásíme jako „existuje, velikost neznámá" (`Some(u64::MAX)`) - opatrnější
/// varianta než tvrdit, že soubor není.
fn file_len(path: &std::ffi::OsStr) -> Option<u64> {
    let p = Path::new(path);
    match std::fs::metadata(p) {
        Ok(m) => Some(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some(u64::MAX),
    }
}

/// Zkontroluje `<katalog>.lrcat.lock`, `<katalog>.lrcat-journal` a
/// `<katalog>.lrcat-wal` vedle souboru katalogu (PROJECT.md §2).
pub fn detect_lock(catalog_path: &Path) -> LockStatus {
    let base = catalog_path.as_os_str().to_os_string();

    let mut lock_path = base.clone();
    lock_path.push(".lock");
    let lock_file = Path::new(&lock_path).exists();

    let mut journal_path = base.clone();
    journal_path.push("-journal");

    let mut wal_path = base;
    wal_path.push("-wal");

    LockStatus {
        lock_file,
        rollback_journal_bytes: file_len(&journal_path),
        wal_bytes: file_len(&wal_path),
    }
}

#[derive(Debug)]
pub struct CatalogHandle {
    pub conn: Connection,
    pub path: PathBuf,
    pub lock_status: LockStatus,
}

#[derive(Debug, Error)]
pub enum OpenWriteError {
    #[error(transparent)]
    Open(#[from] OpenError),
    #[error(
        "katalog vypadá, že je otevřený v Lightroom Classic (nalezen zámkový soubor) - \
         zavřete Lightroom Classic a zkuste to znovu"
    )]
    CatalogLocked,
}

/// Otevře katalog PRO ZÁPIS.
///
/// Volající musí mít už hotovou ověřenou zálohu (CLAUDE.md invariant 1) -
/// tahle funkce to sama nekontroluje, záloha je věc celé APPLY session, ne
/// jednoho otevření spojení. Co se kontroluje VŽDY, těsně před otevřením,
/// jsou zámky (invariant 2: „ne jen při otevření" - `catalog::apply` proto
/// volá tuhle funkci znovu i uvnitř fáze 0.1, těsně před zápisem, ne jen
/// jednou na začátku session).
///
/// Nikdy neprovádí žádný `PRAGMA`, který by měnil `journal_mode`,
/// `page_size`, `encoding` nebo `user_version` (invariant 3) - jediné, co se
/// tu čte, je schema guard (`sqlite_master`/`PRAGMA table_info`, obojí
/// read-only).
pub fn open_read_write(catalog_path: &Path) -> Result<CatalogHandle, OpenWriteError> {
    if !catalog_path.exists() {
        return Err(OpenWriteError::Open(OpenError::NotFound(
            catalog_path.to_path_buf(),
        )));
    }

    let lock_status = detect_lock(catalog_path);
    if lock_status.is_locked() {
        return Err(OpenWriteError::CatalogLocked);
    }

    let conn = Connection::open_with_flags(
        catalog_path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(OpenError::Sqlite)?;

    // SKUTEČNÁ pojistka proti souběžnému zápisu (invariant 2). Kontrola
    // souborů vedle katalogu je jen heuristika - tohle je mechanismus, který
    // poškození opravdu brání, a oklamat se nedá: když katalog drží jiný
    // proces, `BEGIN IMMEDIATE` v `catalog::apply` selže s SQLITE_BUSY.
    //
    // Timeout je krátký schválně. Výchozích 0 ms by hlásilo BUSY i při
    // okamžiku, kdy jiný proces zrovna dopisuje; nekonečné čekání by zase
    // zamrzlo UI. 3 s je dost na přečkání běžné transakce a málo na to, aby
    // to uživatel vnímal jako zaseknutí.
    conn.busy_timeout(std::time::Duration::from_secs(3))
        .map_err(OpenError::Sqlite)?;

    schema::verify_schema(&conn).map_err(OpenError::Schema)?;

    Ok(CatalogHandle {
        conn,
        path: catalog_path.to_path_buf(),
        lock_status,
    })
}

/// Otevře katalog read-only a ověří schéma.
pub fn open_read_only(catalog_path: &Path) -> Result<CatalogHandle, OpenError> {
    if !catalog_path.exists() {
        return Err(OpenError::NotFound(catalog_path.to_path_buf()));
    }

    let lock_status = detect_lock(catalog_path);
    if lock_status.is_locked() {
        tracing::warn!(
            lock_file = lock_status.lock_file,
            rollback_journal_bytes = ?lock_status.rollback_journal_bytes,
            "katalog vypadá, že je otevřený v Lightroom Classic (read-only otevření je i tak bezpečné)"
        );
    }

    let conn = Connection::open_with_flags(
        catalog_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| {
        // Read-only otevření databáze v režimu WAL potřebuje zapisovatelný
        // `-shm`; když ho nejde vytvořit (např. katalog na read-only médiu),
        // SQLite vrátí jen matoucí "unable to open database file".
        // `journal_mode` NESMÍME měnit (invariant 3), takže jediné, co jde
        // udělat, je nabídnout uživateli srozumitelný důvod.
        if lock_status.wal_file_present() {
            tracing::warn!(
                "katalog je v režimu WAL - pokud je složka jen pro čtení, SQLite si vedle něj \
                 nevytvoří potřebný -shm soubor a otevření selže"
            );
        }
        OpenError::Sqlite(e)
    })?;

    schema::verify_schema(&conn)?;

    Ok(CatalogHandle {
        conn,
        path: catalog_path.to_path_buf(),
        lock_status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn missing_catalog_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.lrcat");
        let err = open_read_only(&path).unwrap_err();
        assert!(matches!(err, OpenError::NotFound(_)));
    }

    fn write_sibling(cat: &Path, suffix: &str, content: &[u8]) {
        let mut p = cat.as_os_str().to_os_string();
        p.push(suffix);
        fs::write(Path::new(&p), content).unwrap();
    }

    /// REGRESE: prázdný `-wal` vzniká při každém otevření katalogu v režimu
    /// WAL - i tím naším read-only. Když se bral jako zámek, nástroj si po
    /// prvním načtení katalogu sám zakázal do něj zapisovat.
    #[test]
    fn empty_wal_is_not_a_lock() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("Test.lrcat");
        fs::write(&cat, b"").unwrap();
        write_sibling(&cat, "-wal", b"");
        write_sibling(&cat, "-shm", b"");

        let status = detect_lock(&cat);
        assert_eq!(status.wal_bytes, Some(0));
        assert!(!status.has_pending_wal());
        assert!(
            !status.is_locked(),
            "prázdný -wal nesmí blokovat zápis - vyrábí si ho i naše vlastní read-only spojení"
        );
    }

    /// Neprázdný `-wal` zápis NEBLOKUJE (o souběhu rozhoduje zámek SQLite),
    /// ale musí být poznat, aby ho záloha zkopírovala s sebou.
    #[test]
    fn non_empty_wal_is_reported_as_pending_but_does_not_block() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("Test.lrcat");
        fs::write(&cat, b"").unwrap();
        write_sibling(&cat, "-wal", b"nejaka nesloucena data");

        let status = detect_lock(&cat);
        assert!(status.has_pending_wal());
        assert!(!status.is_locked());
    }

    /// `.lrcat.lock` je vlastní zámek Lightroomu - ten blokuje vždy.
    #[test]
    fn lightroom_lock_file_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("Test.lrcat");
        fs::write(&cat, b"").unwrap();
        write_sibling(&cat, ".lock", b"");

        let status = detect_lock(&cat);
        assert!(status.lock_file);
        assert!(status.is_locked());
    }

    /// Prázdný `-journal` je zbytek, neprázdný znamená přerušenou transakci.
    #[test]
    fn only_non_empty_rollback_journal_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("Test.lrcat");
        fs::write(&cat, b"").unwrap();

        write_sibling(&cat, "-journal", b"");
        assert!(!detect_lock(&cat).is_locked());

        write_sibling(&cat, "-journal", b"prerusena transakce");
        assert!(detect_lock(&cat).is_locked());
    }

    #[test]
    fn no_lock_files_present() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("Test.lrcat");
        fs::write(&cat, b"").unwrap();
        let status = detect_lock(&cat);
        assert!(!status.is_locked());
    }

    fn minimal_catalog(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER, id_global TEXT, absolutePath TEXT, name TEXT);
             CREATE TABLE AgLibraryFolder (id_local INTEGER, id_global TEXT, pathFromRoot TEXT, rootFolder INTEGER);
             CREATE TABLE AgLibraryFile (id_local INTEGER, id_global TEXT, folder INTEGER, baseName TEXT,
                                          extension TEXT, idx_filename TEXT, lc_idx_filename TEXT,
                                          externalModTime TEXT, md5 TEXT, sidecarExtensions TEXT);",
        )
        .unwrap();
    }

    #[test]
    fn open_read_write_refuses_locked_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("Test.lrcat");
        minimal_catalog(&cat);
        // `.lrcat.lock` - vlastní zámek Lightroomu. Prázdný `-wal` se tu
        // schválně nepoužívá: ten vzniká i běžným otevřením a zámek
        // neznamená (viz `empty_wal_is_not_a_lock`).
        write_sibling(&cat, ".lock", b"");

        let err = open_read_write(&cat).unwrap_err();
        assert!(matches!(err, OpenWriteError::CatalogLocked));
    }

    #[test]
    fn open_read_write_allows_writing_when_unlocked() {
        let dir = tempfile::tempdir().unwrap();
        let cat = dir.path().join("Test.lrcat");
        minimal_catalog(&cat);

        let handle = open_read_write(&cat).unwrap();
        handle
            .conn
            .execute(
                "INSERT INTO AgLibraryRootFolder (id_local, absolutePath) VALUES (1, 'C:/Foto/')",
                [],
            )
            .expect("zápisové spojení musí umožnit zápis");
    }

    #[test]
    fn open_read_write_reports_missing_catalog() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.lrcat");
        let err = open_read_write(&path).unwrap_err();
        assert!(matches!(err, OpenWriteError::Open(OpenError::NotFound(_))));
    }
}
