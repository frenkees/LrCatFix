//! Plný průchod pipeline nad REALISTICKÝM fixture katalogem, včetně ZÁPISU.
//!
//! Proč zvlášť vedle `m0_pipeline.rs`: ten ověřuje jen read-only část (OPEN →
//! EXTRACT → VERIFY). Všechno od M3 dál (APPLY, journal, undo, CLUSTER vzory)
//! se do téhle chvíle testovalo jen proti drobným ručně psaným in-memory
//! katalogům - realistický katalog s diakritikou, dlouhými cestami, sidecary,
//! virtuální kopií a rozsypanými složkami zápisovou cestou nikdy neprošel.
//!
//! CLAUDE.md §10 vyžaduje u M3+ ověřit, že „po zápisu LrC katalog otevře".
//! Lightroom tu spustit nejde, takže se ověřuje to, co jde automatizovat a co
//! by případné poškození odhalilo:
//!   1. `PRAGMA integrity_check` po zápisu,
//!   2. katalog jde znovu otevřít přes `open_read_only` (tedy projde i schema
//!      guardem),
//!   3. kontrolní dotazy §8.1 (žádný osiřelý soubor/složka, počet souborů
//!      beze změny),
//!   4. VERIFY po zápisu vidí opravený stav.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::Connection;

use lrcatfix::catalog::apply::{self, DbPhaseResult, Plan, VerifiedPath};
use lrcatfix::catalog::{extract, open};
use lrcatfix::cluster::{album_scattered, folder_moved, scattered};
use lrcatfix::fsops::verify::{self, FileStatus};
use lrcatfix::index::NameIndex;
use lrcatfix::matching::score::{self, FileMatches};

#[path = "fixtures/make.rs"]
mod make;

/// Postaví `NameIndex` ze skutečných souborů na disku pod `dir` - stejný tvar
/// dat, jaký by dodal INDEX backend, ale bez závislosti na Everything/MFT
/// (a tedy bez elevace a bez skenování celého disku v testu).
fn index_dir(dir: &Path, index: &mut NameIndex) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            index_dir(&path, index);
        } else {
            index.insert(path);
        }
    }
}

fn statuses_of(files: &[extract::CatalogFile]) -> HashMap<i64, FileStatus> {
    verify::verify(files, 4)
        .into_iter()
        .map(|r| (r.file_id, r.status))
        .collect()
}

fn integrity_ok(catalog: &Path) -> bool {
    let conn = Connection::open(catalog).unwrap();
    let result: String = conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .unwrap();
    result == "ok"
}

