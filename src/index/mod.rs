//! INDEX (PROJECT.md §5.2): postaví `NameIndex` - mapu `lowercase(NFC) jméno
//! souboru → seznam absolutních cest`, ze které MATCH (§5.3) hledá kandidáty
//! pro chybějící soubory.
//!
//! Tři backendy z §5.2: A) Everything IPC, B) vlastní MFT scan, C) paralelní
//! walk. **M2 implementuje A) a C).** B) je vyloženě pro M7 (vyžaduje
//! elevaci a `unsafe` izolovaný v `index/mft.rs`, který zatím neexistuje).

pub mod cache;
pub mod everything;
pub mod mft;
pub mod walk;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use smallvec::SmallVec;

use crate::fsops::paths::{match_key, match_key_str};

/// `lowercase(NFC) jméno → cesty` (PROJECT.md §5.2). `SmallVec<[PathBuf; 2]>`
/// - drtivá většina jmen má 0-2 kandidáty, jen výjimečně víc.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NameIndex {
    pub by_name: HashMap<String, SmallVec<[PathBuf; 2]>>,
}

impl NameIndex {
    pub fn insert(&mut self, path: PathBuf) {
        let Some(name) = path.file_name() else { return };
        let key = match_key(name);
        self.by_name.entry(key).or_default().push(path);
    }

