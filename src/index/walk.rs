//! Backend C (PROJECT.md §5.2): paralelní rekurzivní walk přes `jwalk`.
//! Fallback pro ne-NTFS, síťové disky, Linux - a v M2 jediný backend, který
//! umí skenovat celý svazek (Everything backend jen cíleně dotazuje jména).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use jwalk::WalkDir;

use super::{is_excluded, NameIndex};

/// Projde všechny `roots` a vrátí `NameIndex` všech nalezených souborů mimo
/// vyloučené cesty (`exclude`, PROJECT.md §14). Zrušitelné přes `cancel`
/// (CLAUDE.md „Souběžnost" - uživatel musí umět zastavit skenování): kontrola
/// probíhá po každé nalezené položce, takže zastavení je rychlé, i když pár
/// vláken na pozadí ještě dočte rozečtenou složku.
pub fn build_index(roots: &[PathBuf], exclude: &[String], cancel: &AtomicBool) -> NameIndex {
    let mut index = NameIndex::default();
    for root in roots {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        walk_one_root(root, exclude, cancel, &mut index);
    }
    index
}

fn walk_one_root(root: &Path, exclude: &[String], cancel: &AtomicBool, index: &mut NameIndex) {
    let exclude_owned = exclude.to_vec();

    let walker = WalkDir::new(root).skip_hidden(false).process_read_dir(
        move |_depth, _path, _state, children| {
            // Vyřazením z `children` se vyloučená složka ani nenačte - ne jen
            // "nezobrazí ve výsledku" (§14: "Vyloučení patří do indexeru").
            children.retain(|entry_result| match entry_result {
                Ok(entry) => {
                    let forward = entry.path().to_string_lossy().replace('\\', "/");
                    !is_excluded(&forward, &exclude_owned)
                }
                // Chybu (např. odepřený přístup) nechme projít - promítne se do
                // walku jako Err a níže se tiše přeskočí, radši než aby jedna
                // nečitelná složka spadla celý index.
                Err(_) => true,
            });
        },
    );

    for entry in walker {
        if cancel.load(Ordering::Relaxed) {
            return;
        }
        let Ok(entry) = entry else { continue };
        if entry.file_type.is_file() {
            index.insert(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_files_and_skips_excluded_directories() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("Foto")).unwrap();
        std::fs::write(tmp.path().join("Foto").join("IMG_0001.CR2"), b"x").unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules").join("pkg")).unwrap();
        std::fs::write(
            tmp.path()
                .join("node_modules")
                .join("pkg")
                .join("IMG_0002.CR2"),
            b"x",
        )
        .unwrap();

        let exclude = vec!["*/node_modules/*".to_string()];
        let cancel = AtomicBool::new(false);
        let index = build_index(&[tmp.path().to_path_buf()], &exclude, &cancel);

        assert!(!index.candidates("IMG_0001.CR2").is_empty());
        assert!(index.candidates("IMG_0002.CR2").is_empty());
    }

    #[test]
    fn cancelled_before_start_returns_empty_index() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("IMG_0001.CR2"), b"x").unwrap();

        let cancel = AtomicBool::new(true);
        let index = build_index(&[tmp.path().to_path_buf()], &[], &cancel);
        assert!(index.is_empty());
    }

    #[test]
    fn indexes_multiple_roots() {
        let tmp1 = tempfile::tempdir().unwrap();
        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::write(tmp1.path().join("a.CR2"), b"x").unwrap();
        std::fs::write(tmp2.path().join("b.CR2"), b"x").unwrap();

        let cancel = AtomicBool::new(false);
        let index = build_index(
            &[tmp1.path().to_path_buf(), tmp2.path().to_path_buf()],
            &[],
            &cancel,
        );

        assert!(!index.candidates("a.CR2").is_empty());
        assert!(!index.candidates("b.CR2").is_empty());
    }
}