/// Kontrolní dotazy §8.1 - po každém zápisu musí platit.
fn assert_catalog_consistent(catalog: &Path, expected_file_count: i64) {
    assert!(
        integrity_ok(catalog),
        "PRAGMA integrity_check po zápisu neprošel"
    );

    // Katalog musí jít znovu otevřít - projde tím i schema guard.
    let handle = open::open_read_only(catalog).expect("katalog po zápisu musí jít otevřít");

    let orphan_files: i64 = handle
        .conn
        .query_row(
            "SELECT count(*) FROM AgLibraryFile f
             LEFT JOIN AgLibraryFolder fo ON f.folder = fo.id_local
             WHERE fo.id_local IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphan_files, 0, "soubor ukazuje na neexistující složku");

    let orphan_folders: i64 = handle
        .conn
        .query_row(
            "SELECT count(*) FROM AgLibraryFolder fo
             LEFT JOIN AgLibraryRootFolder rf ON fo.rootFolder = rf.id_local
             WHERE rf.id_local IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(orphan_folders, 0, "složka ukazuje na neexistující kořen");

    let count: i64 = handle
        .conn
        .query_row("SELECT count(*) FROM AgLibraryFile", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        count, expected_file_count,
        "počet souborů v katalogu se změnil, i když neměl (§8.1)"
    );
}

/// Vzor A (§7.1) end-to-end: detekce nad realistickým katalogem → repoint
/// jedním UPDATE → katalog je pořád konzistentní a VERIFY vidí opravu.
#[test]
fn folder_moved_repoint_survives_on_realistic_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = make::build(tmp.path());

    let handle = open::open_read_only(&fx.catalog_path).unwrap();
    let roots = extract::list_root_folders(&handle.conn).unwrap();
    let files = extract::extract(&handle.conn).unwrap();
    let file_count = files.len() as i64;
    let statuses = statuses_of(&files);
    drop(handle);

    // Soubory vzoru A musí být před opravou nedohledatelné.
    for id in &fx.folder_moved_file_ids {
        assert_eq!(
            statuses[id],
            FileStatus::FolderMissing,
            "fixture vzoru A musí začínat s chybějící složkou"
        );
    }

    let mut index = NameIndex::default();
    index_dir(tmp.path(), &mut index);
    let missing: Vec<&extract::CatalogFile> = files
        .iter()
        .filter(|f| statuses[&f.file_id].is_repairable())
        .collect();
    let matches: HashMap<i64, FileMatches> = score::match_all(
        &missing,
        &files,
        &index,
        &lrcatfix::matching::calibrate::Calibration::disabled(),
        true,
    )
    .into_iter()
    .map(|m| (m.file_id, m))
    .collect();

    let suggestions = folder_moved::detect(&files, &statuses, &matches, &Default::default());
    let suggestion = suggestions
        .iter()
        .find(|s| s.folder_id == fx.folder_moved_folder_id)
        .expect("vzor A se musí nad fixture detekovat");
    assert_eq!(
        suggestion.target_dir, fx.folder_moved_target,
        "cíl musí být složka, kam se soubory 'přestěhovaly'"
    );

    // --- ZÁPIS ---
    let known_folders = apply::known_folders_by_path(&files);
    let verified = VerifiedPath::check(&suggestion.target_dir).unwrap();
    let db_op = apply::plan_folder_repoint(suggestion.folder_id, &verified, &roots, &known_folders)
        .unwrap();

    let mut journal_path = fx.catalog_path.as_os_str().to_os_string();
    journal_path.push(".journal.jsonl");
    let outcome = apply::apply(
        &fx.catalog_path,
        Path::new(&journal_path),
        "test-folder-moved",
        Plan {
            fs_ops: vec![],
            db_ops: vec![db_op],
        },
    )
    .unwrap();
    assert!(matches!(outcome.db_phase, DbPhaseResult::Committed));

    assert_catalog_consistent(&fx.catalog_path, file_count);

    // §8 fáze 4.2: po zápisu znovu VERIFY - soubory teď musí být nalezené.
    let handle = open::open_read_only(&fx.catalog_path).unwrap();
    let files_after = extract::extract(&handle.conn).unwrap();
    let statuses_after = statuses_of(&files_after);
    for id in &fx.folder_moved_file_ids {
        assert_eq!(
            statuses_after[id],
            FileStatus::Ok,
            "po repointu musí být soubor vzoru A nalezený"
        );
    }

    // Zbytek katalogu se nesměl rozbít - kontrolní vzorek napříč variantami.
    assert_eq!(statuses_after[&fx.ok_file_id], FileStatus::Ok);
    assert_eq!(
        statuses_after[&fx.nfd_disk_file_id],
        FileStatus::Ok,
        "NFD/NFC párování musí po zápisu pořád fungovat"
    );
    assert_eq!(
        statuses_after[&fx.long_path_file_id],
        FileStatus::Ok,
        "cesta > 260 znaků musí po zápisu pořád fungovat"
    );
    assert_eq!(statuses_after[&fx.sidecar_raw_id], FileStatus::Ok);
}

