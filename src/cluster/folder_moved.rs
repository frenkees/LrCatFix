//! Vzor A - celá složka se přestěhovala (PROJECT.md §7.1, `FOLDER_MOVED`).
//!
//! Detekce záměrně nevychází ze jména složky, ale z toho, kde MATCH nejlépe
//! obodované kandidáty skutečně našel - shoda jmen souborů je silnější důkaz
//! než shoda jména adresáře, takže se stejně spolehlivě odhalí i přejmenovaná
//! složka (`2019-07 HR` → `2019-07-Chorvatsko`).

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::catalog::apply::Plan;
use crate::catalog::extract::CatalogFile;
use crate::fsops::verify::FileStatus;
use crate::matching::score::{folder_names_are_similar, FileMatches};

const MIN_MISSING: usize = 3;
const MATCH_THRESHOLD: f64 = 0.8;

#[derive(Debug, Clone)]
pub struct FolderMovedSuggestion {
    pub folder_id: i64,
    pub root_id: i64,
    /// `AgLibraryFolder.pathFromRoot` zdrojové složky, přesně jak je v katalogu.
    pub source_rel_path: String,
    /// Cílová složka `T`, kde MATCH nejčastěji našel nejlépe obodované kandidáty.
    pub target_dir: PathBuf,
    /// Kolik souborů má svého nejlepšího kandidáta v `target_dir`.
    pub matched_in_target: usize,
    /// Kolik souborů ze zdrojové složky mělo vůbec nějakého kandidáta.
    pub with_candidate: usize,
    /// Celkem chybějících souborů ve zdrojové složce (`n` ze specifikace).
    pub total_missing: usize,
    /// `true`, když se jméno `target_dir` výrazně liší od jména zdrojové
    /// složky - PROJECT.md §7.1: „zobraz řádek ⚠ Cílová složka má jiný
    /// název - ověř, že jde o tutéž kolekci fotek".
    pub target_name_differs: bool,
}

impl FolderMovedSuggestion {
    pub fn ratio(&self) -> f64 {
        if self.with_candidate == 0 {
            0.0
        } else {
            self.matched_in_target as f64 / self.with_candidate as f64
        }
    }
}

/// Detekuje vzor A nad chybějícími soubory. `excluded_roots` jsou kořeny,
/// pro které existuje nevyřešený návrh vzoru D (PROJECT.md §7.4 „Pořadí
/// vyhodnocení: D → A → B → C" - dokud root sám neexistuje, cesty souborů
/// pod ním jsou nespolehlivé pro detekci na úrovni složky).
pub fn detect(
    files: &[CatalogFile],
    statuses: &HashMap<i64, FileStatus>,
    matches: &HashMap<i64, FileMatches>,
    excluded_roots: &HashSet<i64>,
) -> Vec<FolderMovedSuggestion> {
    let mut by_folder: HashMap<i64, Vec<&CatalogFile>> = HashMap::new();
    for f in files {
        if excluded_roots.contains(&f.root_id) {
            continue;
        }
        if !status_of(statuses, f.file_id).is_repairable() {
            continue;
        }
        by_folder.entry(f.folder_id).or_default().push(f);
    }

    let mut suggestions: Vec<FolderMovedSuggestion> = by_folder
        .into_iter()
        .filter_map(|(folder_id, folder_files)| detect_one(folder_id, &folder_files, matches))
        .collect();

    suggestions.sort_by_key(|s| std::cmp::Reverse(s.total_missing));
    suggestions
}

fn detect_one(
    folder_id: i64,
    folder_files: &[&CatalogFile],
    matches: &HashMap<i64, FileMatches>,
) -> Option<FolderMovedSuggestion> {
    let total_missing = folder_files.len();
    if total_missing < MIN_MISSING {
        return None;
    }

    let mut target_counts: HashMap<PathBuf, usize> = HashMap::new();
    let mut with_candidate = 0usize;
    for f in folder_files {
        let Some(best) = matches.get(&f.file_id).and_then(|m| m.candidates.first()) else {
            continue;
        };
        let Some(dir) = best.path.parent() else {
            continue;
        };
        with_candidate += 1;
        *target_counts.entry(dir.to_path_buf()).or_insert(0) += 1;
    }
    if with_candidate == 0 {
        return None;
    }

    let (target_dir, matched_in_target) = target_counts.into_iter().max_by_key(|(_, c)| *c)?;

    let sample = folder_files[0];
    let suggestion = FolderMovedSuggestion {
        folder_id,
        root_id: sample.root_id,
        source_rel_path: sample.rel_path.clone(),
        target_dir: target_dir.clone(),
        matched_in_target,
        with_candidate,
        total_missing,
        target_name_differs: !folder_names_are_similar(&target_dir, &sample.rel_path),
    };

    if suggestion.ratio() >= MATCH_THRESHOLD {
        Some(suggestion)
    } else {
        None
    }
}

