//! Vzor C - album rozházené po discích (PROJECT.md §7.3, `ALBUM_SCATTERED`).
//!
//! Na rozdíl od vzorů A/B/D pracuje nad KOLEKCEMI
//! (`catalog::extract::CollectionMember`), ne nad zdrojovou složkou. Schéma
//! pro kolekce NENÍ ověřené na reálném katalogu (CLAUDE.md §6) -
//! `catalog::schema::has_collection_schema` detekci vypne úplně, když
//! tabulky/sloupce nesedí, místo aby nad neověřeným tvarem hádala.
//!
//! **Vědomě mimo rozsah (M6):** fyzické sloučení (přesun). PROJECT.md §7.3
//! to výslovně označuje za nejdestruktivnější operaci v nástroji ("vyžaduj
//! explicitní dvojí potvrzení a nikdy ji nezahrnuj do Auto"). M6 proto dělá
//! jen DETEKCI, zobrazenou v panelu návrhů jako informace bez akčního
//! tlačítka - spouštěcí dialog se dvojím potvrzením přijde později, až bude
//! jasné, kam přesně by se "adresář pojmenovaný podle kolekce" měl zakládat
//! (otevřená otázka, kterou spec sama nedořešuje).

use std::collections::{HashMap, HashSet};

use crate::catalog::extract::{CatalogFile, CollectionMember};
use crate::fsops::verify::FileStatus;

const MIN_DISTINCT_FOLDERS: usize = 10;
const MIN_MISSING_RATIO: f64 = 0.3;

#[derive(Debug, Clone)]
pub struct AlbumScatteredSuggestion {
    pub collection_id: i64,
    pub collection_name: String,
    pub total_files: usize,
    pub missing_files: usize,
    pub distinct_folders: usize,
}

impl AlbumScatteredSuggestion {
    pub fn missing_ratio(&self) -> f64 {
        if self.total_files == 0 {
            0.0
        } else {
            self.missing_files as f64 / self.total_files as f64
        }
    }
}

/// Detekuje vzor C. `members` je výstup `catalog::extract::collection_membership`
/// (prázdné, pokud `has_collection_schema` vrátilo `false` - volající pak
/// tuhle funkci ani nemusí volat, výsledek by stejně byl prázdný).
pub fn detect(
    members: &[CollectionMember],
    all_files: &[CatalogFile],
    statuses: &HashMap<i64, FileStatus>,
) -> Vec<AlbumScatteredSuggestion> {
    let folder_by_file: HashMap<i64, i64> =
        all_files.iter().map(|f| (f.file_id, f.folder_id)).collect();

    let mut by_collection: HashMap<i64, (String, Vec<i64>)> = HashMap::new();
    for m in members {
        by_collection
            .entry(m.collection_id)
            .or_insert_with(|| (m.collection_name.clone(), Vec::new()))
            .1
            .push(m.file_id);
    }

    let mut suggestions: Vec<AlbumScatteredSuggestion> = by_collection
        .into_iter()
        .filter_map(|(collection_id, (name, file_ids))| {
            detect_one(collection_id, name, &file_ids, &folder_by_file, statuses)
        })
        .collect();

    suggestions.sort_by_key(|s| std::cmp::Reverse(s.missing_files));
    suggestions
}

fn detect_one(
    collection_id: i64,
    collection_name: String,
    file_ids: &[i64],
    folder_by_file: &HashMap<i64, i64>,
    statuses: &HashMap<i64, FileStatus>,
) -> Option<AlbumScatteredSuggestion> {
    let total_files = file_ids.len();
    if total_files == 0 {
        return None;
    }

    let distinct_folders: HashSet<i64> = file_ids
        .iter()
        .filter_map(|id| folder_by_file.get(id).copied())
        .collect();
    if distinct_folders.len() < MIN_DISTINCT_FOLDERS {
        return None;
    }

    let missing_files = file_ids
        .iter()
        .filter(|id| status_of(statuses, **id).is_repairable())
        .count();

    let suggestion = AlbumScatteredSuggestion {
        collection_id,
        collection_name,
        total_files,
        missing_files,
        distinct_folders: distinct_folders.len(),
    };

    if suggestion.missing_ratio() >= MIN_MISSING_RATIO {
        Some(suggestion)
    } else {
        None
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
    use std::path::PathBuf;

    fn file(id: i64, folder_id: i64) -> CatalogFile {
        CatalogFile {
            file_id: id,
            folder_id,
            root_id: 1,
            root_path: "C:/Foto/".to_string(),
            rel_path: String::new(),
            filename: format!("IMG_{id}.CR2"),
            base_name: format!("IMG_{id}"),
            extension: "CR2".to_string(),
            expected_path: PathBuf::new(),
            external_mod_time: None,
            md5: None,
            file_size: None,
            sidecar_extensions: Vec::new(),
        }
    }

    fn member(collection_id: i64, name: &str, file_id: i64) -> CollectionMember {
        CollectionMember {
            collection_id,
            collection_name: name.to_string(),
            file_id,
        }
    }

    #[test]
    fn detects_when_scattered_across_many_folders_and_mostly_missing() {
        // 12 souborů, každý ve VLASTNÍ složce (12 distinct >= 10), 4 chybí
        // (4/12 = 33 % >= 30 %).
        let files: Vec<CatalogFile> = (1..=12).map(|i| file(i, i)).collect();
        let members: Vec<CollectionMember> =
            (1..=12).map(|i| member(1, "Nejlepsi 2020", i)).collect();
        let mut statuses = HashMap::new();
        for i in 1..=4 {
            statuses.insert(i, FileStatus::Missing);
        }
        for i in 5..=12 {
            statuses.insert(i, FileStatus::Ok);
        }

        let suggestions = detect(&members, &files, &statuses);
        assert_eq!(suggestions.len(), 1);
        let s = &suggestions[0];
        assert_eq!(s.collection_id, 1);
        assert_eq!(s.collection_name, "Nejlepsi 2020");
        assert_eq!(s.distinct_folders, 12);
        assert_eq!(s.missing_files, 4);
    }

    #[test]
    fn ignores_collection_with_fewer_than_ten_distinct_folders() {
        let files: Vec<CatalogFile> = (1..=12).map(|i| file(i, 1)).collect(); // všechny ve stejné složce
        let members: Vec<CollectionMember> = (1..=12).map(|i| member(1, "Album", i)).collect();
        let mut statuses = HashMap::new();
        for i in 1..=12 {
            statuses.insert(i, FileStatus::Missing);
        }

        assert!(detect(&members, &files, &statuses).is_empty());
    }

    #[test]
    fn ignores_collection_below_missing_ratio_threshold() {
        let files: Vec<CatalogFile> = (1..=12).map(|i| file(i, i)).collect();
        let members: Vec<CollectionMember> = (1..=12).map(|i| member(1, "Album", i)).collect();
        // Jen 1/12 (~8 %) chybí - pod prahem 30 %.
        let mut statuses = HashMap::new();
        statuses.insert(1, FileStatus::Missing);
        for i in 2..=12 {
            statuses.insert(i, FileStatus::Ok);
        }

        assert!(detect(&members, &files, &statuses).is_empty());
    }

    #[test]
    fn empty_members_produce_no_suggestions() {
        let files: Vec<CatalogFile> = Vec::new();
        assert!(detect(&[], &files, &HashMap::new()).is_empty());
    }
}
