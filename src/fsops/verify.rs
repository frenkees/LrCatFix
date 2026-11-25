//! VERIFY (PROJECT.md §5.1): dávkové ověření existence souborů.
//!
//! Naivně `stat()` na každý soubor je pomalé na síťovém disku. Místo toho:
//! seskupíme podle očekávané složky, uděláme jeden `read_dir` na složku a
//! porovnáme množinově přes normalizovaný klíč ([`crate::fsops::paths::match_key`]).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

use crate::catalog::extract::CatalogFile;
use crate::fsops::paths::{match_key, read_dir_verbatim};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileStatus {
    /// Soubor nalezen přesně na očekávané cestě.
    Ok,
    /// Rodičovská složka existuje, ale soubor v ní chybí.
    Missing,
    /// Celá rodičovská složka neexistuje - silný signál pro CLUSTER (§7).
    FolderMissing,
    /// Složku NEJDE PŘEČÍST z jiného důvodu než "neexistuje" - odpojený
    /// síťový disk, nepřipojený externí disk, odepřená oprávnění, I/O chyba.
    ///
    /// **Zásadní rozdíl oproti [`FileStatus::FolderMissing`]:** u toho víme,
    /// že složka opravdu není. Tady nevíme NIC - soubory tam můžou být úplně
    /// v pořádku. Nesmí se proto dostat do MATCH ani do návrhů CLUSTER
    /// (viz [`FileStatus::is_repairable`]), jinak by nástroj nabídl
    /// přepointování katalogu na zálohu jen proto, že uživatel zrovna neměl
    /// zapnuté NAS - přesně ten druh tichého poškození, kterému má nástroj
    /// bránit (CLAUDE.md „Zlaté pravidlo").
    FolderUnavailable,
}

impl FileStatus {
    /// `true` jen pro stavy, kde VÍME, že soubor na očekávané cestě není, a
    /// má tedy smysl hledat náhradu a nabízet opravu.
    ///
    /// Používej tohle místo `!= FileStatus::Ok` všude, kde se rozhoduje, co
    /// vstoupí do MATCH/CLUSTER/zápisu - `!= Ok` by propustilo i
    /// [`FileStatus::FolderUnavailable`], u kterého o souboru nevíme nic.
    pub fn is_repairable(self) -> bool {
        matches!(self, FileStatus::Missing | FileStatus::FolderMissing)
    }
}

#[derive(Debug, Clone)]
pub struct VerifyResult {
    pub file_id: i64,
    pub status: FileStatus,
}

/// Ověří existenci všech `files`. `io_threads` řídí paralelismus této dávky
/// (volající zvolí hodnotu podle toho, jestli jde o lokální nebo síťový
/// svazek - PROJECT.md §5.1 bod 4, CLAUDE.md „Souběžnost").
pub fn verify(files: &[CatalogFile], io_threads: usize) -> Vec<VerifyResult> {
    // Seskup podle rodičovské složky (ne podle folder_id z katalogu - dva
    // různé folder_id mohou teoreticky mířit na tutéž fyzickou cestu, a nás
    // zajímá fyzická cesta, kterou budeme jednou `read_dir`ovat).
    let mut by_parent: HashMap<PathBuf, Vec<&CatalogFile>> = HashMap::new();
    for f in files {
        let parent = f
            .expected_path
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .to_path_buf();
        by_parent.entry(parent).or_default().push(f);
    }

    let groups: Vec<(PathBuf, Vec<&CatalogFile>)> = by_parent.into_iter().collect();

    let run = |groups: &[(PathBuf, Vec<&CatalogFile>)]| {
        groups
            .par_iter()
            .flat_map(|(parent, entries)| verify_one_folder(parent, entries))
            .collect()
    };

    // Stavba vlastního poolu může za neobvyklých okolností selhat (např.
    // vyčerpané OS zdroje na vlákna) - VERIFY je čtecí operace, žádný
    // zápis na ní nezávisí, takže tichý pád na výchozí (globální) rayon
    // pool je bezpečnější než shodit celý běh kvůli limitu na počet vláken
    // pro síťové svazky (§5.1 bod 4 je optimalizace, ne korektnostní
    // požadavek).
    match rayon::ThreadPoolBuilder::new()
        .num_threads(io_threads.max(1))
        .build()
    {
        Ok(pool) => pool.install(|| run(&groups)),
        Err(err) => {
            tracing::warn!(%err, "nejde postavit vlastní thread pool pro VERIFY, používám výchozí");
            run(&groups)
        }
    }
}

