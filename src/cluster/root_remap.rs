//! Vzor D - změna písmene disku / kořene (PROJECT.md §7.4).
//!
//! Řeší se před jakýmkoli skenováním (INDEX fáze, §5.2) - je to nejčastější
//! příčina "ztracených" fotek a skenovat kvůli ní celý disk by bylo absurdní.
//!
//! M0 implementuje §7.4.1 (rychlá sonda svazků, < 1 s): pro každý
//! `AgLibraryRootFolder`, jehož `absolutePath` neexistuje, vezme pár vzorků
//! souborů a zkusí je najít pod jiným písmenem disku.
//!
//! M5 přidává §7.4.3 (obecná detekce z výsledků MATCH) jako fallback pro
//! kořeny, kde sonda (7.4.1) nic nenašla - viz [`fallback_from_match`].
//!
//! §7.4.2 (identifikace podle sériového čísla svazku) je VĚDOMĚ
//! neimplementované: vyžaduje ověřit na reálném katalogu, jestli
//! `AgLibraryRootFolder` vůbec drží identifikátor svazku (CLAUDE.md §6),
//! a bez přístupu k reálnému katalogu to nejde udělat jinak než hádáním,
//! což CLAUDE.md zakazuje. Spec sama u tohohle bodu říká, že sonda 7.4.1
//! „dosahuje prakticky téhož výsledku za srovnatelnou cenu" - tenhle modul
//! se na ni (+ 7.4.3) spoléhá.

use std::collections::HashMap;
use std::path::Path;

use crate::catalog::extract::{CatalogFile, RootFolder};
use crate::fsops::paths::{exists_verbatim, expected_native_path, match_key_str};
use crate::fsops::verify::FileStatus;
use crate::fsops::volumes;
use crate::matching::score::FileMatches;

const SAMPLE_SIZE: usize = 10;
const MATCH_THRESHOLD: f64 = 0.8;
const MATCH_DERIVED_THRESHOLD: f64 = 0.9;

#[derive(Debug, Clone)]
pub struct RemapSuggestion {
    pub root_id: i64,
    pub original_absolute_path: String,
    /// Navržená nová hodnota `AgLibraryRootFolder.absolutePath` (katalogová
    /// konvence: dopředná lomítka, koncové lomítko). Zápis do DB je mimo
    /// rozsah M0 - toto je jen návrh pro CLI/UI report.
    pub proposed_absolute_path: String,
    pub matched_samples: usize,
    pub total_samples: usize,
}

impl RemapSuggestion {
    pub fn ratio(&self) -> f64 {
        if self.total_samples == 0 {
            0.0
        } else {
            self.matched_samples as f64 / self.total_samples as f64
        }
    }
}

/// Vrátí kořeny, jejichž `absolutePath` na disku neexistuje.
pub fn roots_needing_probe(roots: &[RootFolder]) -> Vec<&RootFolder> {
    roots
        .iter()
        .filter(|r| !exists_verbatim(Path::new(&r.absolute_path.replace('/', "\\"))))
        .collect()
}