    pub fn candidates(&self, filename: &str) -> &[PathBuf] {
        self.by_name
            .get(&match_key_str(filename))
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    pub fn len(&self) -> usize {
        self.by_name.values().map(|v| v.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexBackend {
    Everything,
    /// Backend B, §5.2 - jen s elevovaným tokenem (`mft::is_process_elevated`).
    Mft,
    Walk,
}

pub struct IndexResult {
    pub index: NameIndex,
    pub backend: IndexBackend,
}

/// Jak dlouho je cache walk indexu (§5.2 "s TTL") považovaná za platnou, než
/// se radši znovu projde disk. Walk nad celým svazkem může trvat desítky
/// sekund (viz `index/walk.rs`) - bez cache by ho "Ověřit znovu" spouštělo
/// pokaždé znovu.
const WALK_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);

/// Postaví index v pořadí backendů z §5.2: A) Everything (rychlé, cílené
/// dotazy jen na `missing_filenames`), B) MFT scan (jen s elevovaným
/// tokenem, §5.2 „Elevace"), C) paralelní walk nad `roots` (typicky svazky,
/// kde leží kořeny katalogu) jako poslední záchrana.
///
/// `cache_path`, pokud je zadaná, se použije pro B i C - oba stavějí úplný
/// index celého svazku nezávisle na tom, co zrovna hledáme, takže je
/// smysluplně znovupoužitelný mezi voláními (M7 „cache indexu i pro MFT
/// backend"). Cache neví, kterým z obou backendů vznikla - obojí je stejný
/// tvar dat, takže se to nerozlišuje. Everything dotazuje jen konkrétní
/// `missing_filenames`, takže cachovat nemá smysl (bylo by to zavádějící
/// při jiné sadě chybějících souborů).
pub fn build_index(
    missing_filenames: &std::collections::HashSet<String>,
    roots: &[PathBuf],
    exclude: &[String],
    cancel: &std::sync::atomic::AtomicBool,
    cache_path: Option<&Path>,
) -> IndexResult {
    if let Some(index) = everything::try_index(missing_filenames, exclude) {
        tracing::info!(backend = "everything", count = index.len(), "INDEX hotovo");
        return IndexResult {
            index,
            backend: IndexBackend::Everything,
        };
    }

    if let Some(path) = cache_path {
        if let Some(index) = cache::load(path, WALK_CACHE_TTL) {
            tracing::info!(backend = "index-cache", count = index.len(), "INDEX hotovo");
            return IndexResult {
                index,
                backend: IndexBackend::Walk,
            };
        }
    }

    if mft::is_process_elevated() {
        match mft::build_index(roots, exclude, cancel) {
            Ok(index) => {
                tracing::info!(backend = "mft", count = index.len(), "INDEX hotovo");
                if let Some(path) = cache_path {
                    if let Err(err) = cache::save(&index, path) {
                        tracing::warn!(%err, "nejde uložit index cache - pokračuji bez ní");
                    }
                }
                return IndexResult {
                    index,
                    backend: IndexBackend::Mft,
                };
            }
            Err(err) => {
                // §5.2: "tichý fallback na C), ne chybová hláška" - do logu
                // ano (podezřelé/neobvyklé - proto warn), uživateli se
                // ohlásí jen výsledný backend "walk", bez dramatu.
                tracing::warn!(%err, "MFT scan se nepovedl, padám na walk");
            }
        }
    }

    let index = walk::build_index(roots, exclude, cancel);
    tracing::info!(backend = "walk", count = index.len(), "INDEX hotovo");
    if let Some(path) = cache_path {
        if let Err(err) = cache::save(&index, path) {
            tracing::warn!(%err, "nejde uložit index cache - pokračuji bez ní");
        }
    }
    IndexResult {
        index,
        backend: IndexBackend::Walk,
    }
}

/// Ověří, jestli `path` (dopředná lomítka) odpovídá některému vylučovacímu
/// vzoru z `config.toml` §14. Vzor bez `*` se chová jako jméno segmentu
/// cesty (`$RECYCLE.BIN` vyloučí tuhle složku kdekoli ve stromu); vzor s `*`
/// jako standardní wildcard nad celou cestou.
pub fn is_excluded(path_forward_slash: &str, patterns: &[String]) -> bool {
    let lower = path_forward_slash.to_lowercase();
    patterns.iter().any(|pattern| {
        let pattern_lower = pattern.to_lowercase();
        if pattern_lower.contains('*') {
            wildcard_match(&pattern_lower, &lower)
        } else {
            lower.split('/').any(|segment| segment == pattern_lower)
        }
    })
}

/// Klasické `*`-only wildcard porovnání (žádné `?`), lineární dvouukazatelový
/// algoritmus. Vstupy se předpokládají už lowercase.
fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None;
    let mut match_from = 0usize;

    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            match_from = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(si) = star {
            pi = si + 1;
            match_from += 1;
            ti = match_from;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Odvodí defaultní walk kořeny (§5.2 bod C: "svazky, kde leží
/// `AgLibraryRootFolder.absolutePath`") - kořen svazku každé existující
/// (nebo úspěšně přemapované) cesty katalogu, bez duplicit.
pub fn default_walk_roots(catalog_absolute_paths: &[String]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for absolute_path in catalog_absolute_paths {
        let native = absolute_path.replace('/', "\\");
        if let Some(drive) = volume_root_of(Path::new(&native)) {
            if !roots.contains(&drive) {
                roots.push(drive);
            }
        }
    }
    roots
}

#[cfg(windows)]
fn volume_root_of(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some(PathBuf::from(format!("{}:\\", letter as char)))
            }
            _ => None,
        },
        _ => None,
    }
}

#[cfg(not(windows))]
fn volume_root_of(_path: &Path) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_index_insert_and_lookup_is_case_and_nfc_insensitive() {
        let mut idx = NameIndex::default();
        idx.insert(PathBuf::from(r"D:\Zaloha\IMG_1234.CR2"));
        assert_eq!(
            idx.candidates("img_1234.cr2"),
            [PathBuf::from(r"D:\Zaloha\IMG_1234.CR2")]
        );
        assert_eq!(idx.candidates("IMG_1234.CR2").len(), 1);
        assert!(idx.candidates("nope.cr2").is_empty());
    }

    #[test]
    fn bare_pattern_matches_path_segment_anywhere() {
        let patterns = vec!["$RECYCLE.BIN".to_string()];
        assert!(is_excluded("C:/Foto/$RECYCLE.BIN/x.CR2", &patterns));
        assert!(!is_excluded("C:/Foto/2019/x.CR2", &patterns));
    }

    #[test]
    fn wildcard_pattern_matches_anywhere_in_path() {
        let patterns = vec!["*/Backups/*".to_string()];
        assert!(is_excluded(
            "C:/Foto/Lightroom/Backups/2019/cat.lrcat",
            &patterns
        ));
        assert!(!is_excluded("C:/Foto/2019/x.CR2", &patterns));
    }

    #[test]
    fn exclude_matching_is_case_insensitive() {
        let patterns = vec!["*/backups/*".to_string()];
        assert!(is_excluded("C:/Foto/BACKUPS/x.CR2", &patterns));
    }

    #[test]
    fn build_index_writes_and_then_reuses_the_walk_cache() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("IMG_0001.CR2"), b"x").unwrap();
        let cache_path = tmp.path().join("index.bin");
        let cancel = std::sync::atomic::AtomicBool::new(false);

        let first = build_index(
            &std::collections::HashSet::new(),
            &[tmp.path().to_path_buf()],
            &[],
            &cancel,
            Some(&cache_path),
        );
        assert_eq!(first.backend, IndexBackend::Walk);
        assert!(cache_path.exists());
        assert!(!first.index.candidates("IMG_0001.CR2").is_empty());

        // Druhé volání dostane jiný (prázdný) kořen - pokud by nepoužilo
        // cache, nic by nenašlo.
        let second = build_index(
            &std::collections::HashSet::new(),
            &[],
            &[],
            &cancel,
            Some(&cache_path),
        );
        assert!(!second.index.candidates("IMG_0001.CR2").is_empty());
    }

    #[test]
    fn default_walk_roots_deduplicates_by_drive() {
        let roots = default_walk_roots(&[
            "C:/Foto/".to_string(),
            "C:/Users/pavel/Pictures/".to_string(),
        ]);
        #[cfg(windows)]
        assert_eq!(roots, vec![PathBuf::from(r"C:\")]);
    }
}
