//! MATCH (PROJECT.md §5.3): pro každý chybějící soubor najde kandidáty
//! v `NameIndex` a obodová je.
//!
//! MD5 (§5.3, [`md5_signal`]) se počítá jen NA VYŽÁDÁNÍ - z dialogu
//! duplikátů (§6.2, tlačítko „Porovnat MD5 všech") nebo při remíze - nikdy
//! automaticky pro celou dávku (30k hashů by bylo pomalé a hlavně riskantní
//! kvůli cloud placeholderům, §5.1.1). `match_all` ho proto do skóre
//! nezahrnuje - volající ho přičte přes [`Md5Signal::score_bonus`] až po
//! explicitní akci uživatele.
//!
//! `Velikost souboru odpovídá (+30)` ([`size_score`]) byla dřív vědomě mimo
//! rozsah, protože PROJECT.md §3 žádný sloupec s velikostí nedokumentoval.
//! **Sloupec se mezitím našel a ověřil**: `AgLibraryFile.importHash` nese
//! velikost jako třetí pole (formát a měření viz
//! [`crate::catalog::extract::CatalogFile::file_size`]). Signál je proto
//! zapnutý - ale jen jako bonus při shodě, nikdy jako penalizace při neshodě.
//!
//! Na katalogu bez `importHash` (starší schéma) je `file_size` `None` a
//! signál se sám vypne - žádné hádání.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use unicode_normalization::UnicodeNormalization;

use crate::catalog::extract::CatalogFile;
use crate::fsops::attrs::is_cloud_placeholder_path;
use crate::fsops::paths::match_key_str;
use crate::index::{is_excluded, NameIndex};
use crate::matching::calibrate::Calibration;

const SIBLING_BONUS: i32 = 25;
const SUBPATH_EXACT_BONUS: i32 = 15;
const FOLDER_NAME_SIMILARITY_BONUS: i32 = 8;
const FOLDER_NAME_SIMILARITY_THRESHOLD: f64 = 0.7;
const KNOWN_FOLDER_BONUS: i32 = 5;
const TIME_FULL_POINTS: i32 = 25;
const CLOUD_PLACEHOLDER_PENALTY: i32 = -5;
const RECYCLE_TEMP_PENALTY: i32 = -50;
const LRC_BACKUP_PENALTY: i32 = -100;
const MD5_MATCH_BONUS: i32 = 100;
/// PROJECT.md §5.3. Zapnutý od chvíle, kdy se `AgLibraryFile.importHash`
/// ověřil jako zdroj velikosti (viz doc modulu).
const SIZE_MATCH_BONUS: i32 = 30;

