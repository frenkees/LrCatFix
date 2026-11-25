//! APPLY - two-phase commit (PROJECT.md §8).
//!
//! Filesystém a SQLite nesdílejí transakci, takže SQL se generuje/spouští
//! až POTOM, co je jisté, které přesuny souborů doopravdy proběhly - nikdy
//! obráceně (CLAUDE.md invariant 6). Když fáze DATABÁZE selže, fáze
//! KOMPENZACE vrátí už provedené přesuny zpět, aby byl katalog nedotčený
//! (§8 fáze 3).
//!
//! DB operace: [`DbOpTemplate::RemapRoot`] (vzor D, §7.4 -
//! `UPDATE AgLibraryRootFolder`) z M3, case A/B/C z §3.3
//! (`UpdateFileFolder`/`InsertFolderAndUpdateFile`/`RenameFile`) z M4, a od
//! M5 [`DbOpTemplate::RemapFolder`] (vzor A, §7.1 - `UPDATE AgLibraryFolder`,
//! jeden update místo N updatů souborů). Všechny čistě DB (žádný fyzický
//! přesun souboru - INDEX/MATCH ho už našel tam, kde je, zápis jen
//! aktualizuje, kam katalog ukazuje). Skupiny se sidecary (§3.5) a CLUSTER
//! vzor C (ALBUM_SCATTERED, §7.3, vyžaduje fyzické přesuny) přibudou s M6.
//!
//! **Case B - vědomě mimo rozsah:** založení NOVÉHO `AgLibraryRootFolder`
//! (když neznámý je i celý kořen, ne jen podsložka). `InsertFolderAndUpdateFile`
//! vyžaduje existující root a odmítne, pokud neexistuje - kombinace nového
//! rootu s neověřeným globálním čítačem (`catalog::ids`) je zbytečné riziko
//! navíc, které spec nevyžaduje.

use std::path::{Path, PathBuf};

use rusqlite::TransactionBehavior;
use thiserror::Error;

use crate::catalog::backup::{self, BackupError, BackupResult};
use crate::catalog::open::{self, OpenWriteError};
use crate::fsops::moves::{self, MoveError};
use crate::fsops::paths::exists_verbatim;
use crate::journal::write::{JournalError, JournalOp, JournalState, JournalWriter};