fn verify_one_folder(parent: &Path, entries: &[&CatalogFile]) -> Vec<VerifyResult> {
    let read_dir = match read_dir_verbatim(parent) {
        Ok(rd) => rd,
        Err(e) => {
            // ROZLIŠUJEME důvod, protože důsledek je diametrálně jiný:
            // "složka neexistuje" = soubory opravdu chybí, hledej náhradu;
            // "složku nejde přečíst" (odpojené NAS, externí disk, oprávnění)
            // = nevíme nic, soubory tam můžou být v pořádku. Kdyby obojí
            // spadlo do FolderMissing, stačilo by mít vypnuté NAS a nástroj
            // by nabídl přepointovat celý katalog na zálohu.
            // `NotFound` sám o sobě NESTAČÍ: odpojené písmeno disku hlásí
            // Windows úplně stejně jako smazanou složku (ERROR_PATH_NOT_FOUND).
            // Teprve když je svazek dostupný, znamená `NotFound` opravdu
            // "tahle složka tam není" - viz `volumes::is_volume_available`.
            let status = if e.kind() == std::io::ErrorKind::NotFound
                && crate::fsops::volumes::is_volume_available(parent)
            {
                FileStatus::FolderMissing
            } else {
                tracing::warn!(
                    error = %e,
                    count = entries.len(),
                    "složku nejde přečíst - soubory se NEoznačují jako chybějící"
                );
                FileStatus::FolderUnavailable
            };
            return entries
                .iter()
                .map(|f| VerifyResult {
                    file_id: f.file_id,
                    status,
                })
                .collect();
        }
    };

    let mut present: HashSet<String> = HashSet::new();
    for entry in read_dir {
        match entry {
            Ok(e) => {
                present.insert(match_key(&e.file_name()));
            }
            // Nečitelná JEDNOTLIVÁ položka uprostřed jinak čitelné složky.
            // Nesmí zmizet bez záznamu - soubor by se pak nahlásil jako
            // chybějící, aniž bychom věděli proč (§7 doporučení).
            Err(e) => tracing::debug!(error = %e, folder = %parent.display(),
                "položku ve složce nejde přečíst, přeskakuji"),
        }
    }

    entries
        .iter()
        .map(|f| {
            let status = if present.contains(&match_key(std::ffi::OsStr::new(&f.filename))) {
                FileStatus::Ok
            } else {
                FileStatus::Missing
            };
            VerifyResult {
                file_id: f.file_id,
                status,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::extract::CatalogFile;

    fn file(id: i64, dir: &Path, name: &str) -> CatalogFile {
        CatalogFile {
            file_id: id,
            folder_id: 1,
            root_id: 1,
            root_path: dir.to_string_lossy().to_string(),
            rel_path: String::new(),
            filename: name.to_string(),
            base_name: name.to_string(),
            extension: String::new(),
            expected_path: dir.join(name),
            external_mod_time: None,
            md5: None,
            file_size: None,
            sidecar_extensions: Vec::new(),
        }
    }

    #[test]
    fn is_repairable_excludes_unavailable_and_ok() {
        assert!(FileStatus::Missing.is_repairable());
        assert!(FileStatus::FolderMissing.is_repairable());
        assert!(!FileStatus::Ok.is_repairable());
        assert!(
            !FileStatus::FolderUnavailable.is_repairable(),
            "nedostupný svazek se NIKDY nesmí nabídnout k opravě - o souborech na něm nevíme nic"
        );
    }

    /// Nedostupná složka (ne "neexistující") nesmí skončit jako
    /// `FolderMissing` - jinak by odpojené NAS vypadalo jako smazané fotky a
    /// CLUSTER by nabídl přepointovat katalog na zálohu.
    ///
    /// Nedostupnost simulujeme přes cestu, jejíž "rodičovská složka" je ve
    /// skutečnosti SOUBOR: `read_dir` na ni selže s `NotADirectory`/
    /// `InvalidInput` (podle platformy), tedy jinou chybou než `NotFound`.
    #[test]
    fn unreadable_folder_is_unavailable_not_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("jsem_soubor_ne_slozka.txt");
        std::fs::write(&blocker, b"x").unwrap();

        let files = vec![file(1, &blocker, "IMG_0001.CR2")];
        let results = verify(&files, 1);

        assert_eq!(
            results[0].status,
            FileStatus::FolderUnavailable,
            "nečitelná (ne neexistující) složka musí být FolderUnavailable"
        );
        assert!(!results[0].status.is_repairable());
    }

    /// REGRESE z testu na reálném katalogu (LrC 13.3): nepřipojené písmeno
    /// disku vrací `ERROR_PATH_NOT_FOUND` → `io::ErrorKind::NotFound`, tedy
    /// stejnou chybu jako smazaná složka. Dřívější verze fixu to proto
    /// klasifikovala jako `FolderMissing` a odpojený externí disk se tvářil
    /// jako ztracené fotky.
    ///
    /// `Q:` je zvolené jako písmeno, které na běžném stroji není připojené.
    /// Kdyby na testovacím stroji připojené bylo, test se přeskočí, než aby
    /// hlásil falešné selhání.
    #[cfg(windows)]
    #[test]
    fn disconnected_drive_letter_is_unavailable_not_missing() {
        let unmounted = Path::new(r"Q:\");
        if crate::fsops::paths::exists_verbatim(unmounted) {
            eprintln!("Q: je na tomhle stroji připojené, test přeskočen");
            return;
        }

        let files = vec![file(1, Path::new(r"Q:\FOTO\2019"), "IMG_0001.CR2")];
        let results = verify(&files, 1);

        assert_eq!(
            results[0].status,
            FileStatus::FolderUnavailable,
            "nepřipojený disk NENÍ smazaná složka - jinak by nástroj nabídl \
             přepointovat celý katalog jen proto, že disk nebyl zapnutý"
        );
        assert!(!results[0].status.is_repairable());
    }

    #[test]
    fn genuinely_missing_folder_is_still_folder_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_dir = tmp.path().join("tahle_slozka_opravdu_neexistuje");

        let files = vec![file(1, &missing_dir, "IMG_0001.CR2")];
        let results = verify(&files, 1);

        assert_eq!(results[0].status, FileStatus::FolderMissing);
        assert!(
            results[0].status.is_repairable(),
            "opravdu chybějící složka se k opravě nabídnout MÁ"
        );
    }

    #[test]
    fn detects_existing_and_missing_files_and_missing_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let present_dir = tmp.path().join("present");
        std::fs::create_dir(&present_dir).unwrap();
        std::fs::write(present_dir.join("IMG_0001.CR2"), b"x").unwrap();

        let missing_dir = tmp.path().join("does_not_exist");

        let files = vec![
            file(1, &present_dir, "IMG_0001.CR2"),
            file(2, &present_dir, "IMG_0002.CR2"),
            file(3, &missing_dir, "IMG_0003.CR2"),
        ];

        let mut results = verify(&files, 2);
        results.sort_by_key(|r| r.file_id);

        assert_eq!(results[0].status, FileStatus::Ok);
        assert_eq!(results[1].status, FileStatus::Missing);
        assert_eq!(results[2].status, FileStatus::FolderMissing);
    }

    #[test]
    fn match_is_case_insensitive() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("img_0001.cr2"), b"x").unwrap();
        let files = vec![file(1, tmp.path(), "IMG_0001.CR2")];
        let results = verify(&files, 1);
        assert_eq!(results[0].status, FileStatus::Ok);
    }
}
