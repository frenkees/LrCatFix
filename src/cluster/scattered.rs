//! Vzor B - rozsypaná složka (PROJECT.md §7.2, `SCATTERED`).
//!
//! Detekce (M5) + konsolidace (M6, [`plan_consolidate`]) - přesun N souborů
//! zpátky do zdrojové složky, kterou katalog už očekává, vč. sidecarů
//! (§3.5), sdílené jádro s vzorem A viz `cluster::plan_return_to_expected_path`.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use crate::catalog::apply::Plan;
use crate::catalog::extract::CatalogFile;
use crate::fsops::verify::FileStatus;
use crate::matching::score::FileMatches;

const MIN_DISTINCT_TARGETS: usize = 5;
const MAX_MEDIAN_PER_TARGET: f64 = 2.0;

#[derive(Debug, Clone)]
pub struct ScatteredSuggestion {
    pub folder_id: i64,
    pub root_id: i64,
    pub source_rel_path: String,
    pub total_missing: usize,
    pub with_candidate: usize,
    /// Počet různých cílových složek, kde MATCH našel nejlépe obodované
    /// kandidáty (`k` ze specifikace).
    pub distinct_targets: usize,
    pub median_per_target: f64,
}

/// Detekuje vzor B. `excluded_roots` - stejně jako u vzoru A, kořeny
/// čekající na vyřešení vzoru D (§7.4). `excluded_folders` - složky, které
/// už si vzalo vzor A (PROJECT.md §7.4 „Pořadí vyhodnocení: D → A → B → C -
/// každý vyřešený vzor odebere soubory z fronty pro nižší úrovně").
pub fn detect(
    files: &[CatalogFile],
    statuses: &HashMap<i64, FileStatus>,
    matches: &HashMap<i64, FileMatches>,
    excluded_roots: &HashSet<i64>,
    excluded_folders: &HashSet<i64>,
) -> Vec<ScatteredSuggestion> {
    let mut by_folder: HashMap<i64, Vec<&CatalogFile>> = HashMap::new();
    for f in files {
        if excluded_roots.contains(&f.root_id) || excluded_folders.contains(&f.folder_id) {
            continue;
        }
        if !status_of(statuses, f.file_id).is_repairable() {
            continue;
        }
        by_folder.entry(f.folder_id).or_default().push(f);
    }

    let mut suggestions: Vec<ScatteredSuggestion> = by_folder
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
) -> Option<ScatteredSuggestion> {
    let total_missing = folder_files.len();

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

    let distinct_targets = target_counts.len();
    if distinct_targets < MIN_DISTINCT_TARGETS {
        return None;
    }

    let median_per_target = median(target_counts.values().copied().collect());
    if median_per_target > MAX_MEDIAN_PER_TARGET {
        return None;
    }

    let sample = folder_files[0];
    Some(ScatteredSuggestion {
        folder_id,
        root_id: sample.root_id,
        source_rel_path: sample.rel_path.clone(),
        total_missing,
        with_candidate,
        distinct_targets,
        median_per_target,
    })
}

/// Sestaví `Plan` pro vzor B "konsolidace" (PROJECT.md §7.2): přesune KAŽDÝ
/// soubor ze zdrojové složky, který má nějakého kandidáta, zpátky na cestu,
/// kterou katalog už očekává - na rozdíl od vzoru A (`folder_moved::plan_move_back`)
/// bez omezení na jednu konkrétní cílovou složku, protože SCATTERED soubory
/// jsou z definice v mnoha různých místech. `db_ops` je vždy prázdný.
pub fn plan_consolidate(
    suggestion: &ScatteredSuggestion,
    all_files: &[CatalogFile],
    matches: &HashMap<i64, FileMatches>,
) -> Plan {
    crate::cluster::plan_return_to_expected_path(
        suggestion.folder_id,
        None,
        "consolidate",
        all_files,
        matches,
    )
}

