//! Journal (PROJECT.md §9): jeden JSON objekt na řádek, append-only.
//!
//! CLAUDE.md invariant 9: „Každá změna, kterou nástroj provede, musí být
//! v journalu PŘED tím, než se provede." [`JournalWriter::record`] proto po
//! zápisu volá `sync_data()` - teprve po návratu smí volající operaci
//! skutečně provést.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JournalState {
    /// Naplánováno, ještě se neprovedlo (fáze 0.5 / před `BEGIN IMMEDIATE`).
    Planned,
    /// Operace proběhla a je potvrzená (DB `COMMIT`, nebo obecný úspěch).
    Done,
    /// Skupina souborových přesunů úspěšně dokončena (fáze 1.2).
    FsDone,
    /// Skupina souborových přesunů selhala - vyřazena z DB plánu (fáze 1.3).
    FsFailed,
    /// Kompenzační fáze vrátila přesun zpět po selhání DB commitu (fáze 3).
    Compensated,
    /// Kompenzace se nepodařila - vyžaduje ruční zásah (fáze 3.2).
    CompensationFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum JournalOp {
    DbUpdate {
        table: String,
        id_local: i64,
        before: serde_json::Value,
        after: serde_json::Value,
    },
    DbInsert {
        table: String,
        id_local: i64,
        after: serde_json::Value,
    },
    FileMove {
        src: PathBuf,
        dst: PathBuf,
        size: u64,
        /// Sekundy od Unix epochy.
        mtime: i64,
        /// `None` u přesunů v rámci svazku (atomický rename nepotřebuje
        /// hash-verifikaci) - vyplněné jen u kopií přes svazky (§16.2).
        hash_blake3: Option<String>,
        /// Společné pro celou skupinu (soubor + sidecary, §3.5). `None`,
        /// dokud grupování nezavede M6.
        group_id: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    pub ts: String,
    pub session: String,
    pub seq: u64,
    pub state: JournalState,
    #[serde(flatten)]
    pub op: JournalOp,
}

#[derive(Debug, Error)]
pub enum JournalError {
    #[error("chyba I/O při práci s journalem: {0}")]
    Io(#[from] std::io::Error),
    #[error("journal je poškozený na řádku {line}: {source}")]
    Corrupt {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("nejde serializovat journal záznam: {0}")]
    Serialize(#[from] serde_json::Error),
}

pub struct JournalWriter {
    file: File,
    session: String,
    next_seq: u64,
}

impl JournalWriter {
    /// Otevře (nebo založí) `path` v append módu pro danou session.
    /// `next_seq` navazuje na existující obsah, aby čísla `seq` zůstala
    /// monotónní i po znovuotevření stejného souboru.
    pub fn open(path: &Path, session: impl Into<String>) -> Result<Self, JournalError> {
        let existing_max_seq = if path.exists() {
            read_all(path)?.iter().map(|e| e.seq).max().unwrap_or(0)
        } else {
            0
        };
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file,
            session: session.into(),
            next_seq: existing_max_seq + 1,
        })
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// Zapíše jeden záznam a počká, až je fyzicky na disku. Volající smí
    /// provést operaci, kterou záznam popisuje, teprve PO úspěšném návratu.
    pub fn record(&mut self, state: JournalState, op: JournalOp) -> Result<u64, JournalError> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let entry = JournalEntry {
            ts: chrono::Utc::now().to_rfc3339(),
            session: self.session.clone(),
            seq,
            state,
            op,
        };
        let mut line = serde_json::to_string(&entry)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.sync_data()?;
        Ok(seq)
    }
}

/// Přečte celý journal. Neúplný POSLEDNÍ řádek (typicky stopa přerušeného
/// zápisu při pádu procesu) se tiše přeskočí - invariant 9 zaručuje, že
/// operace, kterou by popisoval, se tedy nikdy neprovedla. Poškozený řádek
/// KDEKOLI JINDE je vážnější (naznačuje poškození souboru) a vrátí chybu.
pub fn read_all(path: &Path) -> Result<Vec<JournalEntry>, JournalError> {
    let content = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let mut entries = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        match serde_json::from_str::<JournalEntry>(line) {
            Ok(entry) => entries.push(entry),
            Err(source) => {
                if i == lines.len() - 1 {
                    tracing::warn!(%source, "poslední řádek journalu je neúplný, přeskakuji");
                } else {
                    return Err(JournalError::Corrupt {
                        line: i + 1,
                        source,
                    });
                }
            }
        }
    }
    Ok(entries)
}

