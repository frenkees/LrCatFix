//! Přesuny souborů s retry logikou (PROJECT.md §16.2, §8) a skupiny se
//! sidecary (§3.5).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use thiserror::Error;

use crate::catalog::extract::CatalogFile;
use crate::fsops::paths::{exists_verbatim, match_key_str, read_dir_verbatim, to_verbatim_for_io};

/// Přípony, které PROJECT.md §3.5 jmenuje jako obvyklé sidecary, KROMĚ toho,
/// co katalog sám hlásí v `AgLibraryFile.sidecarExtensions` (zdroj pravdy) -
/// druhá pojistka pro sidecary, které LrC nezaznamenal (typicky ručně
/// zkopírovaná poznámka vedle fotky).
const KNOWN_SIDECAR_EXTENSIONS: &[&str] = &["xmp", "thm", "wav", "aae", "dop", "pp3", "arp"];

#[derive(Debug, Clone)]
pub struct SidecarMember {
    pub path: PathBuf,
    /// Lowercase, bez tečky.
    pub extension: String,
}

/// Přípony (lowercase, bez tečky), které v `folder_id` katalog eviduje jako
/// VLASTNÍ `AgLibraryFile` řádek pro `base_name` - PROJECT.md §3.5: "před
/// přesunem zjisti, jestli druhý soubor páru má v katalogu vlastní
/// `id_local`. Pokud ano, plánuj ho jako samostatnou položku." Použij
/// výsledek jako `independent_extensions` v [`find_sidecars`], aby se
/// RAW+JPEG pár (dva nezávislé řádky) omylem nesvezl jako sidecar.
pub fn independent_extensions_for(
    all_files: &[CatalogFile],
    folder_id: i64,
    base_name: &str,
) -> HashSet<String> {
    let base_key = match_key_str(base_name);
    all_files
        .iter()
        .filter(|f| f.folder_id == folder_id && match_key_str(&f.base_name) == base_key)
        .map(|f| match_key_str(&f.extension))
        .collect()
}

/// Najde doprovodné soubory `file` v `source_dir` - tam, kde `file` teď
/// fyzicky leží (typicky nalezený kandidát z MATCH, ne nutně
/// `file.expected_path`). Bere v úvahu `file.sidecar_extensions`
/// (`AgLibraryFile.sidecarExtensions`, §3.5 "zdroj pravdy") i
/// [`KNOWN_SIDECAR_EXTENSIONS`] jako druhou pojistku, ale NIKDY přípony ze
/// `independent_extensions` (viz [`independent_extensions_for`]) - ty patří
/// samostatnému `AgLibraryFile` řádku, ne tomuhle souboru.
pub fn find_sidecars(
    file: &CatalogFile,
    source_dir: &Path,
    independent_extensions: &HashSet<String>,
) -> Vec<SidecarMember> {
    let Ok(entries) = read_dir_verbatim(source_dir) else {
        return Vec::new();
    };

    let base_key = match_key_str(&file.base_name);
    let own_filename_key = match_key_str(&file.filename);
    let mut candidate_extensions: HashSet<String> = file
        .sidecar_extensions
        .iter()
        .map(|e| match_key_str(e))
        .collect();
    candidate_extensions.extend(KNOWN_SIDECAR_EXTENSIONS.iter().map(|e| e.to_string()));

    let mut members = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if match_key_str(&name_str) == own_filename_key {
            continue; // hlavní soubor sám sobě sidecarem není
        }
        let Some((base, ext)) = split_base_ext(&name_str) else {
            continue;
        };
        if match_key_str(base) != base_key {
            continue;
        }
        let ext_key = match_key_str(ext);
        if independent_extensions.contains(&ext_key) {
            continue;
        }
        if !candidate_extensions.contains(&ext_key) {
            continue;
        }
        members.push(SidecarMember {
            // `source_dir.join(name)`, NE `entry.path()`: `read_dir_verbatim`
            // prohání `source_dir` přes `\\?\` (§16.1), takže `entry.path()`
            // by verbatim prefix zdědila - a ten se nesmí dostat do UI ani
            // do `ShellExecuteW` (PROJECT.md §16.1). `source_dir` je přesně
            // ten čistý tvar, který volající předal.
            path: source_dir.join(&name),
            extension: ext_key,
        });
    }
    members
}