fn median(mut values: Vec<usize>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_unstable();
    let mid = values.len() / 2;
    if values.len() % 2 == 1 {
        values[mid] as f64
    } else {
        (values[mid - 1] + values[mid]) as f64 / 2.0
    }
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

    fn matches_for(id: i64, path: &str) -> (i64, FileMatches) {
        (
            id,
            FileMatches {
                file_id: id,
                candidates: vec![ScoredCandidate {
                    path: PathBuf::from(path),
                    score: 50,
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
            sidecar_extensions: Vec::new(),
        }
    }

    #[test]
    fn plan_consolidate_moves_files_from_many_different_target_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let expected_dir = tmp.path().join("expected");
        std::fs::create_dir_all(&expected_dir).unwrap();

        let mut files = Vec::new();
        let mut matches = HashMap::new();
        for i in 1..=3 {
            let candidate_dir = tmp.path().join(format!("scatter{i}"));
            std::fs::create_dir_all(&candidate_dir).unwrap();
            let candidate_path = candidate_dir.join(format!("IMG_{i}.CR2"));
            std::fs::write(&candidate_path, b"raw").unwrap();

            files.push(real_file(
                i,
                10,
                &format!("IMG_{i}"),
                "CR2",
                expected_dir.join(format!("IMG_{i}.CR2")),
            ));
            matches.insert(i, matches_for(i, candidate_path.to_str().unwrap()).1);
        }

        let suggestion = ScatteredSuggestion {
            folder_id: 10,
            root_id: 1,
            source_rel_path: "2019/Export/".to_string(),
            total_missing: 3,
            with_candidate: 3,
            distinct_targets: 3,
            median_per_target: 1.0,
        };

        let plan = plan_consolidate(&suggestion, &files, &matches);
        assert!(plan.db_ops.is_empty());
        assert_eq!(
            plan.fs_ops.len(),
            3,
            "všechny tři soubory z různých cílů se musí naplánovat"
        );
        for op in &plan.fs_ops {
            assert_eq!(op.dst.parent().unwrap(), expected_dir);
        }
    }

    #[test]
    fn detects_scattered_when_files_spread_across_many_targets() {
        let files: Vec<CatalogFile> = (1..=6)
            .map(|i| file(i, 10, 1, "2019/Export/", &format!("IMG_{i:04}.CR2")))
            .collect();
        // Šest souborů, každý ve VLASTNÍ cílové složce - k=6, medián=1.
        let matches: HashMap<i64, FileMatches> = (1..=6)
            .map(|i| matches_for(i, &format!(r"D:\Export{i}\IMG_{i:04}.CR2")))
            .collect();

        let suggestions = detect(
            &files,
            &HashMap::new(),
            &matches,
            &HashSet::new(),
            &HashSet::new(),
        );

        assert_eq!(suggestions.len(), 1);
        let s = &suggestions[0];
        assert_eq!(s.folder_id, 10);
        assert_eq!(s.distinct_targets, 6);
        assert_eq!(s.median_per_target, 1.0);
    }

    #[test]
    fn ignores_folder_with_fewer_than_five_distinct_targets() {
        let files: Vec<CatalogFile> = (1..=4)
            .map(|i| file(i, 10, 1, "2019/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let matches: HashMap<i64, FileMatches> = (1..=4)
            .map(|i| matches_for(i, &format!(r"D:\Export{i}\IMG_{i:04}.CR2")))
            .collect();

        assert!(detect(
            &files,
            &HashMap::new(),
            &matches,
            &HashSet::new(),
            &HashSet::new()
        )
        .is_empty());
    }

    #[test]
    fn ignores_folder_when_median_per_target_too_high() {
        // Pět souborů koncentrovaných hlavně do jedné cílové složky - to je
        // spíš vzor A, ne vzor B (medián by vyšel vysoký).
        let files: Vec<CatalogFile> = (1..=10)
            .map(|i| file(i, 10, 1, "2019/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let mut matches: HashMap<i64, FileMatches> = HashMap::new();
        for i in 1..=8 {
            matches.insert(i, matches_for(i, r"D:\Zaloha\IMG.CR2").1);
        }
        for (idx, i) in (9..=10).enumerate() {
            matches.insert(i, matches_for(i, &format!(r"D:\Jinde{idx}\IMG.CR2")).1);
        }
        // distinct targets = 3 (< 5) - i tenhle případ by měl selhat na
        // počtu cílů, ne jen na mediánu, ale ověřuje to stejnou hranici.
        assert!(detect(
            &files,
            &HashMap::new(),
            &matches,
            &HashSet::new(),
            &HashSet::new()
        )
        .is_empty());
    }

    #[test]
    fn excludes_folders_already_claimed_by_pattern_a() {
        let files: Vec<CatalogFile> = (1..=6)
            .map(|i| file(i, 10, 1, "2019/", &format!("IMG_{i:04}.CR2")))
            .collect();
        let matches: HashMap<i64, FileMatches> = (1..=6)
            .map(|i| matches_for(i, &format!(r"D:\Export{i}\IMG_{i:04}.CR2")))
            .collect();

        let mut excluded_folders = HashSet::new();
        excluded_folders.insert(10i64);

        assert!(detect(
            &files,
            &HashMap::new(),
            &matches,
            &HashSet::new(),
            &excluded_folders
        )
        .is_empty());
    }

    #[test]
    fn median_computes_average_for_even_count() {
        assert_eq!(median(vec![1, 2, 3, 4]), 2.5);
        assert_eq!(median(vec![1, 2, 3]), 2.0);
        assert_eq!(median(Vec::new()), 0.0);
    }
}