#[derive(Debug, Clone)]
pub struct FileMoveOp {
    pub src: PathBuf,
    pub dst: PathBuf,
    /// Společné pro skupinu soubor+sidecary (§3.5). `None`, dokud grupování
    /// nezavede M6.
    pub group_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum DbOpTemplate {
    /// Vzor D (§7.4): `UPDATE AgLibraryRootFolder SET absolutePath = ...`.
    RemapRoot {
        root_id: i64,
        new_absolute_path: String,
    },
    /// Případ A (§3.3): soubor nalezen ve složce, kterou katalog už zná -
    /// `UPDATE AgLibraryFile SET folder = ...`.
    UpdateFileFolder { file_id: i64, new_folder_id: i64 },
    /// Případ B (§3.3): soubor nalezen v neznámé (ale existující) složce pod
    /// ZNÁMÝM rootem - založí `AgLibraryFolder`, pak jako A. `root_id` musí
    /// odkazovat na existující `AgLibraryRootFolder` (viz modul doc).
    InsertFolderAndUpdateFile {
        file_id: i64,
        root_id: i64,
        path_from_root: String,
    },
    /// Případ C (§3.3): přejmenování - `idx_filename`/`lc_idx_filename`/
    /// `baseName`/`extension`. `originalFilename` se nikdy nemění.
    RenameFile {
        file_id: i64,
        new_base_name: String,
        new_extension: String,
        new_idx_filename: String,
    },
    /// Vzor A (§7.1, "Repoint DB"): `UPDATE AgLibraryFolder SET
    /// pathFromRoot = ..., rootFolder = ...` - jeden update místo N updatů
    /// jednotlivých souborů. `new_root_id` musí odkazovat na existující
    /// `AgLibraryRootFolder` (stejné omezení jako u `InsertFolderAndUpdateFile`
    /// - založení nového rootu je mimo rozsah).
    RemapFolder {
        folder_id: i64,
        new_root_id: i64,
        new_path_from_root: String,
    },
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub fs_ops: Vec<FileMoveOp>,
    pub db_ops: Vec<DbOpTemplate>,
}

#[derive(Debug, Error)]
pub enum ApplyError {
    #[error("katalog vypadá otevřený v Lightroom Classic - zápis odmítnut")]
    CatalogLocked,
    #[error("záloha katalogu selhala, zápis se neprovede: {0}")]
    Backup(#[from] BackupError),
    #[error(transparent)]
    OpenWrite(#[from] OpenWriteError),
    #[error("chyba journalu: {0}")]
    Journal(#[from] JournalError),
    #[error("chyba I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("chyba SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug)]
pub struct FsPhaseResult {
    pub committed: Vec<FileMoveOp>,
    pub failed: Vec<(FileMoveOp, String)>,
    /// Vyřazeno už při pre-flightu (§8 bod 0.4) - vůbec se nezkoušelo.
    pub rejected: Vec<(FileMoveOp, String)>,
}

#[derive(Debug)]
pub enum DbPhaseResult {
    Committed,
    RolledBack {
        reason: String,
        compensation_failures: Vec<(PathBuf, String)>,
    },
}

#[derive(Debug)]
pub struct ApplyOutcome {
    pub session: String,
    pub backup: BackupResult,
    pub fs_phase: FsPhaseResult,
    pub db_phase: DbPhaseResult,
}

/// Celá APPLY session: fáze 0 (příprava + záloha) → fáze 1 (filesystém) →
/// fáze 2 (databáze, s automatickou kompenzací při selhání) → fáze 4
/// (dokončení - volající si podle [`ApplyOutcome`] udělá souhrn a znovu
/// spustí VERIFY nad dotčenými soubory).
pub fn apply(
    catalog_path: &Path,
    journal_path: &Path,
    session: &str,
    plan: Plan,
) -> Result<ApplyOutcome, ApplyError> {
    // Fáze 0.1: re-check zámků TĚSNĚ před zápisem (invariant 2) - `apply()`
    // volá `open::open_read_write` až uvnitř fáze 2, takže se zámky
    // kontrolují znovu i tady, dřív než se vůbec začne s drahou zálohou.
    if open::detect_lock(catalog_path).is_locked() {
        return Err(ApplyError::CatalogLocked);
    }

    // Fáze 0.2: záloha. Bez ní se nepokračuje (CLAUDE.md invariant 1).
    let backup_result = backup::create_backup(catalog_path)?;

    let mut journal = JournalWriter::open(journal_path, session)?;

    // Fáze 1: filesystém.
    let fs_phase = run_fs_phase(&plan.fs_ops, &mut journal)?;

    // Fáze 2: databáze (s kompenzací při selhání).
    let db_phase = run_db_phase(
        catalog_path,
        &mut journal,
        &plan.db_ops,
        &fs_phase.committed,
    )?;

    Ok(ApplyOutcome {
        session: session.to_string(),
        backup: backup_result,
        fs_phase,
        db_phase,
    })
}

/// Fáze 0.4 (pre-flight) + fáze 1 (přesuny). Veřejné, aby to
/// `tests/compensation.rs` mohl zavolat samostatně a mezi fázi 1 a 2 vložit
/// vynucené selhání (přesně postup, který CLAUDE.md §12 pro test
/// kompenzace popisuje).
///
/// Pracuje po SKUPINÁCH (`FileMoveOp::group_id`, §3.5): hlavní soubor +
/// sidecary jsou "atomická skupina" - selže-li kterýkoli člen, vrátí se
/// zpět celá skupina a do `failed`/`rejected` jde jako celek, ne jen ten
/// jeden soubor. Položky bez `group_id` tvoří každá svou vlastní
/// jednoprvkovou skupinu (nezávislý soubor).
pub fn run_fs_phase(
    fs_ops: &[FileMoveOp],
    journal: &mut JournalWriter,
) -> Result<FsPhaseResult, ApplyError> {
    let (valid, rejected) = preflight_fs_ops(fs_ops);

    for op in &valid {
        journal.record(JournalState::Planned, file_move_journal_op(op))?;
    }

    let mut committed = Vec::new();
    let mut failed = Vec::new();

    // CLAUDE.md „Souběžnost" + §8 fáze 1.4: neukončuj běh při první chybě -
    // 499 z 500 úspěšných skupin je platný výsledek.
    for group in group_by_group_id(valid) {
        run_one_group(group, journal, &mut committed, &mut failed)?;
    }

    Ok(FsPhaseResult {
        committed,
        failed,
        rejected,
    })
}

/// Seskupí podle `group_id` - `None` položky tvoří KAŽDÁ svou vlastní
/// jednoprvkovou skupinu (nezávislý soubor bez sidecarů).
fn group_by_group_id(ops: Vec<FileMoveOp>) -> Vec<Vec<FileMoveOp>> {
    let mut named: std::collections::BTreeMap<String, Vec<FileMoveOp>> =
        std::collections::BTreeMap::new();
    let mut groups: Vec<Vec<FileMoveOp>> = Vec::new();
    for op in ops {
        match &op.group_id {
            Some(id) => named.entry(id.clone()).or_default().push(op),
            None => groups.push(vec![op]),
        }
    }
    groups.extend(named.into_values());
    groups
}

/// Provede jednu skupinu. Selže-li kterýkoli člen, vrátí zpět ty, co v TÉTO
/// skupině už proběhly (v obráceném pořadí, stejný princip jako fáze 3 -
/// `compensate`), a CELOU skupinu zařadí do `failed`. Když se i vrácení
/// nepovede, soubor zůstává na novém místě a je to nahlas vidět ve zprávě o
/// selhání (nikdy tiše) - stejná zásada jako u kompenzace fáze DATABÁZE.
fn run_one_group(
    group: Vec<FileMoveOp>,
    journal: &mut JournalWriter,
    committed: &mut Vec<FileMoveOp>,
    failed: &mut Vec<(FileMoveOp, String)>,
) -> Result<(), ApplyError> {
    let mut done: Vec<(FileMoveOp, moves::MoveReport)> = Vec::new();

    for op in &group {
        match moves::move_file(&op.src, &op.dst) {
            Ok(report) => done.push((op.clone(), report)),
            Err(e) => {
                let mut rollback_failures = Vec::new();
                for (done_op, _) in done.iter().rev() {
                    if let Err(rollback_err) = moves::move_file(&done_op.dst, &done_op.src) {
                        tracing::error!(
                            file = %done_op.dst.display(),
                            error = %rollback_err,
                            "vrácení souboru po selhání skupiny (§3.5) se nepovedlo - vyžaduje ruční zásah"
                        );
                        rollback_failures.push(format!(
                            "{} zůstal na novém místě (vrácení selhalo: {rollback_err})",
                            done_op.dst.display()
                        ));
                    }
                }
                let reason = if rollback_failures.is_empty() {
                    format!(
                        "skupina selhala na {} ({e}), zbytek skupiny vrácen zpět",
                        op.dst.display()
                    )
                } else {
                    format!(
                        "skupina selhala na {} ({e}) A vrácení části skupiny se nepovedlo - \
                         vyžaduje ruční kontrolu: {}",
                        op.dst.display(),
                        rollback_failures.join("; ")
                    )
                };
                for group_op in &group {
                    journal.record(JournalState::FsFailed, file_move_journal_op(group_op))?;
                    failed.push((group_op.clone(), reason.clone()));
                }
                return Ok(());
            }
        }
    }

    for (op, report) in done {
        let (size, mtime) = metadata_for_journal(&op.dst)?;
        journal.record(
            JournalState::FsDone,
            JournalOp::FileMove {
                src: op.src.clone(),
                dst: op.dst.clone(),
                size,
                mtime,
                hash_blake3: report.hash_blake3,
                group_id: op.group_id.clone(),
            },
        )?;
        committed.push(op);
    }
    Ok(())
}

/// Pre-flight je taky skupinový (§0.4 + §3.5): když kterýkoli člen skupiny
/// neprojde, vyřadí se CELÁ skupina, i členové, co by sami o sobě prošli -
/// jinak by se sidecar oddělil od hlavního souboru už tady, ne až při
/// selhání přesunu.
fn preflight_fs_ops(ops: &[FileMoveOp]) -> (Vec<FileMoveOp>, Vec<(FileMoveOp, String)>) {
    let mut valid = Vec::new();
    let mut rejected = Vec::new();

    for group in group_by_group_id(ops.to_vec()) {
        let reasons: Vec<(FileMoveOp, String)> = group
            .iter()
            .filter_map(|op| preflight_reason(op).map(|r| (op.clone(), r)))
            .collect();

        if reasons.is_empty() {
            valid.extend(group);
        } else if let ([op], [(_, reason)]) = (group.as_slice(), reasons.as_slice()) {
            rejected.push((op.clone(), reason.clone()));
        } else {
            let combined = reasons
                .iter()
                .map(|(op, r)| format!("{}: {r}", op.dst.display()))
                .collect::<Vec<_>>()
                .join("; ");
            for op in group {
                rejected.push((op, format!("skupina (sidecary) vyřazena: {combined}")));
            }
        }
    }

    (valid, rejected)
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum TargetGoneError {
    #[error(
        "cíl „{0}“ na disku (už) neexistuje - mezitím se přesunul, byl smazán, nebo se \
         odpojil disk; zápis do katalogu se pro něj neprovede"
    )]
    Gone(String),
}

/// Cesta, u které bylo ověřeno, že na disku existuje (§8 bod 0.4).
///
/// **Proč vlastní typ, a ne jen funkce vracející `bool`:** `preflight_fs_ops`
/// hlídá jen přesuny souborů - operace typu „přepiš cestu v katalogu" (ruční
/// resolve §6.2, repoint složky §7.1, přemapování kořene §7.4) žádný
/// `FileMoveOp` nemají, a přesto stojí na předpokladu, že cíl na disku
/// existuje. Mezi fází MATCH a potvrzením přitom může uběhnout libovolně
/// dlouho - uživatel odklikává stovky souborů v dialogu duplikátů a mezitím
/// může odpojit disk nebo soubor přesunout.
///
/// `plan_*` funkce proto cestu přijímají VÝHRADNĚ v téhle podobě a jediný
/// způsob, jak ji v produkčním kódu vyrobit, je [`VerifiedPath::check`].
/// Kontrolu tak nejde zapomenout zavolat - hlídá to typový systém, ne
/// pozornost autora další zápisové cesty.
///
/// **Co to NEŘEŠÍ:** mezi ověřením a `COMMIT` může soubor pořád zmizet
/// (TOCTOU). Cílem je zkrátit to okno z „minuty až hodiny, co uživatel
/// odklikával dialogy" na jednotky milisekund, ne ho vymýtit - úplnou
/// jistotu by dalo jen držet soubor otevřený po celou transakci, což u
/// tisíců souborů nejde. Zbytkové riziko pokrývá až fáze KOMPENZACE (§8
/// fáze 3) a undo (§9).
#[derive(Debug, Clone)]
pub struct VerifiedPath(PathBuf);

impl VerifiedPath {
    /// Jediná cesta, jak `VerifiedPath` vyrobit v produkčním kódu.
    pub fn check(path: &Path) -> Result<Self, TargetGoneError> {
        if exists_verbatim(path) {
            Ok(Self(path.to_path_buf()))
        } else {
            Err(TargetGoneError::Gone(path.display().to_string()))
        }
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Rodičovská složka ověřené cesty - bez dalšího dotazu na disk.
    /// Když existuje soubor, existuje i složka, ve které leží, takže se
    /// ověření dědí. Používá ruční resolve (§6.2): ověřuje SOUBOR (silnější
    /// tvrzení - složka může zůstat, i když soubor zmizí), ale k sestavení
    /// `DbOpTemplate` potřebuje jeho složku.
    pub fn parent(&self) -> Option<Self> {
        self.0.parent().map(|p| Self(p.to_path_buf()))
    }

    /// Jen pro testy `plan_*` funkcí, které mají zůstat čistě logické a
    /// nesahat na disk. V produkčním kódu není dostupné.
    #[cfg(test)]
    pub(crate) fn unchecked(path: impl Into<PathBuf>) -> Self {
        Self(path.into())
    }
}

fn preflight_reason(op: &FileMoveOp) -> Option<String> {
    if !exists_verbatim(&op.src) {
        return Some(format!("zdrojový soubor neexistuje: {}", op.src.display()));
    }
    if exists_verbatim(&op.dst) {
        return Some(format!("cílový soubor už existuje: {}", op.dst.display()));
    }
    match op.dst.parent() {
        Some(parent) if exists_verbatim(parent) => None,
        _ => Some(format!("cílová složka neexistuje: {}", op.dst.display())),
    }
}

fn file_move_journal_op(op: &FileMoveOp) -> JournalOp {
    let (size, mtime) = metadata_for_journal(&op.src).unwrap_or((0, 0));
    JournalOp::FileMove {
        src: op.src.clone(),
        dst: op.dst.clone(),
        size,
        mtime,
        hash_blake3: None,
        group_id: op.group_id.clone(),
    }
}

fn metadata_for_journal(path: &Path) -> Result<(u64, i64), std::io::Error> {
    let meta = std::fs::metadata(path)?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok((meta.len(), mtime))
}

#[derive(Debug, Error)]
enum DbPhaseError {
    #[error("chyba SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("chyba journalu: {0}")]
    Journal(#[from] JournalError),
    #[error("kontrolní dotaz selhal: {0}")]
    IntegrityCheck(String),
    #[error("chyba generování id: {0}")]
    Id(#[from] crate::catalog::ids::IdError),
    #[error("root {0} pro novou složku neexistuje - založení nového rootu je mimo rozsah")]
    UnknownRoot(i64),
    #[error("složka root={root_id} pathFromRoot='{path_from_root}' už v katalogu existuje")]
    DuplicateFolder {
        root_id: i64,
        path_from_root: String,
    },
}

/// Fáze 2 (§8): `BEGIN IMMEDIATE`, aplikuj `db_ops`, kontrolní dotazy
/// (§8.1), `COMMIT`. Při jakémkoli selhání (`ROLLBACK` proběhne automaticky
/// přes `Drop` na `rusqlite::Transaction`, když se nezavolá `commit()`)
/// spustí fázi 3 - vrátí `committed_fs` zpět.
pub fn run_db_phase(
    catalog_path: &Path,
    journal: &mut JournalWriter,
    db_ops: &[DbOpTemplate],
    committed_fs: &[FileMoveOp],
) -> Result<DbPhaseResult, ApplyError> {
    // Invariant 2: zámky se kontrolují znovu, těsně před zápisem - ne jen na
    // začátku session.
    let mut handle = open::open_read_write(catalog_path)?;

    let files_count_before: i64 =
        handle
            .conn
            .query_row("SELECT count(*) FROM AgLibraryFile", [], |r| r.get(0))?;

    let mut applied_ops: Vec<JournalOp> = Vec::new();
    let outcome: Result<(), DbPhaseError> = (|| {
        let tx = handle
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        for op in db_ops {
            let journal_ops = apply_db_op(&tx, journal, op)?;
            applied_ops.extend(journal_ops);
        }

        run_integrity_checks(&tx, files_count_before)?;

        tx.commit()?;
        Ok(())
    })();

    match outcome {
        Ok(()) => {
            for journal_op in applied_ops {
                if let Err(e) = journal.record(JournalState::Done, journal_op) {
                    tracing::error!(%e, "DB commit proběhl, ale nejde zapsat potvrzující journal záznam");
                }
            }
            Ok(DbPhaseResult::Committed)
        }
        Err(e) => {
            tracing::error!(%e, "fáze DATABÁZE selhala, spouštím kompenzaci (§8 fáze 3)");
            let compensation_failures = compensate(committed_fs, journal);
            Ok(DbPhaseResult::RolledBack {
                reason: e.to_string(),
                compensation_failures,
            })
        }
    }
}

/// Vrací všechny journal záznamy (`Planned`), které operace vygenerovala -
/// case B je ve skutečnosti dvě SQL operace (`INSERT` + `UPDATE`), takže
/// jedna `DbOpTemplate` položka může mít víc než jeden `JournalOp`.
fn apply_db_op(
    tx: &rusqlite::Transaction,
    journal: &mut JournalWriter,
    op: &DbOpTemplate,
) -> Result<Vec<JournalOp>, DbPhaseError> {
    match op {
        DbOpTemplate::RemapRoot {
            root_id,
            new_absolute_path,
        } => {
            let before_path: String = tx.query_row(
                "SELECT absolutePath FROM AgLibraryRootFolder WHERE id_local = ?1",
                [root_id],
                |r| r.get(0),
            )?;

            let journal_op = JournalOp::DbUpdate {
                table: "AgLibraryRootFolder".to_string(),
                id_local: *root_id,
                before: serde_json::json!({ "absolutePath": before_path }),
                after: serde_json::json!({ "absolutePath": new_absolute_path }),
            };
            journal.record(JournalState::Planned, journal_op.clone())?;

            tx.execute(
                "UPDATE AgLibraryRootFolder SET absolutePath = ?1 WHERE id_local = ?2",
                rusqlite::params![new_absolute_path, root_id],
            )?;

            Ok(vec![journal_op])
        }

        DbOpTemplate::UpdateFileFolder {
            file_id,
            new_folder_id,
        } => Ok(vec![update_file_folder(
            tx,
            journal,
            *file_id,
            *new_folder_id,
        )?]),

        DbOpTemplate::InsertFolderAndUpdateFile {
            file_id,
            root_id,
            path_from_root,
        } => {
            let root_exists: bool = tx
                .prepare("SELECT 1 FROM AgLibraryRootFolder WHERE id_local = ?1")?
                .exists([root_id])?;
            if !root_exists {
                return Err(DbPhaseError::UnknownRoot(*root_id));
            }
            let duplicate: bool = tx
                .prepare(
                    "SELECT 1 FROM AgLibraryFolder WHERE rootFolder = ?1 AND pathFromRoot = ?2",
                )?
                .exists(rusqlite::params![root_id, path_from_root])?;
            if duplicate {
                return Err(DbPhaseError::DuplicateFolder {
                    root_id: *root_id,
                    path_from_root: path_from_root.clone(),
                });
            }

            let new_folder_id = crate::catalog::ids::next_id_local(tx, "AgLibraryFolder")?;
            let new_folder_global = crate::catalog::ids::unique_id_global(tx, "AgLibraryFolder")?;

            let insert_op = JournalOp::DbInsert {
                table: "AgLibraryFolder".to_string(),
                id_local: new_folder_id,
                after: serde_json::json!({
                    "id_global": new_folder_global,
                    "pathFromRoot": path_from_root,
                    "rootFolder": root_id,
                }),
            };
            journal.record(JournalState::Planned, insert_op.clone())?;
            tx.execute(
                "INSERT INTO AgLibraryFolder (id_local, id_global, pathFromRoot, rootFolder) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![new_folder_id, new_folder_global, path_from_root, root_id],
            )?;

            let update_op = update_file_folder(tx, journal, *file_id, new_folder_id)?;

            Ok(vec![insert_op, update_op])
        }

        DbOpTemplate::RenameFile {
            file_id,
            new_base_name,
            new_extension,
            new_idx_filename,
        } => {
            let (before_base, before_ext, before_idx, before_lc): (String, String, String, String) = tx.query_row(
                "SELECT baseName, extension, idx_filename, lc_idx_filename FROM AgLibraryFile WHERE id_local = ?1",
                [file_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?;
            let new_lc = new_idx_filename.to_lowercase();

            let journal_op = JournalOp::DbUpdate {
                table: "AgLibraryFile".to_string(),
                id_local: *file_id,
                before: serde_json::json!({
                    "baseName": before_base, "extension": before_ext,
                    "idx_filename": before_idx, "lc_idx_filename": before_lc,
                }),
                // originalFilename se NIKDY nemění (PROJECT.md §3.3).
                after: serde_json::json!({
                    "baseName": new_base_name, "extension": new_extension,
                    "idx_filename": new_idx_filename, "lc_idx_filename": new_lc,
                }),
            };
            journal.record(JournalState::Planned, journal_op.clone())?;

            tx.execute(
                "UPDATE AgLibraryFile SET baseName = ?1, extension = ?2, idx_filename = ?3, lc_idx_filename = ?4 \
                 WHERE id_local = ?5",
                rusqlite::params![new_base_name, new_extension, new_idx_filename, new_lc, file_id],
            )?;

            Ok(vec![journal_op])
        }

        DbOpTemplate::RemapFolder {
            folder_id,
            new_root_id,
            new_path_from_root,
        } => {
            let root_exists: bool = tx
                .prepare("SELECT 1 FROM AgLibraryRootFolder WHERE id_local = ?1")?
                .exists([new_root_id])?;
            if !root_exists {
                return Err(DbPhaseError::UnknownRoot(*new_root_id));
            }

            let (before_path, before_root): (String, i64) = tx.query_row(
                "SELECT pathFromRoot, rootFolder FROM AgLibraryFolder WHERE id_local = ?1",
                [folder_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;

            let journal_op = JournalOp::DbUpdate {
                table: "AgLibraryFolder".to_string(),
                id_local: *folder_id,
                before: serde_json::json!({ "pathFromRoot": before_path, "rootFolder": before_root }),
                after: serde_json::json!({ "pathFromRoot": new_path_from_root, "rootFolder": new_root_id }),
            };
            journal.record(JournalState::Planned, journal_op.clone())?;

            tx.execute(
                "UPDATE AgLibraryFolder SET pathFromRoot = ?1, rootFolder = ?2 WHERE id_local = ?3",
                rusqlite::params![new_path_from_root, new_root_id, folder_id],
            )?;

            Ok(vec![journal_op])
        }
    }
}

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ManualResolveError {
    #[error("cílová složka „{0}“ nepatří pod žádný známý kořen katalogu - ruční mapování rootů zatím GUI neumí")]
    NoMatchingRoot(String),
    /// PROJECT.md §7.1 bod 1: "pokud T už v katalogu existuje jako jiná
    /// složka, nelze repointovat". Signál pro volajícího (`plan_folder_repoint`),
    /// aby místo jednoho `RemapFolder` sestavil N× `UpdateFileFolder` na
    /// existující `existing_folder_id` - `plan_manual_resolve` už tohle umí
    /// pro jednotlivé soubory.
    #[error("cílová složka už v katalogu existuje jako jiná složka (id {existing_folder_id}) - nejde přepointovat celou složku najednou")]
    TargetFolderAlreadyExists { existing_folder_id: i64 },
}

/// Postaví `HashMap` "cesta ke složce (normalizovaný klíč) -> `folder_id`"
/// ze všech souborů v katalogu - vstup pro [`plan_manual_resolve`], aby
/// poznal, jestli cílová složka vybraného kandidáta v katalogu už existuje
/// (případ A), nebo je potřeba ji založit (případ B).
pub fn known_folders_by_path(
    files: &[crate::catalog::extract::CatalogFile],
) -> std::collections::HashMap<String, i64> {
    let mut map = std::collections::HashMap::new();
    for f in files {
        if let Some(parent) = f.expected_path.parent() {
            map.entry(folder_key(parent)).or_insert(f.folder_id);
        }
    }
    map
}

fn to_forward_slash_dir(path: &Path) -> String {
    let mut s = path.to_string_lossy().replace('\\', "/");
    if !s.ends_with('/') {
        s.push('/');
    }
    s
}

fn folder_key(dir: &Path) -> String {
    crate::fsops::paths::match_key_str(&to_forward_slash_dir(dir))
}

/// Rozdělí `target` na segmenty a porovná prvních `root.len()` z nich s
/// `root` case-insensitively (§16.3 - porovnání jen přes normalizovaný
/// klíč). Vrací zbylé segmenty ve SKUTEČNÉM (nenormalizovaném) tvaru z
/// `target`, spojené `/` a s koncovým lomítkem - přesně formát, jaký
/// `AgLibraryFolder.pathFromRoot` v katalogu očekává. `None`, pokud `target`
/// pod `root` vůbec neleží.
fn strip_root_prefix(target_forward: &str, root_absolute: &str) -> Option<String> {
    use crate::fsops::paths::match_key_str;

    let target_segments: Vec<&str> = target_forward
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let root_segments: Vec<&str> = root_absolute
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if target_segments.len() < root_segments.len() {
        return None;
    }
    let (target_prefix, target_rest) = target_segments.split_at(root_segments.len());
    let prefix_matches = target_prefix
        .iter()
        .zip(root_segments.iter())
        .all(|(a, b)| match_key_str(a) == match_key_str(b));
    if !prefix_matches {
        return None;
    }
    if target_rest.is_empty() {
        Some(String::new())
    } else {
        Some(format!("{}/", target_rest.join("/")))
    }
}

/// Sestaví `DbOpTemplate` pro ruční resolve jednoho souboru z dialogu
/// duplikátů (PROJECT.md §6.2): `target_dir` je rodičovská složka
/// kandidáta, kterého uživatel vybral. Když `target_dir` v katalogu už
/// existuje jako `AgLibraryFolder` (viz [`known_folders_by_path`]), vrátí
/// levné [`DbOpTemplate::UpdateFileFolder`] (případ A); jinak
/// [`DbOpTemplate::InsertFolderAndUpdateFile`] (případ B) pod nejbližším
/// (nejdelším) odpovídajícím rootem. Když žádný root nesedí, vrátí chybu -
/// nikdy nehádá (CLAUDE.md §6: "Nehádej").
pub fn plan_manual_resolve(
    file_id: i64,
    target_dir: &VerifiedPath,
    roots: &[crate::catalog::extract::RootFolder],
    known_folders: &std::collections::HashMap<String, i64>,
) -> Result<DbOpTemplate, ManualResolveError> {
    let target_dir = target_dir.as_path();
    if let Some(folder_id) = known_folders.get(&folder_key(target_dir)) {
        return Ok(DbOpTemplate::UpdateFileFolder {
            file_id,
            new_folder_id: *folder_id,
        });
    }

    let (root_id, path_from_root) = resolve_root_and_path_from_root(target_dir, roots)?;
    Ok(DbOpTemplate::InsertFolderAndUpdateFile {
        file_id,
        root_id,
        path_from_root,
    })
}

/// Najde nejbližší (nejdelší, tedy nejspecifičtější) `root`, pod který
/// `target_dir` spadá, a vrátí `(root_id, path_from_root)` - sdílené jádro
/// pro [`plan_manual_resolve`] (případ B) i [`plan_folder_repoint`].
fn resolve_root_and_path_from_root(
    target_dir: &Path,
    roots: &[crate::catalog::extract::RootFolder],
) -> Result<(i64, String), ManualResolveError> {
    let target_forward = to_forward_slash_dir(target_dir);
    let mut best: Option<(&crate::catalog::extract::RootFolder, String)> = None;
    for root in roots {
        if let Some(rel) = strip_root_prefix(&target_forward, &root.absolute_path) {
            let is_more_specific = best
                .as_ref()
                .is_none_or(|(r, _)| root.absolute_path.len() > r.absolute_path.len());
            if is_more_specific {
                best = Some((root, rel));
            }
        }
    }

    best.map(|(root, path_from_root)| (root.root_id, path_from_root))
        .ok_or(ManualResolveError::NoMatchingRoot(target_forward))
}

/// Sestaví `DbOpTemplate` pro vzor A (`FOLDER_MOVED`, PROJECT.md §7.1,
/// "Repoint DB"): jeden `RemapFolder` místo N `UpdateFileFolder`. Když
/// `target_dir` v katalogu už existuje jako JINÁ složka než `folder_id`
/// (typicky proto, že tam uživatel dřív něco naimportoval), repoint podle
/// specifikace nejde - vrátí [`ManualResolveError::TargetFolderAlreadyExists`],
/// volající (`app::state`) pak sestaví N× `UpdateFileFolder` na
/// `existing_folder_id` sám (`plan_manual_resolve` na to nestačí, protože ta
/// funkce bere jeden `file_id`, ne celou složku).
pub fn plan_folder_repoint(
    folder_id: i64,
    target_dir: &VerifiedPath,
    roots: &[crate::catalog::extract::RootFolder],
    known_folders: &std::collections::HashMap<String, i64>,
) -> Result<DbOpTemplate, ManualResolveError> {
    let target_dir = target_dir.as_path();
    if let Some(&existing_folder_id) = known_folders.get(&folder_key(target_dir)) {
        if existing_folder_id != folder_id {
            return Err(ManualResolveError::TargetFolderAlreadyExists { existing_folder_id });
        }
    }

    let (new_root_id, new_path_from_root) = resolve_root_and_path_from_root(target_dir, roots)?;
    Ok(DbOpTemplate::RemapFolder {
        folder_id,
        new_root_id,
        new_path_from_root,
    })
}

/// Sestaví `DbOpTemplate` pro vzor D (§7.4, přemapování kořene).
///
/// Na rozdíl od [`plan_manual_resolve`]/[`plan_folder_repoint`] nebere
/// [`VerifiedPath`], ale katalogový řetězec (dopředná lomítka, koncové
/// lomítko) - a ověření si udělá sám. Důvod: převod katalogové konvence na
/// nativní cestu je detail téhle operace a duplikovat ho na obou volajících
/// (GUI i CLI) by byl přesně ten druh kopie, kde se chyba časem rozejde.
/// Kontrola je tak i tady nepřeskočitelná, jen je uvnitř místo v signatuře.
pub fn plan_root_remap(
    root_id: i64,
    new_absolute_path: &str,
) -> Result<DbOpTemplate, TargetGoneError> {
    let native = PathBuf::from(new_absolute_path.replace('/', "\\"));
    VerifiedPath::check(&native)?;
    Ok(DbOpTemplate::RemapRoot {
        root_id,
        new_absolute_path: new_absolute_path.to_string(),
    })
}

fn update_file_folder(
    tx: &rusqlite::Transaction,
    journal: &mut JournalWriter,
    file_id: i64,
    new_folder_id: i64,
) -> Result<JournalOp, DbPhaseError> {
    let before_folder: i64 = tx.query_row(
        "SELECT folder FROM AgLibraryFile WHERE id_local = ?1",
        [file_id],
        |r| r.get(0),
    )?;

    let journal_op = JournalOp::DbUpdate {
        table: "AgLibraryFile".to_string(),
        id_local: file_id,
        before: serde_json::json!({ "folder": before_folder }),
        after: serde_json::json!({ "folder": new_folder_id }),
    };
    journal.record(JournalState::Planned, journal_op.clone())?;

    tx.execute(
        "UPDATE AgLibraryFile SET folder = ?1 WHERE id_local = ?2",
        rusqlite::params![new_folder_id, file_id],
    )?;

    Ok(journal_op)
}

/// Kontrolní dotazy před commitem (PROJECT.md §8.1). Nesplnění → chyba →
/// volající udělá `ROLLBACK` (implicitně přes `Drop`).
fn run_integrity_checks(
    tx: &rusqlite::Transaction,
    files_count_before: i64,
) -> Result<(), DbPhaseError> {
    let orphan_files: i64 = tx.query_row(
        "SELECT count(*) FROM AgLibraryFile f \
         LEFT JOIN AgLibraryFolder fo ON f.folder = fo.id_local \
         WHERE fo.id_local IS NULL",
        [],
        |r| r.get(0),
    )?;
    if orphan_files != 0 {
        return Err(DbPhaseError::IntegrityCheck(format!(
            "{orphan_files} souborů ukazuje na neexistující složku"
        )));
    }

    let orphan_folders: i64 = tx.query_row(
        "SELECT count(*) FROM AgLibraryFolder fo \
         LEFT JOIN AgLibraryRootFolder rf ON fo.rootFolder = rf.id_local \
         WHERE rf.id_local IS NULL",
        [],
        |r| r.get(0),
    )?;
    if orphan_folders != 0 {
        return Err(DbPhaseError::IntegrityCheck(format!(
            "{orphan_folders} složek ukazuje na neexistující kořen"
        )));
    }

    let files_count_after: i64 =
        tx.query_row("SELECT count(*) FROM AgLibraryFile", [], |r| r.get(0))?;
    if files_count_after != files_count_before {
        return Err(DbPhaseError::IntegrityCheck(format!(
            "počet souborů v katalogu se změnil ({files_count_before} → {files_count_after}), i když neměl"
        )));
    }

    Ok(())
}

/// Fáze 3 (§8): vrať `committed_fs` zpět, v obráceném pořadí. Chyby se
/// sbírají a vrací volajícímu - i částečně neúspěšná kompenzace se musí
/// hlasitě nahlásit jako vyžadující ruční zásah (§8 fáze 3.2), nikdy ne
/// tiše.
fn compensate(committed_fs: &[FileMoveOp], journal: &mut JournalWriter) -> Vec<(PathBuf, String)> {
    let mut failures = Vec::new();
    for op in committed_fs.iter().rev() {
        match moves::move_file(&op.dst, &op.src) {
            Ok(_) => {
                if let Err(e) = journal.record(
                    JournalState::Compensated,
                    JournalOp::FileMove {
                        src: op.dst.clone(),
                        dst: op.src.clone(),
                        size: 0,
                        mtime: 0,
                        hash_blake3: None,
                        group_id: op.group_id.clone(),
                    },
                ) {
                    tracing::error!(%e, "nejde zapsat kompenzační journal záznam (soubor se ale vrátil)");
                }
            }
            Err(source) => {
                if let Err(e) = journal.record(
                    JournalState::CompensationFailed,
                    JournalOp::FileMove {
                        src: op.dst.clone(),
                        dst: op.src.clone(),
                        size: 0,
                        mtime: 0,
                        hash_blake3: None,
                        group_id: op.group_id.clone(),
                    },
                ) {
                    tracing::error!(%e, "nejde zapsat záznam o selhané kompenzaci");
                }
                failures.push((op.dst.clone(), describe_move_error(&source)));
            }
        }
    }
    failures
}

fn describe_move_error(e: &MoveError) -> String {
    e.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn make_catalog(path: &Path) {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, id_global TEXT, absolutePath TEXT, name TEXT);
             CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, id_global TEXT, pathFromRoot TEXT, rootFolder INTEGER);
             CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, id_global TEXT, folder INTEGER, baseName TEXT,
                                          extension TEXT, idx_filename TEXT, lc_idx_filename TEXT, externalModTime TEXT,
                                          md5 TEXT, sidecarExtensions TEXT);
             INSERT INTO AgLibraryRootFolder VALUES (1, 'R1', 'E:/Foto/', 'Foto');
             INSERT INTO AgLibraryFolder VALUES (10, 'F1', '2019/', 1);
             INSERT INTO AgLibraryFolder VALUES (11, 'F2', '2020/', 1);
             INSERT INTO AgLibraryFile VALUES (100, 'D1', 10, 'IMG_0001', 'CR2', 'IMG_0001.CR2', 'img_0001.cr2', NULL, NULL, NULL);",
        )
        .unwrap();
    }

    #[test]
    fn remap_root_commits_and_journals_planned_then_done() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::RemapRoot {
                root_id: 1,
                new_absolute_path: "F:/Foto/".to_string(),
            }],
        };

        let outcome = apply(&catalog, &journal_path, "sess1", plan).unwrap();
        assert!(matches!(outcome.db_phase, DbPhaseResult::Committed));

        let conn = Connection::open(&catalog).unwrap();
        let path: String = conn
            .query_row(
                "SELECT absolutePath FROM AgLibraryRootFolder WHERE id_local = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(path, "F:/Foto/");

        assert!(outcome.backup.catalog_backup.exists());

        let entries = crate::journal::write::read_session(&journal_path, "sess1").unwrap();
        let states: Vec<_> = entries.iter().map(|e| e.state).collect();
        assert!(states.contains(&JournalState::Planned));
        assert!(states.contains(&JournalState::Done));
    }

    #[test]
    fn nonexistent_root_id_rolls_back_without_changing_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::RemapRoot {
                root_id: 99999,
                new_absolute_path: "F:/Foto/".to_string(),
            }],
        };

        let outcome = apply(&catalog, &journal_path, "sess1", plan).unwrap();
        assert!(matches!(outcome.db_phase, DbPhaseResult::RolledBack { .. }));

        let conn = Connection::open(&catalog).unwrap();
        let path: String = conn
            .query_row(
                "SELECT absolutePath FROM AgLibraryRootFolder WHERE id_local = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            path, "E:/Foto/",
            "katalog nesmí být selháním DB fáze nijak pozměněný"
        );
    }

    #[test]
    fn refuses_to_apply_when_catalog_is_locked() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        // `.lrcat.lock`, ne prázdný `-wal`: ten vzniká i naším vlastním
        // read-only otevřením a zámek neznamená (viz `catalog::open`).
        let mut lock = catalog.as_os_str().to_os_string();
        lock.push(".lock");
        std::fs::write(&lock, b"").unwrap();
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::RemapRoot {
                root_id: 1,
                new_absolute_path: "F:/Foto/".to_string(),
            }],
        };

