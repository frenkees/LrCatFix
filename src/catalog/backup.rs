//! Záloha katalogu (PROJECT.md §2, CLAUDE.md invariant 1): **bez úspěšné
//! zálohy se nikdy nezapisuje.**
//!
//! Kopíruje se do `*.tmp` a přejmenuje na finální jméno až po ověření
//! velikosti (a u samotného `.lrcat` navíc `PRAGMA integrity_check`) - takže
//! poškozená/částečná záloha nikdy nevypadá jako platná (§2 „Nedokončená
//! záloha"). Při selhání se `.tmp` soubor **nemaže** (CLAUDE.md invariant 5:
//! „Nikdy nemaž soubor na disku" - platí bez výjimky i pro naše vlastní
//! rozpracované soubory; ponechaný `.tmp` navíc pomůže při ladění, co se
//! pokazilo).

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use thiserror::Error;

/// Rezerva navíc k 2× velikosti zálohovaných souborů (PROJECT.md §2).
const MIN_SPARE_BYTES: u64 = 500 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum BackupError {
    #[error(
        "na cílovém svazku není dost místa pro zálohu: potřeba alespoň {needed_mb} MB, \
         volných je jen {available_mb} MB"
    )]
    InsufficientSpace { needed_mb: u64, available_mb: u64 },
    #[error("nejde zjistit volné místo na cílovém svazku zálohy")]
    UnknownFreeSpace,
    #[error("chyba I/O při zálohování: {0}")]
    Io(#[from] std::io::Error),
    #[error("záloha katalogu neprošla kontrolou integrity (PRAGMA integrity_check): {0}")]
    IntegrityCheckFailed(String),
    #[error("velikost zálohy ({actual} B) neodpovídá zdroji ({expected} B) - kopie je poškozená")]
    SizeMismatch { expected: u64, actual: u64 },
    #[error("chyba SQLite při ověřování zálohy: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone)]
pub struct BackupResult {
    pub backup_dir: PathBuf,
    pub catalog_backup: PathBuf,
    /// `None`, pokud vedle katalogu žádný `*.lrcat-data` soubor neexistuje.
    pub data_backup: Option<PathBuf>,
    /// `None`, pokud `-wal` neexistuje nebo je prázdný.
    ///
    /// Neprázdný `-wal` obsahuje potvrzené transakce, které ještě nejsou
    /// sloučené do `.lrcat`. Kdyby se nezálohoval, chyběla by ta data
    /// v záloze a obnova by katalog vrátila do staršího stavu, než v jakém
    /// doopravdy byl - tichá ztráta dat přesně toho druhu, kterému má záloha
    /// bránit.
    pub wal_backup: Option<PathBuf>,
}

/// Vytvoří zálohu do defaultního umístění `<katalog>/LrCatFix-backups/<timestamp>/`.
pub fn create_backup(catalog_path: &Path) -> Result<BackupResult, BackupError> {
    create_backup_to(catalog_path, &default_backup_dir(catalog_path))
}

/// Jako [`create_backup`], ale s explicitním cílovým adresářem - PROJECT.md
/// §2 „Nedostatek místa → odmítnout a nabídnout jiný cíl zálohy": tohle je
/// místo, kudy UI ten jiný cíl předá.
pub fn create_backup_to(
    catalog_path: &Path,
    backup_dir: &Path,
) -> Result<BackupResult, BackupError> {
    let catalog_size = std::fs::metadata(catalog_path)?.len();
    let data_path = data_file_path(catalog_path);
    let data_size = std::fs::metadata(&data_path).map(|m| m.len()).unwrap_or(0);
    let wal_path = sibling_path(catalog_path, "-wal");
    let wal_size = std::fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0);
    let total_size = catalog_size + data_size + wal_size;

    check_free_space(backup_dir, total_size)?;
    std::fs::create_dir_all(backup_dir)?;

    let catalog_backup = copy_and_verify_catalog(catalog_path, backup_dir)?;
    let data_backup = if data_path.exists() {
        Some(copy_and_verify_plain(&data_path, backup_dir)?)
    } else {
        None
    };
    // Prázdný `-wal` je běžný vedlejší produkt otevření katalogu v režimu WAL
    // a nenese žádná data - kopírovat ho nemá smysl. Neprázdný ano, viz
    // `BackupResult::wal_backup`.
    let wal_backup = if wal_size > 0 {
        Some(copy_and_verify_plain(&wal_path, backup_dir)?)
    } else {
        None
    };

    Ok(BackupResult {
        backup_dir: backup_dir.to_path_buf(),
        catalog_backup,
        data_backup,
        wal_backup,
    })
}