fn split_base_ext(filename: &str) -> Option<(&str, &str)> {
    let dot = filename.rfind('.')?;
    if dot == 0 {
        return None;
    }
    Some((&filename[..dot], &filename[dot + 1..]))
}

/// WinAPI chybové kódy, u kterých má smysl to zkusit znovu (§16.2) -
/// typicky dočasné zámky od antiviru/indexeru/zálohovacího agenta.
const RETRYABLE_CODES: &[i32] = &[
    32,   // ERROR_SHARING_VIOLATION
    33,   // ERROR_LOCK_VIOLATION
    5,    // ERROR_ACCESS_DENIED (často jen dočasné od AV)
    1224, // ERROR_USER_MAPPED_FILE
    170,  // ERROR_BUSY
];

/// `ERROR_NOT_SAME_DEVICE` - signál k přepnutí na kopírování přes svazky,
/// ne k opakování (retry by to nespravil).
const ERROR_NOT_SAME_DEVICE: i32 = 17;

const MAX_ATTEMPTS: u32 = 5;
const BASE_DELAYS_MS: [u64; 5] = [50, 100, 200, 400, 800];

pub struct RetryOutcome<T> {
    pub result: std::io::Result<T>,
    pub attempts: u32,
}

/// Jediný wrapper pro retry logiku (§16.2) - používá ho přesun, kopie,
/// mazání i budoucí otevření k hashování. `op` dostává číslo pokusu
/// (1-based) a může si samo ověřit idempotenci (§16.2: „před dalším pokusem
/// ověř skutečný stav") dřív, než vrátí chybu.
pub fn with_retry<T>(mut op: impl FnMut(u32) -> std::io::Result<T>) -> RetryOutcome<T> {
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        match op(attempts) {
            Ok(value) => {
                return RetryOutcome {
                    result: Ok(value),
                    attempts,
                }
            }
            Err(e) => {
                if attempts >= MAX_ATTEMPTS || !is_retryable(&e) {
                    return RetryOutcome {
                        result: Err(e),
                        attempts,
                    };
                }
                std::thread::sleep(Duration::from_millis(jittered_delay_ms(
                    BASE_DELAYS_MS[(attempts - 1) as usize],
                )));
            }
        }
    }
}

fn is_retryable(e: &std::io::Error) -> bool {
    e.raw_os_error()
        .map(|code| RETRYABLE_CODES.contains(&code))
        .unwrap_or(false)
}

/// ±25 % jitter bez závislosti na crate `rand` (není v PROJECT.md §4) -
/// `RandomState` je součást `std` a je zaseedovaná z OS entropie při každém
/// `new()`, takže se dá použít jako lehký zdroj náhodnosti.
fn jittered_delay_ms(base_ms: u64) -> u64 {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let random_u64 = RandomState::new().build_hasher().finish();
    let fraction = (random_u64 as f64) / (u64::MAX as f64); // 0.0..1.0
    let jitter_range = base_ms as f64 * 0.25;
    let jitter = (fraction * 2.0 - 1.0) * jitter_range; // -25 %..+25 %
    (base_ms as f64 + jitter).max(0.0) as u64
}

