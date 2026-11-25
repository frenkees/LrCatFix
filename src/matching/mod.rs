//! MATCH orchestrace: INDEX (§5.2) → kalibrace času (§5.4) → skórování
//! (§5.3), spuštěné dohromady jako jeden krok. Sdílené mezi CLI (`main.rs`)
//! a GUI (`app/state.rs`), stejně jako `cluster::root_remap::probe_all`.

pub mod calibrate;
pub mod score;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::SystemTime;

use crate::catalog::extract::CatalogFile;
use crate::config::MatchConfig;
use crate::fsops::verify::FileStatus;
use crate::index::{self, IndexBackend};

pub struct MatchRun {
    pub index_backend: IndexBackend,
    pub index_size: usize,
    pub calibration: calibrate::Calibration,
    /// Klíčováno `file_id` - jen soubory, které VERIFY označila jako
    /// nevyřešené (`Missing` / `FolderMissing`).
    pub matches: HashMap<i64, score::FileMatches>,
}

/// `walk_roots` typicky z [`crate::index::default_walk_roots`]. `cancel` se
/// kontroluje uvnitř INDEX fáze (walk backend) - CLI ho může nechat vždy
/// `false`, GUI ho propojí s tlačítkem "Zrušit". `catalog_path` určuje, kam
/// se uloží cache walk indexu (`<katalog>.index-cache.bin`, §5.2 „cachovat
/// na disk").
pub fn run(
    all_files: &[CatalogFile],
    statuses: &HashMap<i64, FileStatus>,
    walk_roots: &[PathBuf],
    config: &MatchConfig,
    exclude: &[String],
    cancel: &AtomicBool,
    catalog_path: &Path,
) -> MatchRun {
    let missing: Vec<&CatalogFile> = all_files
        .iter()
        // `is_repairable()`, ne `!= Ok` - soubory na nedostupném svazku
        // (FolderUnavailable) se nesmí hledat, o nich nevíme nic (§5.1).
        .filter(|f| status_of(statuses, f.file_id).is_repairable())
        .collect();

    let missing_filenames: HashSet<String> = missing.iter().map(|f| f.filename.clone()).collect();
    let cache_path = index_cache_path(catalog_path);
    let index_result = index::build_index(
        &missing_filenames,
        walk_roots,
        exclude,
        cancel,
        Some(&cache_path),
    );

    let calibration = calibrate::calibrate(&calibration_samples(all_files, statuses));

    let matches = score::match_all(
        &missing,
        all_files,
        &index_result.index,
        &calibration,
        config.folder_name_similarity,
    )
    .into_iter()
    .map(|m| (m.file_id, m))
    .collect();

    MatchRun {
        index_backend: index_result.backend,
        index_size: index_result.index.len(),
        calibration,
        matches,
    }
}

fn index_cache_path(catalog_path: &Path) -> PathBuf {
    let mut name = catalog_path.as_os_str().to_os_string();
    name.push(".index-cache.bin");
    PathBuf::from(name)
}

fn status_of(statuses: &HashMap<i64, FileStatus>, file_id: i64) -> FileStatus {
    statuses
        .get(&file_id)
        .copied()
        .unwrap_or(FileStatus::Missing)
}

/// Vzorky pro kalibraci (§5.4 krok 1): až 200 existujících souborů s platnou
/// `externalModTime` hodnotou.
fn calibration_samples(
    all_files: &[CatalogFile],
    statuses: &HashMap<i64, FileStatus>,
) -> Vec<(f64, SystemTime)> {
    all_files
        .iter()
        .filter(|f| status_of(statuses, f.file_id) == FileStatus::Ok)
        .filter_map(|f| {
            let raw = f.external_mod_time.as_ref()?.as_f64()?;
            let actual = std::fs::metadata(&f.expected_path).ok()?.modified().ok()?;
            Some((raw, actual))
        })
        .take(200)
        .collect()
}
