//! Povinný test kompenzační fáze (CLAUDE.md §4: `cargo test --test
//! compensation`; PROJECT.md §8 fáze 3; §12 „Test kompenzace").
//!
//! Vynutí selhání fáze DATABÁZE (dotaz na neexistující `root_id` uprostřed
//! transakce, PO tom, co jeden platný `UPDATE` už proběhl) a ověří, že:
//! 1. všechny soubory přesunuté ve fázi FILESYSTÉM se vrátí zpět bit-po-bitu
//!    na svá původní místa,
//! 2. katalog zůstane úplně nedotčený (i ta část transakce, která před
//!    selháním stihla proběhnout, se vrátí přes `ROLLBACK`),
//! 3. kompenzace neselhala u žádného souboru.

use std::collections::HashMap;
use std::path::PathBuf;

use rusqlite::Connection;

use lrcatfix::catalog::apply::{
    run_db_phase, run_fs_phase, DbOpTemplate, DbPhaseResult, FileMoveOp,
};
use lrcatfix::journal::write::JournalWriter;

fn make_catalog(path: &std::path::Path) {
    let conn = Connection::open(path).unwrap();
    conn.execute_batch(
        "CREATE TABLE AgLibraryRootFolder (id_local INTEGER PRIMARY KEY, id_global TEXT, absolutePath TEXT, name TEXT);
         CREATE TABLE AgLibraryFolder (id_local INTEGER PRIMARY KEY, id_global TEXT, pathFromRoot TEXT, rootFolder INTEGER);
         CREATE TABLE AgLibraryFile (id_local INTEGER PRIMARY KEY, id_global TEXT, folder INTEGER, baseName TEXT,
                                      extension TEXT, idx_filename TEXT, lc_idx_filename TEXT, externalModTime TEXT,
                                      md5 TEXT, sidecarExtensions TEXT);
         INSERT INTO AgLibraryRootFolder VALUES (1, 'R1', 'E:/Foto/', 'Foto');",
    )
    .unwrap();
}

#[test]
fn compensation_reverts_file_moves_and_leaves_catalog_untouched_after_forced_db_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog_path = tmp.path().join("Test.lrcat");
    make_catalog(&catalog_path);
    let journal_path = tmp.path().join("journal.jsonl");

    // --- Připrav N souborů a jejich plánovaný přesun (fáze FILESYSTÉM) ---
    let source_dir = tmp.path().join("puvodni_slozka");
    let target_dir = tmp.path().join("nove_misto");
    std::fs::create_dir_all(&source_dir).unwrap();
    std::fs::create_dir_all(&target_dir).unwrap();

    let file_names = ["IMG_0001.CR2", "IMG_0002.CR2", "IMG_0003.CR2"];
    let mut original_bytes: HashMap<PathBuf, Vec<u8>> = HashMap::new();
    let mut fs_ops = Vec::new();

    for (i, name) in file_names.iter().enumerate() {
        let src = source_dir.join(name);
        let dst = target_dir.join(name);
        let content = format!("raw-data-{i}-{}", "x".repeat(100)).into_bytes();
        std::fs::write(&src, &content).unwrap();
        original_bytes.insert(src.clone(), content);
        fs_ops.push(FileMoveOp {
            src,
            dst,
            group_id: None,
        });
    }

    let mut journal = JournalWriter::open(&journal_path, "compensation-test").unwrap();

    // --- Fáze 1: FILESYSTÉM - musí všech N přesunů dokončit úspěšně ---
    let fs_result = run_fs_phase(&fs_ops, &mut journal).unwrap();
    assert_eq!(fs_result.committed.len(), 3, "všechny 3 přesuny musí uspět");
    assert!(fs_result.failed.is_empty());
    assert!(fs_result.rejected.is_empty());
    for op in &fs_result.committed {
        assert!(!op.src.exists(), "zdroj musí po přesunu zmizet");
        assert!(op.dst.exists(), "cíl musí po přesunu existovat");
    }

    // --- Fáze 2: DATABÁZE - první op platný (provede se v transakci), druhý
    // odkazuje na neexistující root_id a selže -> celá transakce se musí
    // vrátit (i ten první, mezitím provedený UPDATE). ---
    let db_ops = vec![
        DbOpTemplate::RemapRoot {
            root_id: 1,
            new_absolute_path: "Z:/Docasne/".to_string(),
        },
        DbOpTemplate::RemapRoot {
            root_id: 99999,
            new_absolute_path: "Z:/Nikdy/".to_string(),
        },
    ];

    let db_result =
        run_db_phase(&catalog_path, &mut journal, &db_ops, &fs_result.committed).unwrap();

    let compensation_failures = match db_result {
        DbPhaseResult::RolledBack {
            compensation_failures,
            ..
        } => compensation_failures,
        DbPhaseResult::Committed => {
            panic!("DB fáze měla selhat (root_id 99999 neexistuje), ale commitla")
        }
    };
    assert!(
        compensation_failures.is_empty(),
        "kompenzace nesmí selhat u žádného souboru: {compensation_failures:?}"
    );

    // --- Ověření 1: katalog je ÚPLNĚ nedotčený, včetně první (jinak platné) změny ---
    let conn = Connection::open(&catalog_path).unwrap();
    let path: String = conn
        .query_row(
            "SELECT absolutePath FROM AgLibraryRootFolder WHERE id_local = 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        path, "E:/Foto/",
        "i částečně provedená transakce se musí celá vrátit (ROLLBACK)"
    );

    // --- Ověření 2: všechny soubory jsou zpátky na původním místě, bit-po-bitu ---
    for (src, expected_bytes) in &original_bytes {
        assert!(
            src.exists(),
            "soubor {} se musí vrátit na původní místo",
            src.display()
        );
        let actual = std::fs::read(src).unwrap();
        assert_eq!(
            &actual,
            expected_bytes,
            "obsah souboru {} po kompenzaci neodpovídá originálu",
            src.display()
        );
    }
    for op in &fs_result.committed {
        assert!(
            !op.dst.exists(),
            "dočasné umístění {} nesmí po kompenzaci zůstat",
            op.dst.display()
        );
    }

    // --- Ověření 3: journal zaznamenal kompenzaci, ne tiše ---
    let entries =
        lrcatfix::journal::write::read_session(&journal_path, "compensation-test").unwrap();
    let compensated_count = entries
        .iter()
        .filter(|e| e.state == lrcatfix::journal::write::JournalState::Compensated)
        .count();
    assert_eq!(
        compensated_count, 3,
        "journal musí mít záznam o kompenzaci pro každý ze 3 souborů"
    );
}