/// Zkusí najít `root` pod jiným písmenem disku. `files_in_root` musí být
/// podmnožina EXTRACT výsledku patřící tomuto `root.root_id`. Vrací kandidáty
/// se shodou >= [`MATCH_THRESHOLD`], seřazené od nejlepší shody.
pub fn probe(
    root: &RootFolder,
    files_in_root: &[&CatalogFile],
    drive_letters: &[char],
) -> Vec<RemapSuggestion> {
    if files_in_root.is_empty() {
        return Vec::new();
    }

    let sample = evenly_spaced_sample(files_in_root, SAMPLE_SIZE);
    let last_component = last_path_component(&root.absolute_path);
    let current_drive = current_drive_letter(&root.absolute_path);

    let mut candidates = Vec::new();

    for &letter in drive_letters {
        // Sondovat i písmeno, které katalog už má, nemá smysl - to už jsme
        // ověřili jako neexistující v `roots_needing_probe`.
        if Some(letter) == current_drive {
            continue;
        }

        let mut prefixes = vec![format!("{letter}:/")];
        if let Some(comp) = &last_component {
            prefixes.push(format!("{letter}:/{comp}/"));
        }

        for prefix in prefixes {
            let matched = sample
                .iter()
                .filter(|f| {
                    exists_verbatim(&expected_native_path(&prefix, &f.rel_path, &f.filename))
                })
                .count();
            let suggestion = RemapSuggestion {
                root_id: root.root_id,
                original_absolute_path: root.absolute_path.clone(),
                proposed_absolute_path: prefix,
                matched_samples: matched,
                total_samples: sample.len(),
            };
            if suggestion.ratio() >= MATCH_THRESHOLD {
                candidates.push(suggestion);
            }
        }
    }

    candidates.sort_by(|a, b| {
        b.ratio()
            .partial_cmp(&a.ratio())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    candidates
}

/// Zkusí probe() pro všechny kořeny, které to potřebují (viz
/// [`roots_needing_probe`]). Sdílené mezi CLI a GUI vstupním bodem
/// (CLAUDE.md: `app/` volá jen funkce z `catalog/`, `fsops/`, `index/` -
/// respektive z `cluster/`, kam patří tahle orchestrace).
pub fn probe_all(roots: &[RootFolder], files: &[CatalogFile]) -> Vec<RemapSuggestion> {
    let candidates = roots_needing_probe(roots);
    if candidates.is_empty() {
        return Vec::new();
    }
    let drive_letters = volumes::list_drive_letters();
    let mut all = Vec::new();
    for root in candidates {
        let files_in_root: Vec<&CatalogFile> =
            files.iter().filter(|f| f.root_id == root.root_id).collect();
        all.extend(probe(root, &files_in_root, &drive_letters));
    }
    all
}

/// §7.4.3 fallback: pro kořeny, kde sonda (§7.4.1, `probe_all`) nic
/// nenašla, zkusí odvodit nový `absolutePath` z výsledků MATCH místo
/// aktivního zkoušení písmen disků. Volá se AŽ PO MATCH (na rozdíl od
/// `probe_all`, který úmyslně běží před INDEXem/MATCHem, aby se kvůli
/// pouhé změně písmene disku neskenoval celý disk).
pub fn fallback_from_match(
    roots: &[RootFolder],
    files: &[CatalogFile],
    statuses: &HashMap<i64, FileStatus>,
    matches: &HashMap<i64, FileMatches>,
    already_suggested_roots: &std::collections::HashSet<i64>,
) -> Vec<RemapSuggestion> {
    roots_needing_probe(roots)
        .into_iter()
        .filter(|r| !already_suggested_roots.contains(&r.root_id))
        .filter_map(|root| {
            let files_in_root: Vec<&CatalogFile> =
                files.iter().filter(|f| f.root_id == root.root_id).collect();
            derive_from_match(root, &files_in_root, statuses, matches)
        })
        .collect()
}

/// PROJECT.md §7.4.3: "≥ 90 % chybějících souborů z jednoho rootu nalezeno
/// pod jedním společným prefixem se stejnou zbývající strukturou". Pro
/// každý chybějící soubor zkusí z jeho nejlépe obodovaného kandidáta odvodit
/// kořenový prefix ([`derive_root_prefix`]) a hledá prefix, který se
/// shoduje napříč `MATCH_DERIVED_THRESHOLD` podílem souborů.
fn derive_from_match(
    root: &RootFolder,
    files_in_root: &[&CatalogFile],
    statuses: &HashMap<i64, FileStatus>,
    matches: &HashMap<i64, FileMatches>,
) -> Option<RemapSuggestion> {
    let missing: Vec<&&CatalogFile> = files_in_root
        .iter()
        .filter(|f| status_of(statuses, f.file_id).is_repairable())
        .collect();
    if missing.is_empty() {
        return None;
    }

    let mut prefix_counts: HashMap<String, usize> = HashMap::new();
    for f in &missing {
        let Some(best) = matches.get(&f.file_id).and_then(|m| m.candidates.first()) else {
            continue;
        };
        if let Some(prefix) = derive_root_prefix(&best.path, &f.rel_path, &f.filename) {
            *prefix_counts.entry(prefix).or_insert(0) += 1;
        }
    }

    let (best_prefix, matched) = prefix_counts.into_iter().max_by_key(|(_, c)| *c)?;
    let total_samples = missing.len();
    let ratio = matched as f64 / total_samples as f64;
    if ratio < MATCH_DERIVED_THRESHOLD {
        return None;
    }

    Some(RemapSuggestion {
        root_id: root.root_id,
        original_absolute_path: root.absolute_path.clone(),
        proposed_absolute_path: best_prefix,
        matched_samples: matched,
        total_samples,
    })
}

/// Inverze [`crate::fsops::paths::expected_native_path`]: `candidate` MUSÍ
/// strukturálně končit segmenty `rel_path` + `filename` (case-insensitivně,
/// §16.3), jinak `None`. Vrátí zbylý prefix (dopředná lomítka, koncové
/// lomítko) - to, co by muselo být `AgLibraryRootFolder.absolutePath`, aby
/// `expected_native_path(prefix, rel_path, filename) == candidate`.
fn derive_root_prefix(candidate: &Path, rel_path: &str, filename: &str) -> Option<String> {
    let candidate_forward = candidate.to_string_lossy().replace('\\', "/");
    let candidate_segments: Vec<&str> = candidate_forward
        .trim_end_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    let mut rel_segments: Vec<&str> = rel_path.split('/').filter(|s| !s.is_empty()).collect();
    rel_segments.push(filename);

    if candidate_segments.len() <= rel_segments.len() {
        // "<=" ne "<": aspoň jeden segment (svazek/kořen) musí zbýt navíc,
        // jinak by navržený prefix byl prázdný.
        return None;
    }
    let split_at = candidate_segments.len() - rel_segments.len();
    let (prefix_segments, suffix_segments) = candidate_segments.split_at(split_at);

    let suffix_matches = suffix_segments
        .iter()
        .zip(rel_segments.iter())
        .all(|(a, b)| match_key_str(a) == match_key_str(b));
    if !suffix_matches {
        return None;
    }

    Some(format!("{}/", prefix_segments.join("/")))
}

fn status_of(statuses: &HashMap<i64, FileStatus>, file_id: i64) -> FileStatus {
    statuses
        .get(&file_id)
        .copied()
        .unwrap_or(FileStatus::Missing)
}

fn current_drive_letter(absolute_path: &str) -> Option<char> {
    let bytes = absolute_path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        Some(bytes[0].to_ascii_uppercase() as char)
    } else {
        None
    }
}

fn last_path_component(absolute_path: &str) -> Option<String> {
    absolute_path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|s| !s.is_empty() && !s.contains(':'))
        .map(|s| s.to_string())
}