/// Jen záznamy dané session, v pořadí podle `seq`.
pub fn read_session(path: &Path, session: &str) -> Result<Vec<JournalEntry>, JournalError> {
    let mut entries: Vec<JournalEntry> = read_all(path)?
        .into_iter()
        .filter(|e| e.session == session)
        .collect();
    entries.sort_by_key(|e| e.seq);
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_are_persisted_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("journal.jsonl");
        let mut w = JournalWriter::open(&path, "sess1").unwrap();

        w.record(
            JournalState::Planned,
            JournalOp::FileMove {
                src: PathBuf::from("a"),
                dst: PathBuf::from("b"),
                size: 10,
                mtime: 0,
                hash_blake3: None,
                group_id: None,
            },
        )
        .unwrap();
        w.record(
            JournalState::Done,
            JournalOp::DbUpdate {
                table: "AgLibraryFile".to_string(),
                id_local: 1,
                before: serde_json::json!({"folder": 1}),
                after: serde_json::json!({"folder": 2}),
            },
        )
        .unwrap();

        let entries = read_session(&path, "sess1").unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].seq, 1);
        assert_eq!(entries[1].seq, 2);
        assert_eq!(entries[0].state, JournalState::Planned);
        assert_eq!(entries[1].state, JournalState::Done);
    }

    #[test]
    fn reopening_continues_seq_numbering() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("journal.jsonl");
        {
            let mut w = JournalWriter::open(&path, "sess1").unwrap();
            w.record(
                JournalState::Done,
                JournalOp::DbInsert {
                    table: "AgLibraryFolder".to_string(),
                    id_local: 1,
                    after: serde_json::json!({}),
                },
            )
            .unwrap();
        }
        let mut w2 = JournalWriter::open(&path, "sess2").unwrap();
        let seq = w2
            .record(
                JournalState::Done,
                JournalOp::DbInsert {
                    table: "AgLibraryFolder".to_string(),
                    id_local: 2,
                    after: serde_json::json!({}),
                },
            )
            .unwrap();
        assert_eq!(seq, 2);
    }

    #[test]
    fn filters_by_session() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("journal.jsonl");
        let mut w1 = JournalWriter::open(&path, "sess1").unwrap();
        w1.record(
            JournalState::Done,
            JournalOp::DbInsert {
                table: "T".to_string(),
                id_local: 1,
                after: serde_json::json!({}),
            },
        )
        .unwrap();
        let mut w2 = JournalWriter::open(&path, "sess2").unwrap();
        w2.record(
            JournalState::Done,
            JournalOp::DbInsert {
                table: "T".to_string(),
                id_local: 2,
                after: serde_json::json!({}),
            },
        )
        .unwrap();

        assert_eq!(read_session(&path, "sess1").unwrap().len(), 1);
        assert_eq!(read_session(&path, "sess2").unwrap().len(), 1);
    }

    #[test]
    fn truncated_last_line_is_skipped_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("journal.jsonl");
        {
            let mut w = JournalWriter::open(&path, "sess1").unwrap();
            w.record(
                JournalState::Done,
                JournalOp::DbInsert {
                    table: "T".to_string(),
                    id_local: 1,
                    after: serde_json::json!({}),
                },
            )
            .unwrap();
        }
        // Simuluj pád procesu uprostřed zápisu druhého řádku.
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"{\"ts\":\"2026-01-01T00:00:00Z\",\"seq\":2,\"sess")
            .unwrap();

        let entries = read_all(&path).unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn corrupt_line_in_the_middle_is_a_hard_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("journal.jsonl");
        std::fs::write(&path, "not json at all\n{\"ts\":\"x\",\"session\":\"s\",\"seq\":1,\"state\":\"done\",\"op\":\"db_insert\",\"table\":\"T\",\"id_local\":1,\"after\":{}}\n").unwrap();

        let err = read_all(&path).unwrap_err();
        assert!(matches!(err, JournalError::Corrupt { line: 1, .. }));
    }
}