#[derive(Debug, Error)]
pub enum MoveError {
    #[error("cílový soubor už existuje: {}", .0.display())]
    DestinationExists(PathBuf),
    #[error("zdrojový soubor neexistuje: {}", .0.display())]
    SourceMissing(PathBuf),
    #[error("chyba I/O při přesunu souboru: {0}")]
    Io(#[from] std::io::Error),
    #[error("ověření po zkopírování selhalo: {reason}")]
    VerificationFailed { reason: String },
    #[error(
        "soubor byl zkopírován do {}, ale smazání originálu selhalo - existuje teď na obou \
         místech, vyžaduje ruční kontrolu: {source}", .copied_to.display()
    )]
    CopiedButDeleteFailed {
        copied_to: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveMethod {
    /// Atomický `rename` v rámci svazku.
    Rename,
    /// Kopie přes svazky, ověřená velikostí a hashem, pak smazání originálu.
    CopyVerifyDelete,
}

#[derive(Debug, Clone)]
pub struct MoveReport {
    pub method: MoveMethod,
    pub attempts: u32,
    /// Vyplněné jen u [`MoveMethod::CopyVerifyDelete`] - `rename` hash
    /// nepotřebuje, je atomický na úrovni souborového systému.
    pub hash_blake3: Option<String>,
}

/// Přesune `src` na `dst`. Nikdy nepřepíše existující cíl (kolize se řeší
/// výš, v pre-flight fázi APPLY, §8 bod 0.4 - tohle je poslední pojistka).
pub fn move_file(src: &Path, dst: &Path) -> Result<MoveReport, MoveError> {
    if exists_verbatim(dst) {
        return Err(MoveError::DestinationExists(dst.to_path_buf()));
    }
    if !exists_verbatim(src) {
        return Err(MoveError::SourceMissing(src.to_path_buf()));
    }

    let src_v = to_verbatim_for_io(src);
    let dst_v = to_verbatim_for_io(dst);

    let outcome = with_retry(|_attempt| match std::fs::rename(&src_v, &dst_v) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Idempotence (§16.2): `MoveFileExW` je atomické, ale odpověď se
            // mohla ztratit. Pokud cíl existuje a zdroj ne, operace ve
            // skutečnosti proběhla - neopakuj a nehlas chybu.
            if dst_v.exists() && !src_v.exists() {
                Ok(())
            } else {
                Err(e)
            }
        }
    });

    match outcome.result {
        Ok(()) => Ok(MoveReport {
            method: MoveMethod::Rename,
            attempts: outcome.attempts,
            hash_blake3: None,
        }),
        Err(e) if e.raw_os_error() == Some(ERROR_NOT_SAME_DEVICE) => {
            move_across_volumes(&src_v, &dst_v)
        }
        Err(e) => Err(MoveError::Io(e)),
    }
}