/// Sestaví `Plan` pro vzor A, druhou možnost z PROJECT.md §7.1 ("Přesun
/// souborů zpět"): fyzicky přesune nalezené kandidáty zpátky tam, kde je
/// katalog UŽ TEĎ očekává (`file.expected_path`) - katalog se proto vůbec
/// nemění (`db_ops` je vždy prázdný), jen disk se srovná s tím, co v
/// katalogu je. Zahrnuje sidecary (§3.5) - hlavní soubor a jeho sidecary
/// jdou jako jedna `group_id` skupina, aby se `.xmp` nikdy neoddělil.
///
/// Přesune jen soubory, jejichž nejlépe obodovaný kandidát skutečně leží v
/// `suggestion.target_dir` (přesně ty, které detekce započítala do
/// `matched_in_target`) - soubory s kandidátem jinde nebo bez kandidáta
/// vůbec zůstávají nevyřešené, plán je nehádá.
pub fn plan_move_back(
    suggestion: &FolderMovedSuggestion,
    all_files: &[CatalogFile],
    matches: &HashMap<i64, FileMatches>,
) -> Plan {
    crate::cluster::plan_return_to_expected_path(
        suggestion.folder_id,
        Some(suggestion.target_dir.as_path()),
        "folder-move-back",
        all_files,
        matches,
    )
}