/// Stejné vzory jako u INDEX vyloučení (§14) - candidáti odsud by v praxi
/// neměli do indexu vůbec projít (INDEX filtruje dřív), tohle je jen druhá
/// pojistka pro cesty, které filtr nezachytí (§15: "je jen druhá pojistka").
fn recycle_temp_patterns() -> Vec<String> {
    ["$RECYCLE.BIN", "*/Temp/*", "*/tmp/*"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn lrc_backup_patterns() -> Vec<String> {
    ["*/Backups/*", "*/*.lrdata/*", "*/LrCatFix-backups/*"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

#[derive(Debug, Clone)]
pub struct ScoredCandidate {
    pub path: PathBuf,
    pub score: i32,
}

#[derive(Debug, Clone)]
pub struct FileMatches {
    pub file_id: i64,
    /// Seřazeno sestupně podle skóre.
    pub candidates: Vec<ScoredCandidate>,
}

/// `≥ auto_threshold` a jediný kandidát v rozmezí `tie_margin` od druhého
/// nejlepšího (PROJECT.md §5.3 „Prahy"). Samo o sobě nic neaplikuje - jen
/// říká, jestli by šlo o důvěryhodnou shodu; rozhodnutí APPLY přijde s M3/M4.
pub fn is_confident(candidates: &[ScoredCandidate], auto_threshold: u8, tie_margin: u8) -> bool {
    let Some(best) = candidates.first() else {
        return false;
    };
    if best.score < auto_threshold as i32 {
        return false;
    }
    match candidates.get(1) {
        Some(second) => best.score - second.score > tie_margin as i32,
        None => true,
    }
}

/// Obodová kandidáty pro všechny `missing` soubory. `all_files` je kompletní
/// EXTRACT výsledek (pro signál "kandidát je ve složce, kterou katalog už
/// zná") - musí to být VŠECHNY soubory, ne jen chybějící.
pub fn match_all(
    missing: &[&CatalogFile],
    all_files: &[CatalogFile],
    index: &NameIndex,
    calibration: &Calibration,
    folder_name_similarity_enabled: bool,
) -> Vec<FileMatches> {
    let known_folders: HashSet<PathBuf> = all_files
        .iter()
        .filter_map(|f| f.expected_path.parent().map(Path::to_path_buf))
        .collect();

    let sibling_counts = build_sibling_counts(missing, index);
    let recycle_temp = recycle_temp_patterns();
    let lrc_backups = lrc_backup_patterns();

    missing
        .iter()
        .map(|file| {
            let mut candidates: Vec<ScoredCandidate> = index
                .candidates(&file.filename)
                .iter()
                .map(|path| {
                    let score = score_candidate(
                        file,
                        path,
                        &known_folders,
                        &sibling_counts,
                        calibration,
                        folder_name_similarity_enabled,
                        &recycle_temp,
                        &lrc_backups,
                    );
                    ScoredCandidate {
                        path: path.clone(),
                        score,
                    }
                })
                .collect();
            candidates.sort_by_key(|c| std::cmp::Reverse(c.score));
            FileMatches {
                file_id: file.file_id,
                candidates,
            }
        })
        .collect()
}

type SiblingCounts = HashMap<(i64, PathBuf), HashSet<i64>>;

/// Pro každou dvojici (zdrojová složka, cílová složka kandidáta) spočítá,
/// které chybějící soubory z té zdrojové složky mají kandidáta v té cílové -
/// nejsilnější signál v praxi (§5.3): víc souborů ze stejného zdroje
/// nalezených ve stejném cíli silně naznačuje přesunutou/rozsypanou složku.
fn build_sibling_counts(missing: &[&CatalogFile], index: &NameIndex) -> SiblingCounts {
    let mut counts: SiblingCounts = HashMap::new();
    for file in missing {
        for candidate in index.candidates(&file.filename) {
            if let Some(dir) = candidate.parent() {
                counts
                    .entry((file.folder_id, dir.to_path_buf()))
                    .or_default()
                    .insert(file.file_id);
            }
        }
    }
    counts
}

#[allow(clippy::too_many_arguments)]
fn score_candidate(
    file: &CatalogFile,
    candidate: &Path,
    known_folders: &HashSet<PathBuf>,
    sibling_counts: &SiblingCounts,
    calibration: &Calibration,
    folder_name_similarity_enabled: bool,
    recycle_temp_patterns: &[String],
    lrc_backup_patterns: &[String],
) -> i32 {
    let mut score = 0i32;
    let candidate_dir = candidate.parent();

    if let Some(dir) = candidate_dir {
        let siblings = sibling_counts
            .get(&(file.folder_id, dir.to_path_buf()))
            .map(HashSet::len)
            .unwrap_or(0);
        if siblings > 1 {
            score += SIBLING_BONUS;
        }

        if relative_subpath_matches(dir, &file.rel_path) {
            score += SUBPATH_EXACT_BONUS;
        }

        if folder_name_similarity_enabled && folder_names_are_similar(dir, &file.rel_path) {
            score += FOLDER_NAME_SIMILARITY_BONUS;
        }

        if known_folders.contains(dir) {
            score += KNOWN_FOLDER_BONUS;
        }
    }

    score += attrs_signal(file, candidate, calibration);

    let forward = candidate.to_string_lossy().replace('\\', "/");
    if is_excluded(&forward, recycle_temp_patterns) {
        score += RECYCLE_TEMP_PENALTY;
    }
    if is_excluded(&forward, lrc_backup_patterns) {
        score += LRC_BACKUP_PENALTY;
    }

    // Levná kontrola atributů (§5.1.1) - žádné hashování, žádné stahování,
    // proto se počítá automaticky pro každého kandidáta (na rozdíl od MD5).
    if is_cloud_placeholder_path(candidate) {
        score += CLOUD_PLACEHOLDER_PENALTY;
    }

    score
}

fn relative_subpath_matches(candidate_dir: &Path, rel_path: &str) -> bool {
    let source_segments: Vec<String> = rel_path
        .split('/')
        .filter(|s| !s.is_empty())
        .map(match_key_str)
        .collect();
    if source_segments.is_empty() {
        return false;
    }
    let candidate_segments: Vec<String> = candidate_dir
        .components()
        .filter_map(|c| c.as_os_str().to_str())
        .map(match_key_str)
        .collect();
    if candidate_segments.len() < source_segments.len() {
        return false;
    }
    candidate_segments[candidate_segments.len() - source_segments.len()..] == source_segments[..]
}

/// `pub(crate)` navíc pro `cluster::folder_moved` (§7.1) - "cílová složka má
/// jiný název" varování v UI používá STEJNÝ práh podobnosti jako skórovací
/// bonus, aby se uživateli nezobrazovalo varování u dvojice jmen, kterou
/// MATCH samo považuje za "dost podobnou" (a naopak).
pub(crate) fn folder_names_are_similar(candidate_dir: &Path, rel_path: &str) -> bool {
    let Some(expected_name) = rel_path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty())
    else {
        return false;
    };
    let Some(candidate_name) = candidate_dir.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let similarity = strsim::normalized_levenshtein(
        &normalize_folder_name(expected_name),
        &normalize_folder_name(candidate_name),
    );
    similarity >= FOLDER_NAME_SIMILARITY_THRESHOLD
}

/// Odstraní diakritiku a sjednotí oddělovače `-_ ` na mezeru (PROJECT.md
/// §5.3), aby "2019-07 Chorvatsko" a "2019_07_Chorvatsko" vyšly stejně.
fn normalize_folder_name(name: &str) -> String {
    let without_diacritics: String = name
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .collect();
    let unified: String = without_diacritics
        .chars()
        .map(|c| if c == '-' || c == '_' { ' ' } else { c })
        .collect();
    unified
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Body za shodu času A velikosti dohromady - obojí se čte z JEDNOHO
/// `metadata()`, takže kandidát stojí jeden `stat()`, ne dva. Na síťovém
/// svazku s desítkami tisíc kandidátů to není detail.
///
/// Neúspěšné `metadata()` (soubor zmizel, odpojený svazek) = 0 bodů, nikdy
/// panika - kandidát se prostě neobodová.
fn attrs_signal(file: &CatalogFile, candidate: &Path, calibration: &Calibration) -> i32 {
    let Ok(metadata) = std::fs::metadata(candidate) else {
        return 0;
    };
    size_score(file, &metadata) + time_signal(file, &metadata, calibration)
}

/// PROJECT.md §5.3 „Velikost souboru odpovídá (+30)". Zdroj velikosti je
/// `AgLibraryFile.importHash` - ověřený formát viz
/// [`crate::catalog::extract::CatalogFile::file_size`].
///
/// **Neshoda velikosti není veto, jen absence bonusu.** LrC zapisuje metadata
/// zpátky do některých formátů (ověřeno na `.dng`, `.png`, `.tif`), takže
/// soubor může na disku legitimně vážit o pár kilobajtů víc než při importu -
/// a pořád jde o tentýž snímek. Penalizovat by znamenalo trestat správného
/// kandidáta. Stejné pravidlo jako u MD5 mismatche (§5.3).
fn size_score(file: &CatalogFile, metadata: &std::fs::Metadata) -> i32 {
    match file.file_size {
        Some(expected) if metadata.len() == expected => SIZE_MATCH_BONUS,
        _ => 0,
    }
}

fn time_signal(file: &CatalogFile, metadata: &std::fs::Metadata, calibration: &Calibration) -> i32 {
    if !calibration.is_usable() {
        return 0;
    }
    let Some(raw) = file.external_mod_time.as_ref().and_then(|t| t.as_f64()) else {
        return 0;
    };
    let Some(expected_unix) = calibration.to_unix_seconds(raw) else {
        return 0;
    };
    let Ok(actual) = metadata.modified() else {
        return 0;
    };
    time_score(expected_unix, actual, TIME_FULL_POINTS)
}

/// PROJECT.md §5.4: plné body při rozdílu ≤ 2 s, poloviční při rozdílu, který
/// je celočíselným násobkem 3600 s (±2 s - FAT ukládá lokální čas, takže
/// letní/zimní čas nebo změna pásma posune o celé hodiny), jinak nula.
fn time_score(expected_unix_seconds: f64, actual: SystemTime, full_points: i32) -> i32 {
    let Ok(actual_unix) = actual.duration_since(UNIX_EPOCH) else {
        return 0;
    };
    let diff = (expected_unix_seconds - actual_unix.as_secs_f64()).abs();
    if diff <= 2.0 {
        return full_points;
    }
    let hours = (diff / 3600.0).round();
    if hours >= 1.0 && (diff - hours * 3600.0).abs() <= 2.0 {
        return full_points / 2;
    }
    0
}

/// Výsledek MD5 porovnání pro jednoho kandidáta - viz [`md5_signal`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Md5Signal {
    /// Katalogové MD5 se shoduje s nově spočítaným hashem kandidáta - jistota.
    Match,
    /// Katalogové MD5 se NEshoduje - silný signál PROTI tomuto kandidátovi.
    Mismatch,
    /// Kandidát je cloud placeholder (§5.1.1) - hash se záměrně nespočítal,
    /// aby se nespustilo stažení.
    CloudPlaceholder,
    /// Katalog pro tenhle soubor MD5 vůbec nemá, nebo hashování selhalo
    /// (I/O chyba) - signál prostě není k dispozici.
    Unavailable,
}

impl Md5Signal {
    pub fn score_bonus(self) -> i32 {
        match self {
            Md5Signal::Match => MD5_MATCH_BONUS,
            Md5Signal::Mismatch | Md5Signal::CloudPlaceholder | Md5Signal::Unavailable => 0,
        }
    }
}

/// Spočítá MD5 signál PRO JEDEN kandidát. **Volat jen na vyžádání** (§6.2
/// tlačítko „Porovnat MD5 všech", nebo programově při remíze) - nikdy
/// automaticky pro celou dávku chybějících souborů.
pub fn md5_signal(catalog_md5: Option<&str>, candidate: &Path) -> Md5Signal {
    let Some(expected) = catalog_md5 else {
        return Md5Signal::Unavailable;
    };
    if is_cloud_placeholder_path(candidate) {
        return Md5Signal::CloudPlaceholder;
    }
    match crate::fsops::hash::md5_hex(candidate) {
        Ok(actual) if actual.eq_ignore_ascii_case(expected) => Md5Signal::Match,
        Ok(_) => Md5Signal::Mismatch,
        Err(_) => Md5Signal::Unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn file(id: i64, folder_id: i64, rel_path: &str, filename: &str) -> CatalogFile {
        CatalogFile {
            file_id: id,
            folder_id,
            root_id: 1,
            root_path: "C:/Foto/".to_string(),
            rel_path: rel_path.to_string(),
            filename: filename.to_string(),
            base_name: filename.to_string(),
            extension: String::new(),
            expected_path: PathBuf::from(format!("C:/Foto/{rel_path}{filename}")),
            external_mod_time: None,
            md5: None,
            file_size: None,
            sidecar_extensions: Vec::new(),
        }
    }

    /// Pomocník: skutečný soubor o dané velikosti, ať `size_score` čte
    /// opravdové `metadata()` a ne atrapu.
    fn file_of_size(dir: &Path, name: &str, bytes: usize) -> PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, vec![b'x'; bytes]).unwrap();
        path
    }

    #[test]
    fn size_matches_catalog_gives_bonus() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = file_of_size(tmp.path(), "a.cr2", 1234);
        let mut f = file(1, 10, "2019/", "a.cr2");
        f.file_size = Some(1234);

        let meta = std::fs::metadata(&candidate).unwrap();
        assert_eq!(size_score(&f, &meta), SIZE_MATCH_BONUS);
    }

    /// Neshoda velikosti NESMÍ být penalizace - LrC dopisuje metadata do
    /// .dng/.png/.tif, takže správný kandidát může vážit o pár kB víc.
    #[test]
    fn size_mismatch_is_never_a_penalty() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = file_of_size(tmp.path(), "a.dng", 5000);
        let mut f = file(1, 10, "2019/", "a.dng");
        f.file_size = Some(4987);

        let meta = std::fs::metadata(&candidate).unwrap();
        assert_eq!(size_score(&f, &meta), 0);
    }

    #[test]
    fn size_signal_is_off_when_catalog_has_no_size() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = file_of_size(tmp.path(), "a.cr2", 1234);
        let f = file(1, 10, "2019/", "a.cr2");
        assert_eq!(f.file_size, None);

        let meta = std::fs::metadata(&candidate).unwrap();
        assert_eq!(size_score(&f, &meta), 0);
    }

    /// Zmizelý kandidát nesmí shodit skórování - jen nedostane body.
    #[test]
    fn attrs_signal_returns_zero_for_vanished_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = file(1, 10, "2019/", "a.cr2");
        f.file_size = Some(1234);

        let gone = tmp.path().join("neexistuje.cr2");
        assert_eq!(attrs_signal(&f, &gone, &Calibration::disabled()), 0);
    }

    /// Regrese pro celý smysl toho signálu: dva stejnojmenní kandidáti, kde
    /// se liší JEN velikost. Bez velikosti je to remíza a `is_confident`
    /// vrátí `false`; s velikostí musí vyhrát ten správný.
    #[test]
    fn size_breaks_a_tie_between_identically_named_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let right = file_of_size(tmp.path(), "DSC_0173.JPG", 3_299_111);
        let wrong_dir = tmp.path().join("jinde");
        std::fs::create_dir(&wrong_dir).unwrap();
        let wrong = file_of_size(&wrong_dir, "DSC_0173.JPG", 2_100_000);

        let mut f = file(1, 10, "", "DSC_0173.JPG");
        let mut index = NameIndex::default();
        index.insert(right.clone());
        index.insert(wrong.clone());
        let calibration = Calibration::disabled();

        // Bez velikosti: obě cesty dostanou stejné skóre, tedy remíza.
        let all = vec![f.clone()];
        let before = match_all(&[&f], &all, &index, &calibration, false);
        assert_eq!(before[0].candidates.len(), 2);
        assert_eq!(
            before[0].candidates[0].score, before[0].candidates[1].score,
            "bez velikosti nemá skórovač čím rozhodnout"
        );
        assert!(!is_confident(&before[0].candidates, 70, 15));

        // S velikostí z katalogu vyhraje ten správný, a to o celý bonus.
        f.file_size = Some(3_299_111);
        let all = vec![f.clone()];
        let after = match_all(&[&f], &all, &index, &calibration, false);
        assert_eq!(after[0].candidates[0].path, right);
        assert_eq!(
            after[0].candidates[0].score - after[0].candidates[1].score,
            SIZE_MATCH_BONUS
        );
    }

    #[test]
    fn time_score_full_points_within_two_seconds() {
        let base = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        assert_eq!(time_score(1_600_000_001.0, base, 25), 25);
        assert_eq!(time_score(1_600_000_003.0, base, 25), 0);
    }

    #[test]
    fn time_score_half_points_for_hour_shift() {
        let base = UNIX_EPOCH + Duration::from_secs(1_600_000_000);
        assert_eq!(time_score(1_600_000_000.0 + 3600.0, base, 25), 12);
        assert_eq!(time_score(1_600_000_000.0 - 2.0 * 3600.0, base, 25), 12);
        assert_eq!(time_score(1_600_000_000.0 + 3700.0, base, 25), 0);
    }

    #[test]
    fn relative_subpath_matches_exact_tail() {
        assert!(relative_subpath_matches(
            Path::new(r"D:\Zaloha\2019\2019-07 HR"),
            "2019/2019-07 HR/"
        ));
        assert!(!relative_subpath_matches(
            Path::new(r"D:\Zaloha\2019\Jina slozka"),
            "2019/2019-07 HR/"
        ));
    }

    #[test]
    fn folder_names_are_similar_ignores_diacritics_and_separators() {
        assert!(folder_names_are_similar(
            Path::new(r"D:\2019_07_Chorvatsko"),
            "2019/2019-07 Chorvátsko/"
        ));
        assert!(!folder_names_are_similar(
            Path::new(r"D:\Uplne Jina"),
            "2019/2019-07 Chorvatsko/"
        ));
    }

    #[test]
    fn siblings_in_same_target_folder_get_bonus() {
        let f1 = file(1, 10, "2019/HR/", "a.CR2");
        let f2 = file(2, 10, "2019/HR/", "b.CR2");
        let missing = vec![&f1, &f2];

        let mut index = NameIndex::default();
        index.insert(PathBuf::from(r"D:\Zaloha\a.CR2"));
        index.insert(PathBuf::from(r"D:\Zaloha\b.CR2"));

        let all_files = vec![f1.clone(), f2.clone()];
        let calibration = Calibration::disabled();
        let results = match_all(&missing, &all_files, &index, &calibration, false);

        for r in &results {
            assert_eq!(r.candidates.len(), 1);
            assert!(
                r.candidates[0].score >= SIBLING_BONUS,
                "score {} should include sibling bonus",
                r.candidates[0].score
            );
        }
    }

    #[test]
    fn single_candidate_is_not_a_sibling_of_itself() {
        let f1 = file(1, 10, "2019/HR/", "a.CR2");
        let missing = vec![&f1];

        let mut index = NameIndex::default();
        index.insert(PathBuf::from(r"D:\Zaloha\a.CR2"));

        let all_files = vec![f1.clone()];
        let calibration = Calibration::disabled();
        let results = match_all(&missing, &all_files, &index, &calibration, false);

        assert_eq!(results[0].candidates[0].score, 0);
    }

    #[test]
    fn is_confident_requires_threshold_and_margin() {
        let candidates = vec![
            ScoredCandidate {
                path: PathBuf::from("a"),
                score: 80,
            },
            ScoredCandidate {
                path: PathBuf::from("b"),
                score: 60,
            },
        ];
        assert!(is_confident(&candidates, 70, 15));
        assert!(!is_confident(&candidates, 70, 25));

        let low = vec![ScoredCandidate {
            path: PathBuf::from("a"),
            score: 50,
        }];
        assert!(!is_confident(&low, 70, 15));

        let single = vec![ScoredCandidate {
            path: PathBuf::from("a"),
            score: 90,
        }];
        assert!(is_confident(&single, 70, 15));

        assert!(!is_confident(&[], 70, 15));
    }

    #[test]
    fn md5_signal_matches_when_hash_agrees() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("a.bin");
        std::fs::write(&candidate, b"hello world").unwrap();
        let signal = md5_signal(Some("5eb63bbbe01eeed093cb22bb8f5acdc3"), &candidate);
        assert_eq!(signal, Md5Signal::Match);
        assert_eq!(signal.score_bonus(), MD5_MATCH_BONUS);
    }

    #[test]
    fn md5_signal_matches_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("a.bin");
        std::fs::write(&candidate, b"hello world").unwrap();
        let signal = md5_signal(Some("5EB63BBBE01EEED093CB22BB8F5ACDC3"), &candidate);
        assert_eq!(signal, Md5Signal::Match);
    }

    #[test]
    fn md5_signal_mismatch_gives_no_bonus() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("a.bin");
        std::fs::write(&candidate, b"something else").unwrap();
        let signal = md5_signal(Some("5eb63bbbe01eeed093cb22bb8f5acdc3"), &candidate);
        assert_eq!(signal, Md5Signal::Mismatch);
        assert_eq!(signal.score_bonus(), 0);
    }

    #[test]
    fn md5_signal_unavailable_without_catalog_value() {
        let tmp = tempfile::tempdir().unwrap();
        let candidate = tmp.path().join("a.bin");
        std::fs::write(&candidate, b"data").unwrap();
        assert_eq!(md5_signal(None, &candidate), Md5Signal::Unavailable);
    }

    #[test]
    fn cloud_placeholder_penalty_is_never_applied_to_ordinary_local_candidates() {
        let f1 = file(1, 10, "2019/HR/", "a.CR2");
        let missing = vec![&f1];

        // Syntetická cesta MIMO tempdir - `tempfile::tempdir()` na Windows
        // vždy leží pod `...\Temp\...`, což by samo o sobě spustilo
        // RECYCLE_TEMP_PENALTY a test by testoval něco jiného, než tvrdí.
        let mut index = NameIndex::default();
        index.insert(PathBuf::from(r"D:\Zaloha\a.CR2"));

        let all_files = vec![f1.clone()];
        let calibration = Calibration::disabled();
        let results = match_all(&missing, &all_files, &index, &calibration, false);
        // Obyčejný lokální soubor nesmí dostat penalizaci za cloud placeholder
        // (GetFileAttributesW na neexistující cestě vrátí "není placeholder").
        assert_eq!(results[0].candidates[0].score, 0);
    }
}
