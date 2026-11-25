//! Detekce cloud placeholderů (PROJECT.md §5.1.1) - OneDrive/Google
//! Drive/iCloud/Dropbox "Files On-Demand". Existence takového souboru je
//! bezpečné a rychlé ověřit (metadata jsou lokální), ale ČTENÍ OBSAHU
//! (hashování, EXIF náhled) by spustilo stažení - klidně stovky GB při
//! 30 tisících fotkách. Tenhle modul proto MUSÍ běžet před jakýmkoli
//! hashováním (CLAUDE.md invariant 8: nikdy nestahovat z cloudu bez
//! explicitního souhlasu).

#[cfg(windows)]
mod win {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{
        GetFileAttributesW, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
        FILE_ATTRIBUTE_RECALL_ON_OPEN, INVALID_FILE_ATTRIBUTES,
    };

    /// `true`, pokud `path` má atributy typické pro cloud placeholder
    /// (§5.1.1: `OFFLINE`, `RECALL_ON_OPEN`, `RECALL_ON_DATA_ACCESS`).
    ///
    /// **Vědomě mimo rozsah:** obyčejný `REPARSE_POINT` (symlink/junction)
    /// bez těchto bitů se NEPOVAŽUJE za placeholder - rozlišení by
    /// vyžadovalo číst reparse tag (`GetFileInformationByHandleEx`), což
    /// přidává další `unsafe` povrch za cenu pokrytí případu, který §5.1.1
    /// ani nejmenuje. `false` (chyba čtení atributů, např. souboru
    /// neexistuje) se bere jako "není placeholder" - volající si existenci
    /// ověřuje jinde (`fsops::paths::exists_verbatim`).
    pub fn is_cloud_placeholder(path: &str) -> bool {
        let wide = HSTRING::from(path);
        // SAFETY: `wide` je platný, nulou ukončený řetězec po celou dobu
        // volání. `GetFileAttributesW` nebere žádný jiný ukazatel a jen čte
        // metadata souborového systému - nespouští stažení obsahu.
        let attrs = unsafe { GetFileAttributesW(&wide) };
        if attrs == INVALID_FILE_ATTRIBUTES {
            return false;
        }
        let cloud_bits = FILE_ATTRIBUTE_OFFLINE.0
            | FILE_ATTRIBUTE_RECALL_ON_OPEN.0
            | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS.0;
        attrs & cloud_bits != 0
    }
}

#[cfg(windows)]
pub use win::is_cloud_placeholder;

#[cfg(not(windows))]
pub fn is_cloud_placeholder(_path: &str) -> bool {
    false
}

/// Pohodlnější varianta nad `&Path` - viz [`is_cloud_placeholder`].
pub fn is_cloud_placeholder_path(path: &std::path::Path) -> bool {
    is_cloud_placeholder(&path.to_string_lossy())
}

/// Velikost a čas poslední změny kandidáta pro dialog duplikátů (PROJECT.md
/// §6.2 - sloupce "Velikost"/"Změněno"). Jen čtení metadat souborového
/// systému - na rozdíl od `fsops::hash::md5_hex` nikdy nečte obsah souboru,
/// takže je bezpečné volat i na cloud placeholderu (nespustí stažení).
/// `None`, pokud soubor neexistuje nebo metadata nejdou přečíst.
pub fn size_and_modified(path: &std::path::Path) -> Option<(u64, std::time::SystemTime)> {
    let metadata = std::fs::metadata(crate::fsops::paths::to_verbatim_for_io(path)).ok()?;
    let modified = metadata.modified().ok()?;
    Some((metadata.len(), modified))
}

#[cfg(test)]
mod size_and_modified_tests {
    use super::size_and_modified;

    #[test]
    fn reads_size_and_modified_for_existing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a.CR2");
        std::fs::write(&file, b"hello").unwrap();
        let (size, _modified) = size_and_modified(&file).unwrap();
        assert_eq!(size, 5);
    }

    #[test]
    fn missing_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(size_and_modified(&tmp.path().join("neexistuje.CR2")).is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_local_file_is_not_a_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("normal.CR2");
        std::fs::write(&file, b"data").unwrap();
        assert!(!is_cloud_placeholder_path(&file));
    }

    #[test]
    fn missing_file_is_not_a_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("neexistuje.CR2");
        assert!(!is_cloud_placeholder_path(&file));
    }
}