fn status_of(statuses: &HashMap<i64, FileStatus>, file_id: i64) -> FileStatus {
    statuses
        .get(&file_id)
        .copied()
        .unwrap_or(FileStatus::Missing)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::matching::score::ScoredCandidate;

    fn file(id: i64, folder_id: i64, root_id: i64, rel_path: &str, filename: &str) -> CatalogFile {
        CatalogFile {
            file_id: id,
            folder_id,
            root_id,
            root_path: "C:/Foto/".to_string(),
            rel_path: rel_path.to_string(),
            filename: filename.to_string(),
            base_name: filename.to_string(),
            extension: String::new(),
            expected_path: PathBuf::from(rel_path).join(filename),
            external_mod_time: None,
            md5: None,
            file_size: None,
            sidecar_extensions: Vec::new(),
        }
    }

    fn matches_for(id: i64, path: &str, score: i32) -> (i64, FileMatches) {
        (
            id,
            FileMatches {
                file_id: id,
                candidates: vec![ScoredCandidate {
                    path: PathBuf::from(path),
                    score,
                }],
            },
        )
    }

    fn real_file(
        id: i64,
        folder_id: i64,
        base_name: &str,
        extension: &str,
        expected_path: PathBuf,
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
            expected_path,
            external_mod_time: None,
            md5: None,
            file_size: None,
            sidecar_extensions: sidecar_extensions.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn suggestion_for(folder_id: i64, target_dir: PathBuf) -> FolderMovedSuggestion {
        FolderMovedSuggestion {
            folder_id,
            root_id: 1,
            source_rel_path: "2019/2019-07 HR/".to_string(),
            target_dir,
            matched_in_target: 1,
            with_candidate: 1,
            total_missing: 1,
            target_name_differs: false,
        }
    }

    #[test]
    fn plan_move_back_moves_candidate_and_its_sidecar_as_one_group() {
        let tmp = tempfile::tempdir().unwrap();
        let target_dir = tmp.path().join("target");
        let expected_dir = tmp.path().join("expected");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::create_dir_all(&expected_dir).unwrap();
        std::fs::write(target_dir.join("IMG_3000.CR2"), b"raw").unwrap();
        std::fs::write(target_dir.join("IMG_3000.xmp"), b"<xmp/>").unwrap();

        let expected_path = expected_dir.join("IMG_3000.CR2");
        let f = real_file(1, 10, "IMG_3000", "CR2", expected_path.clone(), &["xmp"]);
        let matches: HashMap<i64, FileMatches> = [matches_for(
            1,
            target_dir.join("IMG_3000.CR2").to_str().unwrap(),
            90,
        )]
        .into_iter()
        .collect();
        let suggestion = suggestion_for(10, target_dir.clone());

        let plan = plan_move_back(&suggestion, &[f], &matches);

        assert!(
            plan.db_ops.is_empty(),
            "vzor A move-back nikdy nemění katalog"
        );
        assert_eq!(plan.fs_ops.len(), 2);

        let main_op = plan
            .fs_ops
            .iter()
            .find(|op| op.dst == expected_path)
            .unwrap();
        assert_eq!(main_op.src, target_dir.join("IMG_3000.CR2"));

        let xmp_op = plan
            .fs_ops
            .iter()
            .find(|op| op.src == target_dir.join("IMG_3000.xmp"))
            .unwrap();
        assert_eq!(xmp_op.dst, expected_dir.join("IMG_3000.xmp"));

        assert_eq!(
            main_op.group_id, xmp_op.group_id,
            "hlavní soubor a sidecar musí sdílet group_id (§3.5)"
        );
        assert!(main_op.group_id.is_some());
    }

    #[test]
    fn plan_move_back_skips_files_whose_best_candidate_is_elsewhere() {
        let tmp = tempfile::tempdir().unwrap();
        let target_dir = tmp.path().join("target");
        let elsewhere = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();

        let expected_path = tmp.path().join("expected").join("IMG_1.CR2");
        let f = real_file(1, 10, "IMG_1", "CR2", expected_path, &[]);
        let matches: HashMap<i64, FileMatches> = [matches_for(
            1,
            elsewhere.join("IMG_1.CR2").to_str().unwrap(),
            90,
        )]
        .into_iter()
        .collect();
        let suggestion = suggestion_for(10, target_dir);

        let plan = plan_move_back(&suggestion, &[f], &matches);
        assert!(plan.fs_ops.is_empty());
    }

    #[test]
    fn plan_move_back_skips_files_without_any_candidate() {
        let tmp = tempfile::tempdir().unwrap();
        let target_dir = tmp.path().join("target");
        std::fs::create_dir_all(&target_dir).unwrap();

        let expected_path = tmp.path().join("expected").join("IMG_1.CR2");
        let f = real_file(1, 10, "IMG_1", "CR2", expected_path, &[]);
        let suggestion = suggestion_for(10, target_dir);

        let plan = plan_move_back(&suggestion, &[f], &HashMap::new());
        assert!(plan.fs_ops.is_empty());
    }

    #[test]
    fn detects_folder_moved_when_most_candidates_share_target_dir() {
        let files: Vec<CatalogFile> = (1..=5)
            .map(|i| file(i, 10, 1, "2019/2019-07 HR/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let matches: HashMap<i64, FileMatches> = (1..=4)
            .map(|i| matches_for(i, &format!(r"D:\Zaloha\HR2019\IMG_{i:04}.CR2"), 90))
            .chain(std::iter::once(matches_for(
                5,
                r"E:\Old\dump\IMG_0005.CR2",
                40,
            )))
            .collect();

        let suggestions = detect(&files, &HashMap::new(), &matches, &HashSet::new());

        assert_eq!(suggestions.len(), 1);
        let s = &suggestions[0];
        assert_eq!(s.folder_id, 10);
        assert_eq!(s.total_missing, 5);
        assert_eq!(s.with_candidate, 5);
        assert_eq!(s.matched_in_target, 4);
        assert_eq!(s.target_dir, PathBuf::from(r"D:\Zaloha\HR2019"));
    }

    #[test]
    fn ignores_folder_with_fewer_than_three_missing_files() {
        let files: Vec<CatalogFile> = (1..=2)
            .map(|i| file(i, 10, 1, "2019/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let matches: HashMap<i64, FileMatches> = (1..=2)
            .map(|i| matches_for(i, &format!(r"D:\Zaloha\IMG_{i:04}.CR2"), 90))
            .collect();

        assert!(detect(&files, &HashMap::new(), &matches, &HashSet::new()).is_empty());
    }

    #[test]
    fn ignores_folder_when_candidates_are_too_scattered() {
        let files: Vec<CatalogFile> = (1..=5)
            .map(|i| file(i, 10, 1, "2019/", &format!("IMG_{i:04}.CR2")))
            .collect();
        // Každý kandidát v jiné cílové složce - žádná nedosáhne 80 %.
        let matches: HashMap<i64, FileMatches> = (1..=5)
            .map(|i| matches_for(i, &format!(r"D:\Misto{i}\IMG_{i:04}.CR2"), 50))
            .collect();

        assert!(detect(&files, &HashMap::new(), &matches, &HashSet::new()).is_empty());
    }

    /// REGRESE k nejzávažnějšímu nálezu auditu: odpojené NAS nesmí vypadat
    /// jako přestěhovaná složka. Kdyby `detect` bralo `!= Ok` místo
    /// `is_repairable()`, tenhle případ by prošel a nástroj by nabídl
    /// přepointovat katalog na zálohu jen proto, že disk nebyl připojený.
    #[test]
    fn does_not_propose_repair_for_files_on_an_unavailable_volume() {
        let files: Vec<CatalogFile> = (1..=5)
            .map(|i| file(i, 10, 1, "2019/2019-07 HR/", &format!("IMG_{i:04}.CR2")))
            .collect();
        // MATCH by kandidáty v záloze našel...
        let matches: HashMap<i64, FileMatches> = (1..=5)
            .map(|i| matches_for(i, &format!(r"D:\Zaloha\HR2019\IMG_{i:04}.CR2"), 90))
            .collect();
        // ...ale svazek je jen nedostupný, ne prázdný.
        let statuses: HashMap<i64, FileStatus> = (1..=5)
            .map(|i| (i, FileStatus::FolderUnavailable))
            .collect();

        let suggestions = detect(&files, &statuses, &matches, &HashSet::new());
        assert!(
            suggestions.is_empty(),
            "soubory na nedostupném svazku se nesmí nabídnout k opravě"
        );
    }

    /// Kontrolní protipól předchozího testu: se stejnými daty, ale se
    /// skutečně chybějící složkou, návrh vzniknout MUSÍ.
    #[test]
    fn still_proposes_repair_when_folder_genuinely_missing() {
        let files: Vec<CatalogFile> = (1..=5)
            .map(|i| file(i, 10, 1, "2019/2019-07 HR/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let matches: HashMap<i64, FileMatches> = (1..=5)
            .map(|i| matches_for(i, &format!(r"D:\Zaloha\HR2019\IMG_{i:04}.CR2"), 90))
            .collect();
        let statuses: HashMap<i64, FileStatus> =
            (1..=5).map(|i| (i, FileStatus::FolderMissing)).collect();

        let suggestions = detect(&files, &statuses, &matches, &HashSet::new());
        assert_eq!(suggestions.len(), 1);
        assert_eq!(suggestions[0].matched_in_target, 5);
    }

    #[test]
    fn excludes_files_under_a_root_pending_root_remap() {
        let files: Vec<CatalogFile> = (1..=5)
            .map(|i| file(i, 10, 1, "2019/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let matches: HashMap<i64, FileMatches> = (1..=5)
            .map(|i| matches_for(i, &format!(r"D:\Zaloha\IMG_{i:04}.CR2"), 90))
            .collect();

        let mut excluded = HashSet::new();
        excluded.insert(1i64);

        assert!(detect(&files, &HashMap::new(), &matches, &excluded).is_empty());
    }

    #[test]
    fn flags_target_name_differing_from_source() {
        let files: Vec<CatalogFile> = (1..=3)
            .map(|i| file(i, 10, 1, "2019/2019-07 HR/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let matches: HashMap<i64, FileMatches> = (1..=3)
            .map(|i| {
                matches_for(
                    i,
                    &format!(r"D:\Zaloha\Uplne Jina Slozka\IMG_{i:04}.CR2"),
                    90,
                )
            })
            .collect();

        let suggestions = detect(&files, &HashMap::new(), &matches, &HashSet::new());
        assert_eq!(suggestions.len(), 1);
        assert!(suggestions[0].target_name_differs);
    }

    #[test]
    fn does_not_flag_target_name_when_similar_ignoring_diacritics() {
        let files: Vec<CatalogFile> = (1..=3)
            .map(|i| file(i, 10, 1, "2019/2019-07 HR/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let matches: HashMap<i64, FileMatches> = (1..=3)
            .map(|i| matches_for(i, &format!(r"D:\Zaloha\2019-07-hr\IMG_{i:04}.CR2"), 90))
            .collect();

        let suggestions = detect(&files, &HashMap::new(), &matches, &HashSet::new());
        assert_eq!(suggestions.len(), 1);
        assert!(!suggestions[0].target_name_differs);
    }
}
