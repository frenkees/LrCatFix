//! Integrační test M0 (PROJECT.md §11): OPEN → EXTRACT → VERIFY nad fixture
//! katalogem. Ověřuje, že se pipeline nikdy nepokusí zapsat do katalogu, a že
//! všechny scénáře z CLAUDE.md §7 (diakritika, dlouhé cesty, sidecary,
//! RAW+JPEG, chybějící soubor/složka) fungují dohromady, ne jen izolovaně.
//!
//! Fixture se vytváří do vlastní `tempfile::TempDir` (CLAUDE.md §7: „sdílený
//! mutable fixture je zdroj flaky testů").

#[path = "fixtures/make.rs"]
mod make;

use std::collections::HashMap;

use lrcatfix::catalog::{extract, open};
use lrcatfix::cluster::root_remap;
use lrcatfix::fsops::verify::{self, FileStatus};

#[test]
fn m0_pipeline_open_extract_verify() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = make::build(tmp.path());

    let handle =
        open::open_read_only(&fx.catalog_path).expect("fixture katalog musí projít schema guardem");
    assert!(
        !handle.lock_status.is_locked(),
        "čerstvě vytvořený fixture katalog nemá být zamčený"
    );
    assert!(
        fx.root_dir.exists(),
        "fixture musí reálně vytvořit strom souborů pod root_dir"
    );

    let roots = extract::list_root_folders(&handle.conn).unwrap();
    assert_eq!(
        roots.len(),
        2,
        "očekávaný lokální root + jeden cizí (macOS-style) root"
    );

    #[cfg(windows)]
    {
        let foreign = extract::foreign_roots(&roots);
        assert_eq!(foreign.len(), 1);
        assert_eq!(foreign[0].root_id, fx.foreign_root_id);
    }

    let files = extract::extract(&handle.conn).unwrap();

    // Fixture roste s každým novým vzorem, takže se netvrdí přesný počet
    // (to by znamenalo měnit číslo při každém rozšíření), ale invariant,
    // který musí platit vždy: EXTRACT čte JEN `AgLibraryFile`, takže se
    // počet musí přesně shodovat s počtem řádků v té tabulce - žádný řádek
    // navíc z `Adobe_images` (virtuální kopie, §3.2) ani chybějící.
    let file_rows: i64 = handle
        .conn
        .query_row("SELECT count(*) FROM AgLibraryFile", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        files.len() as i64,
        file_rows,
        "EXTRACT musí vrátit právě jeden záznam na řádek AgLibraryFile"
    );

    // Fixture obsahuje virtuální kopii (CLAUDE.md §7) - ta NESMÍ zdvojit
    // fyzický soubor, na který ukazuje.
    let virtual_copies: i64 = handle
        .conn
        .query_row(
            "SELECT count(*) FROM Adobe_images WHERE copyName IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        virtual_copies > 0,
        "fixture musí obsahovat virtuální kopii, jinak tenhle invariant nic netestuje"
    );
    assert_eq!(
        files.iter().filter(|f| f.file_id == fx.ok_file_id).count(),
        1,
        "soubor s virtuální kopií se smí v EXTRACT objevit právě jednou (§3.2)"
    );

    let io_threads = 4;
    let results = verify::verify(&files, io_threads);
    let status_by_id: HashMap<i64, FileStatus> =
        results.into_iter().map(|r| (r.file_id, r.status)).collect();

    assert_eq!(status_by_id[&fx.ok_file_id], FileStatus::Ok);
    assert_eq!(status_by_id[&fx.missing_file_id], FileStatus::Missing);
    assert_eq!(
        status_by_id[&fx.folder_missing_file_id],
        FileStatus::FolderMissing
    );
    assert_eq!(
        status_by_id[&fx.nfd_disk_file_id],
        FileStatus::Ok,
        "NFC jméno v katalogu se musí spárovat s NFD jménem na disku (§16.3)"
    );
    assert_eq!(status_by_id[&fx.raw_jpeg_raw_id], FileStatus::Ok);
    assert_eq!(
        status_by_id[&fx.raw_jpeg_jpeg_id],
        FileStatus::Ok,
        "RAW a JPEG jsou dva nezávislé řádky"
    );
    assert_eq!(status_by_id[&fx.sidecar_raw_id], FileStatus::Ok);
    assert_eq!(
        status_by_id[&fx.long_path_file_id],
        FileStatus::Ok,
        "soubor s cestou > 260 znaků se musí ověřit přes verbatim prefix (§16.1)"
    );

    // Prázdná složka existuje na disku a žádný soubor v katalogu na ni
    // neukazuje - VERIFY se jí ani nedotkne, ale musí zůstat nedotčená.
    assert!(fx.empty_folder_path.exists());
    assert_eq!(std::fs::read_dir(&fx.empty_folder_path).unwrap().count(), 0);
}

#[test]
fn m0_pipeline_never_writes_to_catalog() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = make::build(tmp.path());
    let handle = open::open_read_only(&fx.catalog_path).unwrap();

    let err = handle
        .conn
        .execute(
            "UPDATE AgLibraryFile SET folder = folder WHERE id_local = 100",
            [],
        )
        .expect_err("spojení otevřené SQLITE_OPEN_READ_ONLY musí zápis odmítnout");
    let msg = err.to_string();
    assert!(
        msg.to_lowercase().contains("read-only") || msg.to_lowercase().contains("readonly"),
        "neočekávaná chybová hláška: {msg}"
    );
}

#[test]
fn m0_root_remap_probe_flags_only_the_missing_root() {
    let tmp = tempfile::tempdir().unwrap();
    let fx = make::build(tmp.path());
    let handle = open::open_read_only(&fx.catalog_path).unwrap();
    let roots = extract::list_root_folders(&handle.conn).unwrap();

    let needing_probe = root_remap::roots_needing_probe(&roots);
    let ids: Vec<i64> = needing_probe.iter().map(|r| r.root_id).collect();

    assert!(
        !ids.contains(&1),
        "lokální root fixture existuje na disku, nemá se sondovat"
    );
    assert!(
        ids.contains(&fx.foreign_root_id),
        "cizí root neexistuje na disku, sonda ho má zachytit"
    );
}
