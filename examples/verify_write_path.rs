//! Ověření zápisové cesty na ODHOZENÉ kopii katalogu - Definition of done
//! milníku M3 (PROJECT.md §11: „Katalog po zápisu otevřitelný v LrC, fotky
//! viditelné; test kompenzace prochází").
//!
//! Tohle nesmí být běžný `#[test]`: potřebuje skutečný katalog vytvořený
//! Lightroomem, a ten se podle CLAUDE.md §7 nesmí objevit v repozitáři ani
//! v testech. Proto example, kterému se cesta předá zvenčí.
//!
//! **Sám si udělá vlastní kopii** do `--work <adresář>` a pracuje výhradně
//! s ní. Na zadaný soubor nikdy nesahá.
//!
//! ```text
//! cargo run --release --example verify_write_path -- <cesta.lrcat> <pracovni-adresar>
//! ```

use std::path::{Path, PathBuf};

use lrcatfix::catalog::{apply, extract, open};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(src), Some(work)) = (args.next(), args.next()) else {
        eprintln!("použití: verify_write_path <cesta.lrcat> <pracovni-adresar>");
        std::process::exit(2);
    };
    let src = PathBuf::from(src);
    let work = PathBuf::from(work);

    if let Err(e) = std::fs::create_dir_all(&work) {
        eprintln!("nejde vytvořit pracovní adresář: {e}");
        std::process::exit(1);
    }
    let catalog = work.join("m3-verify.lrcat");

    println!("1) kopíruji katalog do {}", catalog.display());
    if let Err(e) = std::fs::copy(&src, &catalog) {
        eprintln!("kopie selhala: {e}");
        std::process::exit(1);
    }

    // Kořeny přečteme z kopie, ať víme, co vlastně přepsat.
    println!("2) čtu kořeny (read-only)");
    let (root_id, original_path) = {
        let handle = match open::open_read_only(&catalog) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("read-only otevření selhalo: {e}");
                std::process::exit(1);
            }
        };
        let roots = match extract::list_root_folders(&handle.conn) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("čtení kořenů selhalo: {e}");
                std::process::exit(1);
            }
        };
        let Some(first) = roots.into_iter().next() else {
            eprintln!("katalog nemá žádný AgLibraryRootFolder");
            std::process::exit(1);
        };
        println!("   root {} = '{}'", first.root_id, first.absolute_path);
        (first.root_id, first.absolute_path)
    };

    // TEĎ je read-only spojení zavřené a vedle katalogu leží prázdný `-wal`.
    // Přesně ta situace, ve které nástroj dřív odmítal zapisovat.
    print_siblings(&catalog);

    let lock = open::detect_lock(&catalog);
    println!(
        "3) detekce zámků: lock_file={} journal={:?} wal={:?} → is_locked={}",
        lock.lock_file,
        lock.rollback_journal_bytes,
        lock.wal_bytes,
        lock.is_locked()
    );
    if lock.is_locked() {
        eprintln!("   ✗ katalog se tváří zamčený, zápis by se odmítl");
        std::process::exit(1);
    }
    println!("   ✓ prázdný -wal už zápis neblokuje");

    // Zápis, který jde triviálně poznat i vrátit: přepiš kořen na jinou
    // cestu a hned zpátky.
    let probe = format!("{}__M3_TEST__/", original_path.trim_end_matches('/'));
    let journal = work.join("m3-verify.journal.jsonl");

    println!("4) zapisuji: root {root_id} → '{probe}'");
    let plan = apply::Plan {
        fs_ops: vec![],
        db_ops: vec![apply::DbOpTemplate::RemapRoot {
            root_id,
            new_absolute_path: probe.clone(),
        }],
    };
    match apply::apply(&catalog, &journal, "m3-verify", plan) {
        Ok(outcome) => {
            println!("   ✓ zápis proběhl, db_phase={:?}", outcome.db_phase);
            println!("   záloha: {}", outcome.backup.backup_dir.display());
            if outcome.backup.wal_backup.is_some() {
                println!("   (záloha obsahuje i -wal)");
            }
        }
        Err(e) => {
            eprintln!("   ✗ zápis selhal: {e}");
            std::process::exit(1);
        }
    }

    println!("5) kontrola integrity + znovuotevření");
    match verify_catalog(&catalog, root_id) {
        Ok(path) if path == probe => {
            println!("   ✓ integrity_check OK, nová cesta v katalogu sedí")
        }
        Ok(other) => {
            eprintln!("   ✗ v katalogu je '{other}', čekal jsem '{probe}'");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("   ✗ {e}");
            std::process::exit(1);
        }
    }

    println!("6) undo");
    {
        // Undo potřebuje vlastní zapisovací spojení; otevře se stejnou
        // cestou jako APPLY, tedy včetně kontroly zámků.
        let handle = match open::open_read_write(&catalog) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("   ✗ otevření pro undo selhalo: {e}");
                std::process::exit(1);
            }
        };
        match lrcatfix::journal::undo::undo_session(&handle.conn, &journal, "m3-verify") {
            Ok(report) => println!(
                "   ✓ vráceno {} záznamů, přeskočeno {}",
                report.reverted, report.skipped
            ),
            Err(e) => {
                eprintln!("   ✗ undo selhalo: {e}");
                std::process::exit(1);
            }
        }
    }
    match verify_catalog(&catalog, root_id) {
        Ok(path) if path == original_path => {
            println!("   ✓ katalog je zpátky v původním stavu")
        }
        Ok(other) => {
            eprintln!("   ✗ po undo je v katalogu '{other}', čekal jsem '{original_path}'");
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("   ✗ {e}");
            std::process::exit(1);
        }
    }

    println!("\nHOTOVO. Zbývá jediné, co skript udělat nemůže:");
    println!("otevři {} v Lightroom Classic", catalog.display());
    println!("a ověř, že se katalog načte a fotky jsou vidět.");
}

fn print_siblings(catalog: &Path) {
    let Some(dir) = catalog.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    println!("   soubory vedle katalogu:");
    for e in entries.flatten() {
        let len = e.metadata().map(|m| m.len()).unwrap_or(0);
        println!("     {:<28} {len} B", e.file_name().to_string_lossy());
    }
}

/// `PRAGMA integrity_check` + přečtení cesty kořene přes normální read-only
/// otevření (tedy včetně schema guardu).
fn verify_catalog(catalog: &Path, root_id: i64) -> Result<String, String> {
    let handle = open::open_read_only(catalog).map_err(|e| format!("otevření selhalo: {e}"))?;
    let check: String = handle
        .conn
        .query_row("PRAGMA integrity_check", [], |r| r.get(0))
        .map_err(|e| format!("integrity_check selhal: {e}"))?;
    if check != "ok" {
        return Err(format!("integrity_check vrátil '{check}'"));
    }
    let roots = extract::list_root_folders(&handle.conn)
        .map_err(|e| format!("čtení kořenů selhalo: {e}"))?;
    roots
        .into_iter()
        .find(|r| r.root_id == root_id)
        .map(|r| r.absolute_path)
        .ok_or_else(|| format!("root {root_id} v katalogu zmizel"))
}