/// `<katalog>.lrcat` + přípona, jako `OsString` (jméno katalogu nemusí být
/// platné UTF-8, takže se nesmí procházet přes `String`).
fn sibling_path(catalog_path: &Path, suffix: &str) -> PathBuf {
    let mut name = catalog_path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn default_backup_dir(catalog_path: &Path) -> PathBuf {
    let parent = catalog_path.parent().unwrap_or_else(|| Path::new("."));
    let timestamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    parent.join("LrCatFix-backups").join(timestamp)
}

/// **NEOVĚŘENO** (PROJECT.md §13: „LrC 11+ ukládá část dat do `.lrcat-data` -
/// ověř to na svém katalogu"). Bere se doslovný název ze specifikace
/// (`<katalog>.lrcat-data` vedle `<katalog>.lrcat`). Pokud LrC pomocná data
/// ve skutečnosti ukládá jinak (např. jako složku), tahle funkce je nenajde
/// a záloha by je tiše vynechala - při ověření na reálném katalogu je
/// nutné tohle doplnit/opravit, ne na to zapomenout.
fn data_file_path(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path.as_os_str().to_os_string();
    name.push("-data");
    PathBuf::from(name)
}

fn required_bytes(source_size: u64) -> u64 {
    source_size
        .saturating_mul(2)
        .saturating_add(MIN_SPARE_BYTES)
}

fn check_free_space(target_dir: &Path, source_size: u64) -> Result<(), BackupError> {
    let needed = required_bytes(source_size);
    let available =
        crate::fsops::volumes::free_space_bytes(target_dir).ok_or(BackupError::UnknownFreeSpace)?;
    if available < needed {
        return Err(BackupError::InsufficientSpace {
            needed_mb: needed / (1024 * 1024),
            available_mb: available / (1024 * 1024),
        });
    }
    Ok(())
}

/// Zkopíruje `source` do `backup_dir` jako `<jméno>.tmp` a ověří, že se
/// velikost shoduje. Vrací `(tmp_path, final_path)` - přejmenování na finální
/// jméno je na volajícím, až projdou i další kontroly (u katalogu
/// `integrity_check`).
fn copy_to_backup_verified_by_size(
    source: &Path,
    backup_dir: &Path,
) -> Result<(PathBuf, PathBuf), BackupError> {
    let file_name = source.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "cesta k zálohovanému souboru nemá jméno",
        )
    })?;
    let tmp_path = backup_dir.join(format!("{}.tmp", file_name.to_string_lossy()));
    let final_path = backup_dir.join(file_name);

    std::fs::copy(source, &tmp_path)?;

    let source_size = std::fs::metadata(source)?.len();
    let copied_size = std::fs::metadata(&tmp_path)?.len();
    if source_size != copied_size {
        return Err(BackupError::SizeMismatch {
            expected: source_size,
            actual: copied_size,
        });
    }

    Ok((tmp_path, final_path))
}

fn copy_and_verify_catalog(source: &Path, backup_dir: &Path) -> Result<PathBuf, BackupError> {
    let (tmp_path, final_path) = copy_to_backup_verified_by_size(source, backup_dir)?;
    verify_integrity(&tmp_path)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

/// Sesterský `.lrcat-data` soubor: formát není zdokumentovaný/ověřený, takže
/// se ověřuje jen velikostí - `PRAGMA integrity_check` dává smysl jen pro
/// SQLite, a u tohohle souboru nevíme jistě, že to SQLite je.
fn copy_and_verify_plain(source: &Path, backup_dir: &Path) -> Result<PathBuf, BackupError> {
    let (tmp_path, final_path) = copy_to_backup_verified_by_size(source, backup_dir)?;
    std::fs::rename(&tmp_path, &final_path)?;
    Ok(final_path)
}

fn verify_integrity(path: &Path) -> Result<(), BackupError> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result.eq_ignore_ascii_case("ok") {
        Ok(())
    } else {
        Err(BackupError::IntegrityCheckFailed(result))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_valid_catalog(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE t (x INTEGER); INSERT INTO t VALUES (1);")
            .unwrap();
    }

    #[test]
    fn required_bytes_is_double_plus_spare() {
        assert_eq!(required_bytes(1000), 2000 + MIN_SPARE_BYTES);
        assert_eq!(required_bytes(0), MIN_SPARE_BYTES);
    }

    #[test]
    fn backs_up_valid_catalog_without_data_file() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_valid_catalog(&catalog);

        let backup_dir = tmp.path().join("backup_out");
        let result = create_backup_to(&catalog, &backup_dir).unwrap();

        assert!(result.catalog_backup.exists());
        assert!(!result.catalog_backup.to_string_lossy().ends_with(".tmp"));
        assert_eq!(
            std::fs::metadata(&result.catalog_backup).unwrap().len(),
            std::fs::metadata(&catalog).unwrap().len()
        );
        assert!(result.data_backup.is_none());
    }

    #[test]
    fn backs_up_sibling_data_file_when_present() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_valid_catalog(&catalog);
        let data = tmp.path().join("Test.lrcat-data");
        std::fs::write(&data, b"nejaka pomocna data").unwrap();

        let backup_dir = tmp.path().join("backup_out");
        let result = create_backup_to(&catalog, &backup_dir).unwrap();

        let data_backup = result
            .data_backup
            .expect("data soubor existoval, měl se zálohovat");
        assert!(data_backup.exists());
        assert_eq!(std::fs::read(&data_backup).unwrap(), b"nejaka pomocna data");
    }

    #[test]
    fn corrupt_catalog_fails_integrity_check_and_leaves_no_final_backup() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Corrupt.lrcat");
        // Ne validní SQLite soubor.
        std::fs::write(&catalog, b"tohle neni sqlite databaze vubec").unwrap();

        let backup_dir = tmp.path().join("backup_out");
        let err = create_backup_to(&catalog, &backup_dir).unwrap_err();

        assert!(
            matches!(err, BackupError::Sqlite(_))
                || matches!(err, BackupError::IntegrityCheckFailed(_))
        );
        let final_path = backup_dir.join("Corrupt.lrcat");
        assert!(
            !final_path.exists(),
            "poškozená záloha nesmí nikdy vypadat jako platná"
        );
    }

    #[test]
    fn default_backup_dir_lives_under_lrcatfix_backups_next_to_catalog() {
        let dir = default_backup_dir(Path::new(r"C:\Foto\Katalog.lrcat"));
        assert!(dir.starts_with(r"C:\Foto\LrCatFix-backups"));
    }
}
