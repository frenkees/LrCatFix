//! Hashování obsahu souborů - MD5 (kompatibilita se sloupcem
//! `AgLibraryFile.md5`, PROJECT.md §5.3) - a BLAKE3 pro ověření kopie přes
//! svazky (`fsops::moves`, §16.2) žije přímo tam, protože je čistě interní.
//!
//! **Nikdy se nevolá na cloud placeholderu** (CLAUDE.md invariant 8 -
//! stažení stovek GB by bylo nevratné a nechtěné). Volající by to měl ověřit
//! přes `fsops::attrs` PŘEDEM, ale `md5_hex` to kontroluje i sám jako druhou
//! pojistku, protože důsledek chyby by byl nenávratné stažení dat.

use std::io;
use std::path::Path;

use md5::{Digest, Md5};
use thiserror::Error;

use crate::fsops::attrs::is_cloud_placeholder_path;
use crate::fsops::paths::to_verbatim_for_io;

#[derive(Debug, Error)]
pub enum HashError {
    #[error("nelze hashovat cloud placeholder (stáhlo by se tím z cloudu): {0}")]
    CloudPlaceholder(String),
    #[error("chyba I/O při hashování: {0}")]
    Io(#[from] io::Error),
}

/// MD5 hash souboru jako lowercase hex řetězec.
pub fn md5_hex(path: &Path) -> Result<String, HashError> {
    if is_cloud_placeholder_path(path) {
        return Err(HashError::CloudPlaceholder(path.display().to_string()));
    }
    let mut file = std::fs::File::open(to_verbatim_for_io(path))?;
    let mut hasher = Md5::new();
    io::copy(&mut file, &mut hasher)?;
    Ok(bytes_to_hex(&hasher.finalize()))
}

fn bytes_to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_known_value() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.bin");
        std::fs::write(&file, b"hello world").unwrap();
        // Známá MD5 hodnota "hello world" (ověřitelná nezávisle: `md5sum`).
        assert_eq!(md5_hex(&file).unwrap(), "5eb63bbbe01eeed093cb22bb8f5acdc3");
    }

    #[test]
    fn empty_file_has_the_well_known_empty_md5() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("empty.bin");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(md5_hex(&file).unwrap(), "d41d8cd98f00b204e9800998ecf8427e");
    }
}