fn move_across_volumes(src_v: &Path, dst_v: &Path) -> Result<MoveReport, MoveError> {
    let copy_outcome = with_retry(|_attempt| std::fs::copy(src_v, dst_v).map(|_| ()));
    copy_outcome.result?;

    let src_size = std::fs::metadata(src_v)?.len();
    let dst_size = std::fs::metadata(dst_v)?.len();
    if src_size != dst_size {
        return Err(MoveError::VerificationFailed {
            reason: format!("velikost se neshoduje: zdroj {src_size} B, cíl {dst_size} B"),
        });
    }

    let src_hash = hash_file(src_v)?;
    let dst_hash = hash_file(dst_v)?;
    if src_hash != dst_hash {
        return Err(MoveError::VerificationFailed {
            reason: "hash se po zkopírování neshoduje".to_string(),
        });
    }

    // Teprve TEĎ, po bajtově ověřené kopii, smaž zdroj - jediné povolené
    // mazání v celém nástroji (CLAUDE.md invariant 5), protože tohle je
    // definice "přesunu přes svazky", ne úklid.
    let delete_outcome = with_retry(|_attempt| std::fs::remove_file(src_v));
    match delete_outcome.result {
        Ok(()) => Ok(MoveReport {
            method: MoveMethod::CopyVerifyDelete,
            attempts: copy_outcome.attempts.max(delete_outcome.attempts),
            hash_blake3: Some(dst_hash),
        }),
        Err(source) => Err(MoveError::CopiedButDeleteFailed {
            copied_to: dst_v.to_path_buf(),
            source,
        }),
    }
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher)?;
    Ok(hasher.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(
        id: i64,
        folder_id: i64,
        base_name: &str,
        extension: &str,
        sidecar_extensions: &[&str],
    ) -> CatalogFile {
        CatalogFile {
            file_id: id,
            folder_id,
            root_id: 1,
            root_path: "C:/Foto/".to_string(),
            rel_path: String::new(),
            filename: format!("{base_name}.{extension}"),
            base_name: base_name.to_string(),
            extension: extension.to_string(),
            expected_path: PathBuf::new(),
            external_mod_time: None,
            md5: None,
            file_size: None,
            sidecar_extensions: sidecar_extensions.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn find_sidecars_finds_xmp_and_wav_listed_in_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("IMG_3000.CR2"), b"raw").unwrap();
        std::fs::write(tmp.path().join("IMG_3000.xmp"), b"<xmp/>").unwrap();
        std::fs::write(tmp.path().join("IMG_3000.wav"), b"RIFF").unwrap();

        let f = file(1, 10, "IMG_3000", "CR2", &["xmp", "wav"]);
        let members = find_sidecars(&f, tmp.path(), &HashSet::new());

        let mut exts: Vec<String> = members.iter().map(|m| m.extension.clone()).collect();
        exts.sort();
        assert_eq!(exts, vec!["wav".to_string(), "xmp".to_string()]);
    }

    #[test]
    fn find_sidecars_uses_known_extensions_even_without_catalog_hint() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("IMG_1.CR2"), b"raw").unwrap();
        std::fs::write(tmp.path().join("IMG_1.aae"), b"plist").unwrap();

        // Katalog sidecarExtensions nemá vyplněné - KNOWN_SIDECAR_EXTENSIONS
        // je druhá pojistka.
        let f = file(1, 10, "IMG_1", "CR2", &[]);
        let members = find_sidecars(&f, tmp.path(), &HashSet::new());
        assert_eq!(members.len(), 1);
        assert_eq!(members[0].extension, "aae");
    }

    #[test]
    fn find_sidecars_excludes_independent_raw_jpeg_pair() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("IMG_2000.CR2"), b"raw").unwrap();
        std::fs::write(tmp.path().join("IMG_2000.JPG"), b"jpeg").unwrap();

        let raw = file(1, 10, "IMG_2000", "CR2", &[]);
        let jpeg = file(2, 10, "IMG_2000", "JPG", &[]);
        let independent = independent_extensions_for(&[raw.clone(), jpeg], 10, "IMG_2000");

        // "jpg" má vlastní AgLibraryFile řádek - nesmí se svézt jako sidecar
        // RAWu, i když je to JPEG (což je i v KNOWN_SIDECAR_EXTENSIONS-adjacent
        // riziko, kdyby se nekontrolovalo).
        let members = find_sidecars(&raw, tmp.path(), &independent);
        assert!(members.is_empty());
    }

    /// PROJECT.md §16.1: "Verbatim cesty nikdy nezobrazuj v UI a nepředávej
    /// je do `ShellExecuteW`" - `find_sidecars` interně používá
    /// `read_dir_verbatim`, ale vrácené cesty musí zůstat v ČISTÉM tvaru.
    #[test]
    fn find_sidecars_never_leaks_verbatim_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("IMG_1.CR2"), b"raw").unwrap();
        std::fs::write(tmp.path().join("IMG_1.xmp"), b"<xmp/>").unwrap();

        let f = file(1, 10, "IMG_1", "CR2", &["xmp"]);
        let members = find_sidecars(&f, tmp.path(), &HashSet::new());
        assert_eq!(members.len(), 1);
        assert!(!members[0].path.to_string_lossy().starts_with(r"\\?\"));
        assert_eq!(members[0].path, tmp.path().join("IMG_1.xmp"));
    }

    #[test]
    fn find_sidecars_ignores_the_main_file_itself() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("IMG_1.CR2"), b"raw").unwrap();

        let f = file(1, 10, "IMG_1", "CR2", &[]);
        assert!(find_sidecars(&f, tmp.path(), &HashSet::new()).is_empty());
    }

    #[test]
    fn find_sidecars_ignores_unrelated_extensions_not_in_any_known_list() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("IMG_1.CR2"), b"raw").unwrap();
        std::fs::write(tmp.path().join("IMG_1.txt"), b"poznamka").unwrap();

        let f = file(1, 10, "IMG_1", "CR2", &[]);
        assert!(find_sidecars(&f, tmp.path(), &HashSet::new()).is_empty());
    }

    #[test]
    fn independent_extensions_for_only_looks_at_same_folder() {
        let a = file(1, 10, "IMG_1", "CR2", &[]);
        let b = file(2, 11, "IMG_1", "JPG", &[]); // jiná složka, stejné jméno
        let independent = independent_extensions_for(&[a, b], 10, "IMG_1");
        assert!(!independent.contains("jpg"));
    }

    #[test]
    fn moves_file_within_same_volume() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        let dst = tmp.path().join("b.txt");
        std::fs::write(&src, b"obsah").unwrap();

        let report = move_file(&src, &dst).unwrap();
        assert_eq!(report.method, MoveMethod::Rename);
        assert!(!src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"obsah");
    }

    #[test]
    fn refuses_to_overwrite_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.txt");
        let dst = tmp.path().join("b.txt");
        std::fs::write(&src, b"zdroj").unwrap();
        std::fs::write(&dst, b"uz tu neco je").unwrap();

        let err = move_file(&src, &dst).unwrap_err();
        assert!(matches!(err, MoveError::DestinationExists(_)));
        // Nic se nesmělo změnit.
        assert!(src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"uz tu neco je");
    }

    #[test]
    fn reports_missing_source_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("neexistuje.txt");
        let dst = tmp.path().join("b.txt");

        let err = move_file(&src, &dst).unwrap_err();
        assert!(matches!(err, MoveError::SourceMissing(_)));
    }

    #[test]
    fn with_retry_gives_up_immediately_on_non_retryable_error() {
        let mut calls = 0;
        let outcome: RetryOutcome<()> = with_retry(|_| {
            calls += 1;
            Err(std::io::Error::from_raw_os_error(2)) // ERROR_FILE_NOT_FOUND - neopakovat
        });
        assert_eq!(calls, 1);
        assert_eq!(outcome.attempts, 1);
        assert!(outcome.result.is_err());
    }

    #[test]
    fn with_retry_retries_retryable_errors_up_to_max_attempts() {
        let mut calls = 0;
        let outcome: RetryOutcome<()> = with_retry(|_| {
            calls += 1;
            Err(std::io::Error::from_raw_os_error(32)) // ERROR_SHARING_VIOLATION
        });
        assert_eq!(calls, MAX_ATTEMPTS);
        assert_eq!(outcome.attempts, MAX_ATTEMPTS);
        assert!(outcome.result.is_err());
    }

    #[test]
    fn with_retry_succeeds_after_transient_failures() {
        let mut calls = 0;
        let outcome = with_retry(|_| {
            calls += 1;
            if calls < 3 {
                Err(std::io::Error::from_raw_os_error(32))
            } else {
                Ok(42)
            }
        });
        assert_eq!(outcome.result.unwrap(), 42);
        assert_eq!(outcome.attempts, 3);
    }

    #[test]
    fn jittered_delay_stays_within_twenty_five_percent() {
        for _ in 0..50 {
            let d = jittered_delay_ms(100);
            assert!((75..=125).contains(&d), "delay {d} mimo očekávané rozmezí");
        }
    }
}
