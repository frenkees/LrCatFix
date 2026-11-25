//! Minimální session pro restart po UAC elevaci (PROJECT.md §5.2
//! "Elevace": "...předá session přes `--resume <session.json>`, aby
//! uživatel nepřišel o rozdělanou práci"). Vědomě NEukládá nic víc než
//! cestu ke katalogu - ostatní UI stav (filtry, výběr, rozpracované
//! dialogy) je buď odvoditelný znovu z katalogu, nebo tak pomíjivý, že
//! jeho ztráta při jednom restartu není problém. Zápis do katalogu vždy
//! vyžaduje samostatné explicitní potvrzení (CLAUDE.md §11), takže tenhle
//! restart nikdy nepřerušuje nic nevratného.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct ResumeSession {
    pub catalog_path: Option<PathBuf>,
}

#[derive(Debug, Error)]
pub enum SessionError {
    #[error("chyba I/O při práci se session souborem: {0}")]
    Io(#[from] std::io::Error),
    #[error("session soubor je poškozený: {0}")]
    Json(#[from] serde_json::Error),
}

pub fn write(path: &Path, session: &ResumeSession) -> Result<(), SessionError> {
    let json = serde_json::to_string_pretty(session)?;
    std::fs::write(path, json)?;
    Ok(())
}

pub fn read(path: &Path) -> Result<ResumeSession, SessionError> {
    let content = std::fs::read_to_string(path)?;
    Ok(serde_json::from_str(&content)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("session.json");
        let session = ResumeSession {
            catalog_path: Some(PathBuf::from(r"C:\Foto\Katalog.lrcat")),
        };
        write(&path, &session).unwrap();
        let loaded = read(&path).unwrap();
        assert_eq!(loaded, session);
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read(&tmp.path().join("neexistuje.json")).unwrap_err();
        assert!(matches!(err, SessionError::Io(_)));
    }
}