        let err = apply(&catalog, &journal_path, "sess1", plan).unwrap_err();
        assert!(matches!(err, ApplyError::CatalogLocked));
        // Bez úspěšné zálohy (a zámek zablokoval dřív, než se o ni vůbec
        // zkusilo) nesmí vzniknout žádný zálohovací adresář.
        assert!(!tmp.path().join("LrCatFix-backups").exists());
    }

    #[test]
    fn preflight_rejects_fs_op_with_existing_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("a.CR2");
        let dst = tmp.path().join("b.CR2");
        std::fs::write(&src, b"x").unwrap();
        std::fs::write(&dst, b"uz existuje").unwrap();
        let journal_path = tmp.path().join("journal.jsonl");
        let mut journal = JournalWriter::open(&journal_path, "sess1").unwrap();

        let result = run_fs_phase(
            &[FileMoveOp {
                src: src.clone(),
                dst: dst.clone(),
                group_id: None,
            }],
            &mut journal,
        )
        .unwrap();

        assert_eq!(result.rejected.len(), 1);
        assert!(result.committed.is_empty());
        assert!(src.exists());
        assert_eq!(std::fs::read(&dst).unwrap(), b"uz existuje");
    }

    /// PROJECT.md §12 „Test částečného selhání": jeden přesun z dávky selže,
    /// zbytek musí projít - `run_fs_phase` neukončuje běh při první chybě
    /// (§8 fáze 1.4).
    #[test]
    fn batch_continues_past_a_single_rejected_move() {
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");
        let mut journal = JournalWriter::open(&journal_path, "sess1").unwrap();

        let mut ops = Vec::new();
        for i in 0..3 {
            let src = tmp.path().join(format!("src_{i}.CR2"));
            let dst = tmp.path().join(format!("dst_{i}.CR2"));
            std::fs::write(&src, b"data").unwrap();
            ops.push(FileMoveOp {
                src,
                dst,
                group_id: None,
            });
        }
        // Prostřední cíl už existuje - tenhle jeden musí být odmítnut, ale
        // zbylé dva musí projít.
        std::fs::write(&ops[1].dst, b"prekazka").unwrap();

        let result = run_fs_phase(&ops, &mut journal).unwrap();

        assert_eq!(
            result.committed.len(),
            2,
            "499 z 500 (tady 2 ze 3) je platný výsledek"
        );
        assert_eq!(result.rejected.len(), 1);
        assert!(result.failed.is_empty());
        assert!(ops[0].dst.exists());
        assert!(ops[2].dst.exists());
        assert!(
            ops[1].src.exists(),
            "odmítnutý přesun se nesmí vůbec provést"
        );
    }

    /// PROJECT.md §3.5 bod 1: "Přesun je atomická skupina... Selže-li
    /// kterýkoli člen, vrať zpět celou skupinu."
    #[test]
    fn group_move_succeeds_when_every_member_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");
        let mut journal = JournalWriter::open(&journal_path, "sess1").unwrap();

        let raw_src = tmp.path().join("IMG_3000.CR2");
        let raw_dst = tmp.path().join("moved").join("IMG_3000.CR2");
        let xmp_src = tmp.path().join("IMG_3000.xmp");
        let xmp_dst = tmp.path().join("moved").join("IMG_3000.xmp");
        std::fs::create_dir_all(tmp.path().join("moved")).unwrap();
        std::fs::write(&raw_src, b"raw").unwrap();
        std::fs::write(&xmp_src, b"<xmp/>").unwrap();

        let ops = vec![
            FileMoveOp {
                src: raw_src.clone(),
                dst: raw_dst.clone(),
                group_id: Some("g1".to_string()),
            },
            FileMoveOp {
                src: xmp_src.clone(),
                dst: xmp_dst.clone(),
                group_id: Some("g1".to_string()),
            },
        ];

        let result = run_fs_phase(&ops, &mut journal).unwrap();
        assert_eq!(result.committed.len(), 2);
        assert!(result.failed.is_empty());
        assert!(raw_dst.exists());
        assert!(xmp_dst.exists());
    }

    /// Sidecar (`.xmp`) se nesmí ODDĚLIT od hlavního souboru - když jeho
    /// přesun selže (cíl už existuje), musí se vrátit i ten hlavní, který
    /// už proběhl.
    #[test]
    fn group_move_rolls_back_earlier_member_when_later_member_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");
        let mut journal = JournalWriter::open(&journal_path, "sess1").unwrap();

        let raw_src = tmp.path().join("IMG_3000.CR2");
        let raw_dst = tmp.path().join("moved").join("IMG_3000.CR2");
        let xmp_src = tmp.path().join("IMG_3000.xmp");
        let xmp_dst = tmp.path().join("moved").join("IMG_3000.xmp");
        std::fs::create_dir_all(tmp.path().join("moved")).unwrap();
        std::fs::write(&raw_src, b"raw").unwrap();
        std::fs::write(&xmp_src, b"<xmp/>").unwrap();
        // xmp cíl už existuje - druhý člen skupiny selže na preflightu.
        std::fs::write(&xmp_dst, b"uz existuje").unwrap();

        let ops = vec![
            FileMoveOp {
                src: raw_src.clone(),
                dst: raw_dst.clone(),
                group_id: Some("g1".to_string()),
            },
            FileMoveOp {
                src: xmp_src.clone(),
                dst: xmp_dst.clone(),
                group_id: Some("g1".to_string()),
            },
        ];

        let result = run_fs_phase(&ops, &mut journal).unwrap();

        // Preflight je taky skupinový - celá skupina se vyřadí HNED, RAW se
        // nikdy nezkusí přesunout.
        assert_eq!(result.rejected.len(), 2);
        assert!(result.committed.is_empty());
        assert!(result.failed.is_empty());
        assert!(raw_src.exists(), "RAW musí zůstat na místě");
        assert!(!raw_dst.exists());
    }

    /// Stejný scénář, ale selhání nastane až PŘI PŘESUNU (ne na preflightu) -
    /// ověřuje rollback přímo v `run_one_group`, ne jen group-aware
    /// preflight. Cíl druhého člena skupiny má jako "rodičovskou složku"
    /// obyčejný SOUBOR (`blocker.txt`) - `Path::exists()` (a tedy
    /// `exists_verbatim`) je pro soubor i složku stejně `true`, takže
    /// preflight to nechá projít, ale `std::fs::rename` do takové "složky"
    /// selže se skutečnou OS chybou.
    #[test]
    fn group_move_rolls_back_when_move_itself_fails_mid_group() {
        let tmp = tempfile::tempdir().unwrap();
        let journal_path = tmp.path().join("journal.jsonl");
        let mut journal = JournalWriter::open(&journal_path, "sess1").unwrap();

        let raw_src = tmp.path().join("IMG_3000.CR2");
        let raw_dst = tmp.path().join("moved").join("IMG_3000.CR2");
        let xmp_src = tmp.path().join("IMG_3000.xmp");
        let blocker = tmp.path().join("moved").join("blocker.txt");
        let xmp_dst = blocker.join("IMG_3000.xmp");

        std::fs::create_dir_all(tmp.path().join("moved")).unwrap();
        std::fs::write(&raw_src, b"raw").unwrap();
        std::fs::write(&xmp_src, b"<xmp/>").unwrap();
        std::fs::write(&blocker, b"nejsem slozka").unwrap();

        let ops = vec![
            FileMoveOp {
                src: raw_src.clone(),
                dst: raw_dst.clone(),
                group_id: Some("g1".to_string()),
            },
            FileMoveOp {
                src: xmp_src.clone(),
                dst: xmp_dst.clone(),
                group_id: Some("g1".to_string()),
            },
        ];

        let result = run_fs_phase(&ops, &mut journal).unwrap();

        assert!(
            result.rejected.is_empty(),
            "preflight musí obě položky pustit dál"
        );
        assert_eq!(result.committed.len(), 0);
        assert_eq!(
            result.failed.len(),
            2,
            "celá skupina musí skončit jako selhaná"
        );
        assert!(
            raw_src.exists(),
            "RAW, který v týž skupině už proběhl, se musí vrátit zpátky"
        );
        assert!(!raw_dst.exists());
        assert!(xmp_src.exists(), "xmp se vůbec nepřesunul");
    }

    #[test]
    fn case_a_update_file_folder_repoints_to_known_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::UpdateFileFolder {
                file_id: 100,
                new_folder_id: 11,
            }],
        };
        let outcome = apply(&catalog, &journal_path, "sess1", plan).unwrap();
        assert!(matches!(outcome.db_phase, DbPhaseResult::Committed));

        let conn = Connection::open(&catalog).unwrap();
        let folder: i64 = conn
            .query_row(
                "SELECT folder FROM AgLibraryFile WHERE id_local = 100",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(folder, 11);
    }

    #[test]
    fn case_b_inserts_new_folder_under_known_root_and_repoints_file() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::InsertFolderAndUpdateFile {
                file_id: 100,
                root_id: 1,
                path_from_root: "2021/Nova slozka/".to_string(),
            }],
        };
        let outcome = apply(&catalog, &journal_path, "sess1", plan).unwrap();
        assert!(matches!(outcome.db_phase, DbPhaseResult::Committed));

        let conn = Connection::open(&catalog).unwrap();
        let (folder_id, path_from_root, root): (i64, String, i64) = conn
            .query_row(
                "SELECT fo.id_local, fo.pathFromRoot, fo.rootFolder FROM AgLibraryFile fi \
                 JOIN AgLibraryFolder fo ON fi.folder = fo.id_local WHERE fi.id_local = 100",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(path_from_root, "2021/Nova slozka/");
        assert_eq!(root, 1);
        assert_ne!(folder_id, 10, "musí to být NOVÁ složka, ne ta původní");
    }

    #[test]
    fn case_b_refuses_unknown_root() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::InsertFolderAndUpdateFile {
                file_id: 100,
                root_id: 99999,
                path_from_root: "2021/".to_string(),
            }],
        };
        let outcome = apply(&catalog, &journal_path, "sess1", plan).unwrap();
        assert!(matches!(outcome.db_phase, DbPhaseResult::RolledBack { .. }));

        let conn = Connection::open(&catalog).unwrap();
        let count: i64 = conn
            .query_row("SELECT count(*) FROM AgLibraryFolder", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "žádná nová složka se nesměla založit");
    }

    #[test]
    fn case_b_refuses_duplicate_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        // '2019/' už jako AgLibraryFolder pod rootem 1 existuje (id 10).
        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::InsertFolderAndUpdateFile {
                file_id: 100,
                root_id: 1,
                path_from_root: "2019/".to_string(),
            }],
        };
        let outcome = apply(&catalog, &journal_path, "sess1", plan).unwrap();
        match outcome.db_phase {
            DbPhaseResult::RolledBack { reason, .. } => assert!(reason.contains("existuje")),
            DbPhaseResult::Committed => panic!("duplicitní složka se nesměla založit"),
        }
    }

    #[test]
    fn case_c_renames_file_but_keeps_original_filename_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::RenameFile {
                file_id: 100,
                new_base_name: "IMG_0001_renamed".to_string(),
                new_extension: "CR2".to_string(),
                new_idx_filename: "IMG_0001_renamed.CR2".to_string(),
            }],
        };
        let outcome = apply(&catalog, &journal_path, "sess1", plan).unwrap();
        assert!(matches!(outcome.db_phase, DbPhaseResult::Committed));

        let conn = Connection::open(&catalog).unwrap();
        let (base, idx, lc): (String, String, String) = conn
            .query_row(
                "SELECT baseName, idx_filename, lc_idx_filename FROM AgLibraryFile WHERE id_local = 100",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(base, "IMG_0001_renamed");
        assert_eq!(idx, "IMG_0001_renamed.CR2");
        assert_eq!(lc, "img_0001_renamed.cr2");
    }

    fn root(id: i64, absolute_path: &str) -> crate::catalog::extract::RootFolder {
        crate::catalog::extract::RootFolder {
            root_id: id,
            absolute_path: absolute_path.to_string(),
            name: "Foto".to_string(),
        }
    }

    fn catalog_file_at(
        id: i64,
        folder_id: i64,
        expected_path: &str,
    ) -> crate::catalog::extract::CatalogFile {
        crate::catalog::extract::CatalogFile {
            file_id: id,
            folder_id,
            root_id: 1,
            root_path: String::new(),
            rel_path: String::new(),
            filename: "x.CR2".to_string(),
            base_name: "x".to_string(),
            extension: "CR2".to_string(),
            expected_path: PathBuf::from(expected_path),
            external_mod_time: None,
            md5: None,
            file_size: None,
            sidecar_extensions: Vec::new(),
        }
    }

    #[test]
    fn plan_manual_resolve_uses_update_when_target_folder_is_known() {
        let files = vec![catalog_file_at(1, 10, r"C:\Foto\2019\a.CR2")];
        let known = known_folders_by_path(&files);
        let roots = vec![root(1, "C:/Foto/")];

        let plan = plan_manual_resolve(
            100,
            &VerifiedPath::unchecked(r"C:\Foto\2019"),
            &roots,
            &known,
        )
        .unwrap();
        assert!(matches!(
            plan,
            DbOpTemplate::UpdateFileFolder {
                file_id: 100,
                new_folder_id: 10
            }
        ));
    }

    #[test]
    fn plan_manual_resolve_matches_known_folder_case_insensitively() {
        let files = vec![catalog_file_at(1, 10, r"C:\Foto\2019\a.CR2")];
        let known = known_folders_by_path(&files);
        let roots = vec![root(1, "C:/Foto/")];

        // Jiné velikosti písmen a lomítka - stejná složka.
        let plan = plan_manual_resolve(
            100,
            &VerifiedPath::unchecked(r"c:\foto\2019"),
            &roots,
            &known,
        )
        .unwrap();
        assert!(matches!(
            plan,
            DbOpTemplate::UpdateFileFolder {
                new_folder_id: 10,
                ..
            }
        ));
    }

    #[test]
    fn plan_manual_resolve_inserts_new_folder_under_matching_root() {
        let known = std::collections::HashMap::new();
        let roots = vec![root(1, "C:/Foto/")];

        let plan = plan_manual_resolve(
            100,
            &VerifiedPath::unchecked(r"C:\Foto\2021\Album"),
            &roots,
            &known,
        )
        .unwrap();
        assert!(matches!(
            plan,
            DbOpTemplate::InsertFolderAndUpdateFile {
                file_id: 100,
                root_id: 1,
                ..
            }
        ));
        if let DbOpTemplate::InsertFolderAndUpdateFile { path_from_root, .. } = plan {
            assert_eq!(path_from_root, "2021/Album/");
        }
    }

    #[test]
    fn plan_manual_resolve_refuses_when_no_root_matches() {
        let known = std::collections::HashMap::new();
        let roots = vec![root(1, "C:/Foto/")];

        let err = plan_manual_resolve(
            100,
            &VerifiedPath::unchecked(r"D:\Jina\Slozka"),
            &roots,
            &known,
        )
        .unwrap_err();
        assert!(matches!(err, ManualResolveError::NoMatchingRoot(_)));
    }

    #[test]
    fn plan_manual_resolve_prefers_more_specific_nested_root() {
        let known = std::collections::HashMap::new();
        let roots = vec![root(1, "C:/Foto/"), root(2, "C:/Foto/2021/")];

        let plan = plan_manual_resolve(
            100,
            &VerifiedPath::unchecked(r"C:\Foto\2021\Album"),
            &roots,
            &known,
        )
        .unwrap();
        if let DbOpTemplate::InsertFolderAndUpdateFile {
            root_id,
            path_from_root,
            ..
        } = plan
        {
            assert_eq!(root_id, 2);
            assert_eq!(path_from_root, "Album/");
        } else {
            panic!("expected InsertFolderAndUpdateFile");
        }
    }

    #[test]
    fn verified_path_accepts_existing_file_and_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("IMG_0001.CR2");
        std::fs::write(&file, b"x").unwrap();

        assert!(VerifiedPath::check(&file).is_ok());
        assert!(VerifiedPath::check(tmp.path()).is_ok());
    }

    /// Jádro vedlejšího nálezu: mezi výběrem kandidáta v dialogu a
    /// potvrzením zápisu může soubor zmizet (uživatel odklikává stovky
    /// souborů, mezitím odpojí disk). Bez téhle kontroly by se do katalogu
    /// zapsala cesta, která už neplatí.
    #[test]
    fn verified_path_refuses_vanished_target() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("IMG_0001.CR2");
        std::fs::write(&file, b"x").unwrap();
        assert!(VerifiedPath::check(&file).is_ok());

        // Kandidát se mezitím přesunul jinam / byl smazán.
        std::fs::remove_file(&file).unwrap();

        let err = VerifiedPath::check(&file).unwrap_err();
        assert!(matches!(err, TargetGoneError::Gone(_)));
        assert!(
            err.to_string().contains("neexistuje"),
            "hláška musí uživateli říct, co se stalo: {err}"
        );
    }

    /// `parent()` dědí ověření bez dalšího dotazu na disk - když existuje
    /// soubor, existuje i složka, ve které leží. Používá ruční resolve
    /// (§6.2), který ověřuje soubor, ale plánuje podle složky.
    #[test]
    fn verified_path_parent_inherits_verification() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("IMG_0001.CR2");
        std::fs::write(&file, b"x").unwrap();

        let verified = VerifiedPath::check(&file).unwrap();
        let parent = verified.parent().expect("soubor v tempdiru má rodiče");
        assert_eq!(parent.as_path(), tmp.path());
    }

    #[test]
    fn plan_root_remap_refuses_target_that_does_not_exist() {
        let err = plan_root_remap(1, "X:/rozhodne-neexistujici-cesta/").unwrap_err();
        assert!(matches!(err, TargetGoneError::Gone(_)));
    }

    #[test]
    fn plan_root_remap_accepts_existing_target_and_keeps_catalog_convention() {
        let tmp = tempfile::tempdir().unwrap();
        // Katalogová konvence: dopředná lomítka + koncové lomítko.
        let catalog_style = format!("{}/", tmp.path().to_string_lossy().replace('\\', "/"));

        let op = plan_root_remap(7, &catalog_style).unwrap();
        match op {
            DbOpTemplate::RemapRoot {
                root_id,
                new_absolute_path,
            } => {
                assert_eq!(root_id, 7);
                assert_eq!(
                    new_absolute_path, catalog_style,
                    "do katalogu se musí zapsat původní katalogový tvar, ne nativní"
                );
            }
            other => panic!("expected RemapRoot, got {other:?}"),
        }
    }

    #[test]
    fn plan_folder_repoint_inserts_remap_when_target_unknown() {
        let known = std::collections::HashMap::new();
        let roots = vec![root(1, "C:/Foto/")];

        let plan = plan_folder_repoint(
            10,
            &VerifiedPath::unchecked(r"C:\Foto\2019-Chorvatsko"),
            &roots,
            &known,
        )
        .unwrap();
        assert!(matches!(
            plan,
            DbOpTemplate::RemapFolder {
                folder_id: 10,
                new_root_id: 1,
                ..
            }
        ));
        if let DbOpTemplate::RemapFolder {
            new_path_from_root, ..
        } = plan
        {
            assert_eq!(new_path_from_root, "2019-Chorvatsko/");
        }
    }

    #[test]
    fn plan_folder_repoint_refuses_when_target_already_exists_as_different_folder() {
        let files = vec![catalog_file_at(1, 99, r"C:\Foto\Existujici\a.CR2")];
        let known = known_folders_by_path(&files);
        let roots = vec![root(1, "C:/Foto/")];

        let err = plan_folder_repoint(
            10,
            &VerifiedPath::unchecked(r"C:\Foto\Existujici"),
            &roots,
            &known,
        )
        .unwrap_err();
        assert_eq!(
            err,
            ManualResolveError::TargetFolderAlreadyExists {
                existing_folder_id: 99
            }
        );
    }

    #[test]
    fn plan_folder_repoint_refuses_when_no_root_matches() {
        let known = std::collections::HashMap::new();
        let roots = vec![root(1, "C:/Foto/")];

        let err = plan_folder_repoint(
            10,
            &VerifiedPath::unchecked(r"D:\Jina\Slozka"),
            &roots,
            &known,
        )
        .unwrap_err();
        assert!(matches!(err, ManualResolveError::NoMatchingRoot(_)));
    }

    #[test]
    fn remap_folder_commits_and_repoints_all_files_via_folder_id() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::RemapFolder {
                folder_id: 10,
                new_root_id: 1,
                new_path_from_root: "2019-Chorvatsko/".to_string(),
            }],
        };
        let outcome = apply(&catalog, &journal_path, "sess1", plan).unwrap();
        assert!(matches!(outcome.db_phase, DbPhaseResult::Committed));

        let conn = Connection::open(&catalog).unwrap();
        let (path_from_root, root_folder): (String, i64) = conn
            .query_row(
                "SELECT pathFromRoot, rootFolder FROM AgLibraryFolder WHERE id_local = 10",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(path_from_root, "2019-Chorvatsko/");
        assert_eq!(root_folder, 1);

        // Soubor 100 patří do folder=10 - jeden UPDATE AgLibraryFolder ho
        // repointuje, aniž by se AgLibraryFile vůbec dotkl (PROJECT.md §7.1:
        // "jeden UPDATE... místo N updatů souborů").
        let file_folder: i64 = conn
            .query_row(
                "SELECT folder FROM AgLibraryFile WHERE id_local = 100",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(file_folder, 10);
    }

    #[test]
    fn remap_folder_refuses_unknown_root() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::RemapFolder {
                folder_id: 10,
                new_root_id: 99999,
                new_path_from_root: "2019/".to_string(),
            }],
        };
        let outcome = apply(&catalog, &journal_path, "sess1", plan).unwrap();
        assert!(matches!(outcome.db_phase, DbPhaseResult::RolledBack { .. }));

        let conn = Connection::open(&catalog).unwrap();
        let path_from_root: String = conn
            .query_row(
                "SELECT pathFromRoot FROM AgLibraryFolder WHERE id_local = 10",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(path_from_root, "2019/", "katalog nesmí být pozměněný");
    }

    #[test]
    fn undo_reverses_remap_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::RemapFolder {
                folder_id: 10,
                new_root_id: 1,
                new_path_from_root: "2019-Chorvatsko/".to_string(),
            }],
        };
        apply(&catalog, &journal_path, "sess1", plan).unwrap();

        let conn = Connection::open(&catalog).unwrap();
        crate::journal::undo::undo_session(&conn, &journal_path, "sess1").unwrap();

        let (path_from_root, root_folder): (String, i64) = conn
            .query_row(
                "SELECT pathFromRoot, rootFolder FROM AgLibraryFolder WHERE id_local = 10",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(path_from_root, "2019/");
        assert_eq!(root_folder, 1);
    }

    #[test]
    fn undo_reverses_case_c_rename_including_lc_idx_filename() {
        let tmp = tempfile::tempdir().unwrap();
        let catalog = tmp.path().join("Test.lrcat");
        make_catalog(&catalog);
        let journal_path = tmp.path().join("journal.jsonl");

        let plan = Plan {
            fs_ops: vec![],
            db_ops: vec![DbOpTemplate::RenameFile {
                file_id: 100,
                new_base_name: "Renamed".to_string(),
                new_extension: "CR2".to_string(),
                new_idx_filename: "Renamed.CR2".to_string(),
            }],
        };
        apply(&catalog, &journal_path, "sess1", plan).unwrap();

        let conn = Connection::open(&catalog).unwrap();
        crate::journal::undo::undo_session(&conn, &journal_path, "sess1").unwrap();

        let (base, idx, lc): (String, String, String) = conn
            .query_row("SELECT baseName, idx_filename, lc_idx_filename FROM AgLibraryFile WHERE id_local = 100", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(base, "IMG_0001");
        assert_eq!(idx, "IMG_0001.CR2");
        assert_eq!(lc, "img_0001.cr2");
    }
}
