pub mod album_scattered;
pub mod folder_moved;
pub mod root_remap;
pub mod scattered;

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::catalog::apply::{FileMoveOp, Plan};
use crate::catalog::extract::{CatalogFile, RootFolder};
use crate::fsops::moves::{find_sidecars, independent_extensions_for};
use crate::fsops::verify::FileStatus;
use crate::matching::score::FileMatches;

/// Výsledek celé fáze CLUSTER (§7) - všechny čtyři vzory pohromadě.
#[derive(Debug, Default)]
pub struct ClusterResult {
    /// Vzor D (§7.4) - sonda svazků i odvození z MATCH dohromady.
    pub remap: Vec<root_remap::RemapSuggestion>,
    /// Vzor A (§7.1).
    pub folder_moved: Vec<folder_moved::FolderMovedSuggestion>,
    /// Vzor B (§7.2).
    pub scattered: Vec<scattered::ScatteredSuggestion>,
    /// Vzor C (§7.3) - prázdné, když katalog nemá schéma pro kolekce.
    pub album_scattered: Vec<album_scattered::AlbumScatteredSuggestion>,
}

impl ClusterResult {
    pub fn is_empty(&self) -> bool {
        self.remap.is_empty()
            && self.folder_moved.is_empty()
            && self.scattered.is_empty()
            && self.album_scattered.is_empty()
    }
}

/// Spustí detekci všech vzorů ve správném pořadí (§7.4 „Pořadí vyhodnocení:
/// D → A → B → C - každý vyřešený vzor odebere soubory z fronty pro nižší
/// úrovně").
///
/// **Sdílené mezi CLI a GUI** - dřív tahle orchestrace žila jen v
/// `app/state.rs`, takže `--headless` vzory A/B/C vůbec nepočítal a v CLI
/// reportu nebyly vidět. Jedna implementace znamená, že se obě cesty
/// nemůžou rozejít.
///
/// `probe_remap` je výsledek `root_remap::probe_all`, který se pouští JEŠTĚ
/// PŘED indexem (§7.4.1) - proto přichází jako parametr, ne aby se počítal
/// tady znovu.
pub fn detect_all(
    roots: &[RootFolder],
    files: &[CatalogFile],
    statuses: &HashMap<i64, FileStatus>,
    matches: &HashMap<i64, FileMatches>,
    probe_remap: Vec<root_remap::RemapSuggestion>,
    collection_members: &[crate::catalog::extract::CollectionMember],
) -> ClusterResult {
    let mut remap = probe_remap;

    // Vzor D, fallback z MATCH (§7.4.3) - jen pro kořeny, kde sonda nic
    // nenašla.
    let already: HashSet<i64> = remap.iter().map(|s| s.root_id).collect();
    remap.extend(root_remap::fallback_from_match(
        roots, files, statuses, matches, &already,
    ));

    // Kořeny s nevyřešeným vzorem D mají nespolehlivé cesty souborů pro
    // detekci na úrovni složky, proto se u A/B vynechají.
    let excluded_roots: HashSet<i64> = remap.iter().map(|s| s.root_id).collect();

    let folder_moved = folder_moved::detect(files, statuses, matches, &excluded_roots);
    let claimed: HashSet<i64> = folder_moved.iter().map(|s| s.folder_id).collect();
    let scattered = scattered::detect(files, statuses, matches, &excluded_roots, &claimed);
    let album_scattered = album_scattered::detect(collection_members, files, statuses);

    ClusterResult {
        remap,
        folder_moved,
        scattered,
        album_scattered,
    }
}

/// Zdrojová složka pro zobrazení uživateli.
///
/// `AgLibraryFolder.pathFromRoot` je pro soubory ležící přímo v kořeni
/// katalogu PRÁZDNÝ řetězec, což se do hlášky vysází jako `''` a vypadá to
/// jako chyba. Kořen se proto pojmenuje slovy.
pub fn display_source_path(rel_path: &str) -> String {
    if rel_path.trim().is_empty() {
        "(kořen katalogu)".to_string()
    } else {
        rel_path.to_string()
    }
}

/// Sdílené jádro pro vzor A, druhou možnost ("Přesun souborů zpět",
/// `folder_moved::plan_move_back`) i vzor B ("Konsolidace",
/// `scattered::plan_consolidate`) - PROJECT.md §7.1 bod 2 a §7.2: přesune
/// každý chybějící soubor z `folder_id`, který má nejlépe obodovaného
/// kandidáta (volitelně omezeného na `only_from_dir` - vzor A požaduje
/// jednu konkrétní cílovou složku, vzor B žádnou), zpátky na cestu, kterou
/// katalog už očekává (`file.expected_path`). `db_ops` je vždy prázdný -
/// katalog se nemění, jen disk se srovná s tím, co v katalogu je.
///
/// Detekce kolizí jmen v cíli se řeší stejně jako u každého jiného přesunu
/// (`catalog::apply` pre-flight, §8 bod 0.4 - „cílový soubor už existuje"
/// = odmítnutí a nahlášení, nikdy tiché přeskočení ani přepsání).
/// **Vědomě mimo rozsah:** strategie „přejmenovat `_1`" nebo „vybrat ručně"
/// z PROJECT.md §7.2 - to by vyžadovalo vlastní interaktivní dialog kolizí;
/// odmítnutí + nahlášení je bezpečné, i když míň pohodlné.
pub(crate) fn plan_return_to_expected_path(
    folder_id: i64,
    only_from_dir: Option<&Path>,
    group_prefix: &str,
    all_files: &[CatalogFile],
    matches: &HashMap<i64, FileMatches>,
) -> Plan {
    let mut fs_ops = Vec::new();

    for file in all_files.iter().filter(|f| f.folder_id == folder_id) {
        let Some(best) = matches
            .get(&file.file_id)
            .and_then(|m| m.candidates.first())
        else {
            continue;
        };
        let Some(candidate_dir) = best.path.parent() else {
            continue;
        };
        if let Some(only) = only_from_dir {
            if candidate_dir != only {
                continue;
            }
        }
        let Some(dst_dir) = file.expected_path.parent() else {
            continue;
        };

        let group_id = Some(format!("{group_prefix}-{}", file.file_id));
        fs_ops.push(FileMoveOp {
            src: best.path.clone(),
            dst: file.expected_path.clone(),
            group_id: group_id.clone(),
        });

        let independent = independent_extensions_for(all_files, file.folder_id, &file.base_name);
        for sidecar in find_sidecars(file, candidate_dir, &independent) {
            let Some(sidecar_name) = sidecar.path.file_name() else {
                continue;
            };
            fs_ops.push(FileMoveOp {
                src: sidecar.path.clone(),
                dst: dst_dir.join(sidecar_name),
                group_id: group_id.clone(),
            });
        }
    }

    Plan {
        fs_ops,
        db_ops: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::display_source_path;

    #[test]
    fn empty_rel_path_is_named_instead_of_shown_as_empty_quotes() {
        assert_eq!(display_source_path(""), "(kořen katalogu)");
        assert_eq!(display_source_path("   "), "(kořen katalogu)");
        assert_eq!(display_source_path("2019/HR/"), "2019/HR/");
    }
}