fn evenly_spaced_sample<T>(items: &[T], n: usize) -> Vec<&T> {
    if items.len() <= n || n == 0 {
        return items.iter().collect();
    }
    let step = items.len() as f64 / n as f64;
    (0..n)
        .map(|i| &items[((i as f64) * step) as usize])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(id: i64, rel_path: &str, filename: &str) -> CatalogFile {
        CatalogFile {
            file_id: id,
            folder_id: 1,
            root_id: 1,
            root_path: "E:/Foto/".to_string(),
            rel_path: rel_path.to_string(),
            filename: filename.to_string(),
            base_name: filename.to_string(),
            extension: String::new(),
            expected_path: PathBuf::new(),
            external_mod_time: None,
            md5: None,
            file_size: None,
            sidecar_extensions: Vec::new(),
        }
    }

    #[test]
    fn last_path_component_strips_trailing_slash() {
        assert_eq!(last_path_component("C:/Foto/"), Some("Foto".to_string()));
        assert_eq!(
            last_path_component("C:/Users/pavel/Foto/"),
            Some("Foto".to_string())
        );
        assert_eq!(last_path_component("C:/"), None);
    }

    #[test]
    fn evenly_spaced_sample_covers_whole_range() {
        let items: Vec<i32> = (0..100).collect();
        let sample = evenly_spaced_sample(&items, 10);
        assert_eq!(sample.len(), 10);
        assert_eq!(*sample[0], 0);
        assert!(*sample[9] >= 90);
    }

    /// `probe()` skládá kandidátní prefixy vždy jako `<písmeno>:/...`, tedy
    /// nad skutečnými kořeny disků - v testu nelze bezpečně zapisovat do
    /// kořene reálného disku (a ani by to nemělo být potřeba). Tenhle test
    /// proto ověřuje přesně tu matching logiku, kterou `probe()` uvnitř
    /// používá (prefix + `expected_native_path` + `exists()`), nad tempdirem.
    #[test]
    fn prefix_matching_logic_used_by_probe_finds_relocated_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root_dir = tmp.path().join("Foto");
        std::fs::create_dir_all(root_dir.join("2019")).unwrap();
        std::fs::write(root_dir.join("2019").join("IMG_0001.CR2"), b"x").unwrap();
        std::fs::write(root_dir.join("2019").join("IMG_0002.CR2"), b"x").unwrap();

        let files = [
            file(1, "2019/", "IMG_0001.CR2"),
            file(2, "2019/", "IMG_0002.CR2"),
        ];
        let tmp_prefix = format!(
            "{}/",
            tmp.path().join("Foto").to_string_lossy().replace('\\', "/")
        );
        let matched = files
            .iter()
            .filter(|f| {
                exists_verbatim(&expected_native_path(&tmp_prefix, &f.rel_path, &f.filename))
            })
            .count();
        assert_eq!(matched, 2);
    }

    #[test]
    fn probe_skips_the_drive_letter_the_root_already_has() {
        let root = RootFolder {
            root_id: 1,
            absolute_path: "E:/Foto/".to_string(),
            name: "Foto".to_string(),
        };
        let files = [file(1, "2019/", "IMG_0001.CR2")];
        let refs: Vec<&CatalogFile> = files.iter().collect();
        // 'E' je písmeno, které root už má (a o kterém víme, že neexistuje) -
        // probe() ho musí přeskočit, ne ho nabízet jako "nový" kandidát.
        let candidates = probe(&root, &refs, &['E']);
        assert!(candidates.is_empty());
    }

    #[test]
    fn probe_returns_empty_without_any_drive_letters() {
        let root = RootFolder {
            root_id: 1,
            absolute_path: "E:/Foto/".to_string(),
            name: "Foto".to_string(),
        };
        let files = [file(1, "2019/", "IMG_0001.CR2")];
        let refs: Vec<&CatalogFile> = files.iter().collect();
        assert!(probe(&root, &refs, &[]).is_empty());
    }

    fn matches_for(id: i64, path: &str) -> (i64, FileMatches) {
        (
            id,
            FileMatches {
                file_id: id,
                candidates: vec![crate::matching::score::ScoredCandidate {
                    path: PathBuf::from(path),
                    score: 90,
                }],
            },
        )
    }

    #[test]
    fn derive_root_prefix_strips_matching_suffix() {
        let prefix = derive_root_prefix(
            Path::new(r"F:\Foto\2019\2019-07 HR\IMG_1234.CR2"),
            "2019/2019-07 HR/",
            "IMG_1234.CR2",
        );
        assert_eq!(prefix, Some("F:/Foto/".to_string()));
    }

    #[test]
    fn derive_root_prefix_is_case_insensitive_on_suffix() {
        let prefix = derive_root_prefix(
            Path::new(r"F:\Foto\2019\IMG_1234.CR2"),
            "2019/",
            "img_1234.cr2",
        );
        assert_eq!(prefix, Some("F:/Foto/".to_string()));
    }

    #[test]
    fn derive_root_prefix_none_when_suffix_does_not_match() {
        let prefix = derive_root_prefix(
            Path::new(r"F:\Jinde\IMG_1234.CR2"),
            "2019/2019-07 HR/",
            "IMG_1234.CR2",
        );
        assert_eq!(prefix, None);
    }

    #[test]
    fn derive_root_prefix_none_when_candidate_has_no_room_for_a_root() {
        // Kandidát je přesně "rel_path + filename" bez ničeho navíc - žádný
        // prefix by nezbyl.
        let prefix = derive_root_prefix(Path::new(r"2019\IMG_1234.CR2"), "2019/", "IMG_1234.CR2");
        assert_eq!(prefix, None);
    }

    #[test]
    fn derive_from_match_finds_consistent_prefix_across_most_files() {
        let root = RootFolder {
            root_id: 1,
            absolute_path: "E:/Foto/".to_string(),
            name: "Foto".to_string(),
        };
        let files: Vec<CatalogFile> = (1..=10)
            .map(|i| file(i, "2019/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let refs: Vec<&CatalogFile> = files.iter().collect();

        let mut matches: HashMap<i64, FileMatches> = (1..=9)
            .map(|i| matches_for(i, &format!(r"F:\Foto\2019\IMG_{i:04}.CR2")))
            .collect();
        matches.insert(10, matches_for(10, r"G:\Jinde\IMG_0010.CR2").1);

        let suggestion = derive_from_match(&root, &refs, &HashMap::new(), &matches).unwrap();
        assert_eq!(suggestion.proposed_absolute_path, "F:/Foto/");
        assert_eq!(suggestion.matched_samples, 9);
        assert_eq!(suggestion.total_samples, 10);
    }

    #[test]
    fn derive_from_match_refuses_below_ninety_percent() {
        let root = RootFolder {
            root_id: 1,
            absolute_path: "E:/Foto/".to_string(),
            name: "Foto".to_string(),
        };
        let files: Vec<CatalogFile> = (1..=10)
            .map(|i| file(i, "2019/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let refs: Vec<&CatalogFile> = files.iter().collect();

        // Jen 8/10 (80 %) souhlasí - pod prahem 90 %.
        let mut matches: HashMap<i64, FileMatches> = (1..=8)
            .map(|i| matches_for(i, &format!(r"F:\Foto\2019\IMG_{i:04}.CR2")))
            .collect();
        for i in 9..=10 {
            matches.insert(i, matches_for(i, &format!(r"G:\Jinde{i}\IMG_{i:04}.CR2")).1);
        }

        assert!(derive_from_match(&root, &refs, &HashMap::new(), &matches).is_none());
    }

    #[test]
    fn fallback_from_match_skips_roots_that_already_have_a_probe_suggestion() {
        let roots = vec![RootFolder {
            root_id: 1,
            absolute_path: "E:/Foto/".to_string(),
            name: "Foto".to_string(),
        }];
        let files: Vec<CatalogFile> = (1..=5)
            .map(|i| file(i, "2019/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let matches: HashMap<i64, FileMatches> = (1..=5)
            .map(|i| matches_for(i, &format!(r"F:\Foto\2019\IMG_{i:04}.CR2")))
            .collect();

        let mut already = std::collections::HashSet::new();
        already.insert(1i64);

        assert!(
            fallback_from_match(&roots, &files, &HashMap::new(), &matches, &already).is_empty()
        );
    }
}