/// Ruční resolve (§6.2) end-to-end nad souborem s diakritikou v cestě, včetně
/// undo (§9) - katalog se musí vrátit přesně do původního stavu.
#[test]
fn manual_resolve_then_undo_restores_catalog_on_realistic_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = make::build(tmp.path());

    let handle = open::open_read_only(&fx.catalog_path).unwrap();
    let roots = extract::list_root_folders(&handle.conn).unwrap();
    let files = extract::extract(&handle.conn).unwrap();
    let file_count = files.len() as i64;
    drop(handle);

    // Vezmi jeden ze souborů vzoru B (rozsypané) a "vyřeš" ho ručně.
    let file_id = fx.scattered_file_ids[0];
    let file = files.iter().find(|f| f.file_id == file_id).unwrap();
    let folder_before = file.folder_id;

    let mut index = NameIndex::default();
    index_dir(tmp.path(), &mut index);
    let candidate = index
        .candidates(&file.filename)
        .first()
        .expect("rozsypaný soubor musí mít na disku kandidáta")
        .clone();

    let known_folders = apply::known_folders_by_path(&files);
    let verified_file = VerifiedPath::check(&candidate).unwrap();
    let target_dir = verified_file.parent().unwrap();
    let db_op = apply::plan_manual_resolve(file_id, &target_dir, &roots, &known_folders).unwrap();

    let mut journal_path = fx.catalog_path.as_os_str().to_os_string();
    journal_path.push(".journal.jsonl");
    let session = "test-manual-resolve";
    let outcome = apply::apply(
        &fx.catalog_path,
        Path::new(&journal_path),
        session,
        Plan {
            fs_ops: vec![],
            db_ops: vec![db_op],
        },
    )
    .unwrap();
    assert!(matches!(outcome.db_phase, DbPhaseResult::Committed));
    assert_catalog_consistent(&fx.catalog_path, file_count);

    // Soubor teď musí ukazovat jinam než před zápisem.
    let conn = Connection::open(&fx.catalog_path).unwrap();
    let folder_after: i64 = conn
        .query_row(
            "SELECT folder FROM AgLibraryFile WHERE id_local = ?1",
            [file_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_ne!(folder_after, folder_before, "zápis se musel projevit");

    // --- UNDO (§9) ---
    lrcatfix::journal::undo::undo_session(&conn, Path::new(&journal_path), session).unwrap();
    drop(conn);

    assert_catalog_consistent(&fx.catalog_path, file_count);
    let conn = Connection::open(&fx.catalog_path).unwrap();
    let folder_restored: i64 = conn
        .query_row(
            "SELECT folder FROM AgLibraryFile WHERE id_local = ?1",
            [file_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        folder_restored, folder_before,
        "undo musí vrátit katalog do původního stavu (CLAUDE.md M6 DoD)"
    );
}

/// Vzory B a C se nad fixture musí detekovat - jinak by je žádný test
/// neprocvičil na realistických datech.
#[test]
fn scattered_and_album_patterns_are_detected_on_realistic_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = make::build(tmp.path());

    let handle = open::open_read_only(&fx.catalog_path).unwrap();
    let files = extract::extract(&handle.conn).unwrap();
    let statuses = statuses_of(&files);

    let mut index = NameIndex::default();
    index_dir(tmp.path(), &mut index);
    let missing: Vec<&extract::CatalogFile> = files
        .iter()
        .filter(|f| statuses[&f.file_id].is_repairable())
        .collect();
    let matches: HashMap<i64, FileMatches> = score::match_all(
        &missing,
        &files,
        &index,
        &lrcatfix::matching::calibrate::Calibration::disabled(),
        true,
    )
    .into_iter()
    .map(|m| (m.file_id, m))
    .collect();

    // Vzor A si nejdřív vezme svoje složky (§7.4 pořadí D → A → B → C).
    let a = folder_moved::detect(&files, &statuses, &matches, &Default::default());
    let claimed: std::collections::HashSet<i64> = a.iter().map(|s| s.folder_id).collect();

    let b = scattered::detect(&files, &statuses, &matches, &Default::default(), &claimed);
    let scattered_hit = b
        .iter()
        .find(|s| s.folder_id == fx.scattered_folder_id)
        .expect("vzor B (rozsypaná složka) se musí nad fixture detekovat");
    assert!(
        scattered_hit.distinct_targets >= 5,
        "fixture vzoru B musí mít aspoň 5 různých cílů, má {}",
        scattered_hit.distinct_targets
    );

    // Vzor C - schéma kolekcí ve fixture je, takže detekce musí proběhnout.
    assert!(
        lrcatfix::catalog::schema::has_collection_schema(&handle.conn),
        "fixture musí mít schéma kolekcí, jinak se vzor C nikdy neotestuje"
    );
    let members = extract::collection_membership(&handle.conn).unwrap();
    assert!(
        !members.is_empty(),
        "fixture musí mít naplněnou kolekci (§7.3)"
    );
    let c = album_scattered::detect(&members, &files, &statuses);
    let album_hit = c
        .iter()
        .find(|s| s.collection_id == fx.collection_id)
        .expect("vzor C se musí nad fixture detekovat");
    assert!(album_hit.distinct_folders >= 10);
    assert!(album_hit.missing_ratio() >= 0.3);
}

/// Nedostupný (ne chybějící) svazek se nesmí tvářit jako ztracené fotky -
/// nejzávažnější nález auditu, ověřený nad realistickým katalogem.
#[test]
fn unavailable_folder_never_becomes_a_repair_suggestion() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = make::build(tmp.path());

    let handle = open::open_read_only(&fx.catalog_path).unwrap();
    let files = extract::extract(&handle.conn).unwrap();
    drop(handle);

    // Simulace nedostupné složky: nahradíme adresář SOUBOREM. `read_dir` pak
    // selže jinou chybou než `NotFound` - přesně jako u odpojeného NAS.
    let hr_dir = fx.root_dir.join("2019").join("2019-07 Chorvatsko");
    std::fs::remove_dir_all(&hr_dir).unwrap();
    std::fs::write(&hr_dir, b"uz to neni slozka").unwrap();

    let statuses = statuses_of(&files);
    assert_eq!(
        statuses[&fx.ok_file_id],
        FileStatus::FolderUnavailable,
        "nečitelná složka musí být FolderUnavailable, ne FolderMissing"
    );

    let mut index = NameIndex::default();
    index_dir(tmp.path(), &mut index);
    let missing: Vec<&extract::CatalogFile> = files
        .iter()
        .filter(|f| statuses[&f.file_id].is_repairable())
        .collect();
    assert!(
        !missing.iter().any(|f| f.file_id == fx.ok_file_id),
        "soubor na nedostupném svazku nesmí vstoupit do MATCH"
    );

    let matches: HashMap<i64, FileMatches> = score::match_all(
        &missing,
        &files,
        &index,
        &lrcatfix::matching::calibrate::Calibration::disabled(),
        true,
    )
    .into_iter()
    .map(|m| (m.file_id, m))
    .collect();

    let suggestions = folder_moved::detect(&files, &statuses, &matches, &Default::default());
    assert!(
        !suggestions.iter().any(|s| s.folder_id
            == files
                .iter()
                .find(|f| f.file_id == fx.ok_file_id)
                .unwrap()
                .folder_id),
        "nedostupná složka se nesmí objevit jako návrh na opravu"
    );
}
