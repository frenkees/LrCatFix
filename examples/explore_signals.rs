//! Jednorázový PRŮZKUMNÝ skript (CLAUDE.md §6 bod 2).
//!
//! Zjišťuje, jestli by signál „velikost souboru" (dostupný zdarma
//! z `AgLibraryFile.importHash`) rozhodl mezi kandidáty u souborů, které
//! MATCH nechal nerozhodnuté.
//!
//! Ověřený formát `importHash` (LrC 13.3, vzorek 5000 řádků):
//! `<captureTime v Apple epoše>:<originalFilename>:<velikost v bajtech>`
//! - pole[1] == `originalFilename` u 5000/5000,
//! - pole[0] jako Apple epocha do 1 roku od `captureTime` u 5000/5000,
//! - pole[2] == skutečná velikost na disku u 2953/2958 existujících souborů.
//!
//! Výhradně READ-ONLY (SQLite read-only + `stat()`). Spouštět jen nad KOPIÍ
//! katalogu, nikdy nad produkčním (CLAUDE.md §7).
//!
//! ```text
//! cargo run --release --example explore_signals -- <cesta.lrcat> <report.json>
//! ```

use std::collections::HashMap;

use rusqlite::{Connection, OpenFlags};

fn main() {
    let mut args = std::env::args().skip(1);
    let (Some(catalog), Some(report)) = (args.next(), args.next()) else {
        eprintln!("použití: explore_signals <cesta.lrcat> <report.json>");
        std::process::exit(2);
    };

    let conn = match Connection::open_with_flags(
        &catalog,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nejde otevřít katalog read-only: {e}");
            std::process::exit(1);
        }
    };

    // file_id -> velikost podle katalogu
    let mut sizes: HashMap<i64, u64> = HashMap::new();
    let mut stmt = match conn
        .prepare("SELECT id_local, importHash FROM AgLibraryFile WHERE importHash IS NOT NULL")
    {
        Ok(s) => s,
        Err(e) => {
            eprintln!("dotaz selhal: {e}");
            std::process::exit(1);
        }
    };
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)));
    if let Ok(rows) = rows {
        for (id, hash) in rows.flatten() {
            if let Some(size) = hash.rsplit(':').next().and_then(|s| s.parse::<u64>().ok()) {
                sizes.insert(id, size);
            }
        }
    }

    let raw = match std::fs::read_to_string(&report) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("nejde přečíst report: {e}");
            std::process::exit(1);
        }
    };
    let json: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("report není platný JSON: {e}");
            std::process::exit(1);
        }
    };
    let Some(missing) = json.get("missing_files").and_then(|m| m.as_array()) else {
        eprintln!("report neobsahuje missing_files");
        std::process::exit(1);
    };

    let (mut considered, mut no_size, mut unique, mut ambiguous, mut none_match) = (0, 0, 0, 0, 0);
    let mut multi_candidate_resolved = 0;
    let mut multi_candidate_total = 0;

    for entry in missing {
        let Some(file_id) = entry.get("file_id").and_then(|v| v.as_i64()) else {
            continue;
        };
        let Some(candidates) = entry.get("candidates").and_then(|c| c.as_array()) else {
            continue;
        };
        if candidates.is_empty() {
            continue;
        }
        considered += 1;
        let Some(&want) = sizes.get(&file_id) else {
            no_size += 1;
            continue;
        };

        let mut hits = 0;
        for c in candidates {
            let Some(p) = c.get("path").and_then(|v| v.as_str()) else {
                continue;
            };
            if std::fs::metadata(p).map(|m| m.len()).ok() == Some(want) {
                hits += 1;
            }
        }
        if candidates.len() > 1 {
            multi_candidate_total += 1;
            if hits == 1 {
                multi_candidate_resolved += 1;
            }
        }
        match hits {
            0 => none_match += 1,
            1 => unique += 1,
            _ => ambiguous += 1,
        }
    }

    println!("--- rozhodovací síla signálu „velikost\" ---");
    println!("  chybějících souborů s kandidáty     : {considered}");
    println!("  bez velikosti v katalogu            : {no_size}");
    println!("  PRÁVĚ JEDEN kandidát sedí velikostí : {unique}");
    println!("  víc kandidátů sedí stejnou velikostí: {ambiguous}");
    println!("  žádný kandidát velikostí nesedí     : {none_match}");
    println!(
        "\n  z toho tam, kde je kandidátů VÍC než 1 (tj. je co rozhodovat):\n\
         \x20   jednoznačně rozhodnuto {multi_candidate_resolved} / {multi_candidate_total}"
    );
}
