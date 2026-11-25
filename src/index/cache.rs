//! Cache indexu na disku (`index.bin`, PROJECT.md §5.2) - `bincode`, s TTL a
//! možností „Refresh index" (volající prostě zavolá [`super::build_index`]
//! znovu a přepíše cache přes [`save`]).

use std::path::Path;
use std::time::{Duration, SystemTime};

use thiserror::Error;

use super::NameIndex;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("nejde zapsat index cache: {0}")]
    Io(#[from] std::io::Error),
    #[error("nejde serializovat index cache: {0}")]
    Encode(#[from] bincode::Error),
}

pub fn save(index: &NameIndex, path: &Path) -> Result<(), CacheError> {
    let bytes = bincode::serialize(index)?;
    std::fs::write(path, bytes)?;
    Ok(())
}

/// Načte cache, pokud existuje a není starší než `ttl`. Poškozená/nečitelná
/// cache se tiše ignoruje (`None`, volající pak index prostě znovu postaví) -
/// je to čistě read-only odvozenina z filesystému, takže tichý rebuild je
/// bezpečný. To NENÍ výjimka z CLAUDE.md invariantu o tichých fallbacích -
/// ten platí pro operace, které mění data katalogu/disku, a tahle žádná není.
pub fn load(path: &Path, ttl: Duration) -> Option<NameIndex> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata.modified().ok()?;
    if SystemTime::now().duration_since(modified).ok()? >= ttl {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    bincode::deserialize(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("index.bin");

        let mut index = NameIndex::default();
        index.insert(PathBuf::from(r"D:\Zaloha\IMG_1234.CR2"));
        save(&index, &cache_path).unwrap();

        let loaded = load(&cache_path, Duration::from_secs(3600)).unwrap();
        assert_eq!(
            loaded.candidates("IMG_1234.CR2"),
            [PathBuf::from(r"D:\Zaloha\IMG_1234.CR2")]
        );
    }

    #[test]
    fn expired_cache_is_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("index.bin");
        save(&NameIndex::default(), &cache_path).unwrap();

        assert!(load(&cache_path, Duration::from_secs(0)).is_none());
    }

    #[test]
    fn missing_cache_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(load(
            &tmp.path().join("does_not_exist.bin"),
            Duration::from_secs(3600)
        )
        .is_none());
    }

    #[test]
    fn corrupt_cache_is_ignored_not_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_path = tmp.path().join("index.bin");
        std::fs::write(&cache_path, b"not a valid bincode payload").unwrap();
        assert!(load(&cache_path, Duration::from_secs(3600)).is_none());
    }
}
