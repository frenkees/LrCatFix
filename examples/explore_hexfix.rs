//! Jednorázový PRŮZKUMNÝ skript (CLAUDE.md §6 bod 2): jsou GUID-pojmenované
//! DNG v hex složkách Smart Preview k fotkám, které už v katalogu jsou?
//!
//! Smart Preview se jmenuje podle `id_global` původního snímku a leží ve
//! složce pojmenované prvními 4 hex znaky toho GUID. Když jméno souboru sedí
//! na `id_global` JINÉHO řádku v katalogu, je to důkaz, že jde o náhled
//! duplikovaný do knihovny, ne o samostatnou fotku.
//!
//! Výhradně READ-ONLY. Spouštět jen nad KOPIÍ katalogu (CLAUDE.md §7).
//!
//! ```text
//! cargo run --release --example explore_hexfix -- <cesta.lrcat>
//! ```

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OpenFlags};

fn leaf(rel: &str) -> &str {
    rel.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

fn is_hex_name(n: &str) -> bool {
    n.len() == 4
        && n.chars().all(|c| c.is_ascii_hexdigit())
        && n.chars().any(|c| c.is_ascii_alphabetic())
}

fn norm_guid(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_hexdigit())
        .flat_map(|c| c.to_uppercase())
        .collect()
}

fn main() {
    let Some(catalog) = std::env::args().nth(1) else {
        eprintln!("použití: explore_hexfix <cesta.lrcat>");
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

    // Všechna id_global v katalogu -> kde leží.
    let mut file_guid: HashMap<String, String> = HashMap::new();
    let mut image_guid: HashSet<String> = HashSet::new();

    if let Ok(mut s) = conn.prepare(
        "SELECT fi.id_global, rf.absolutePath || fo.pathFromRoot || fi.idx_filename \
         FROM AgLibraryFile fi \
         JOIN AgLibraryFolder fo ON fo.id_local = fi.folder \
         JOIN AgLibraryRootFolder rf ON rf.id_local = fo.rootFolder",
    ) {
        if let Ok(rows) = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        {
            for (g, p) in rows.flatten() {
                file_guid.insert(norm_guid(&g), p);
            }
        }
    }
    if let Ok(mut s) = conn.prepare("SELECT id_global FROM Adobe_images") {
        if let Ok(rows) = s.query_map([], |r| r.get::<_, String>(0)) {
            for g in rows.flatten() {
                image_guid.insert(norm_guid(&g));
            }
        }
    }
    println!("id_global v AgLibraryFile: {}", file_guid.len());
    println!("id_global v Adobe_images:  {}", image_guid.len());

    // Projdi hex soubory a zkus jejich jméno napárovat.
    let mut stmt = match conn.prepare(
        "SELECT fo.pathFromRoot, fi.baseName, rf.absolutePath \
         FROM AgLibraryFile fi \
         JOIN AgLibraryFolder fo ON fo.id_local = fi.folder \
         JOIN AgLibraryRootFolder rf ON rf.id_local = fo.rootFolder",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("dotaz selhal: {e}");
            return;
        }
    };
    let rows = match stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    }) {
        Ok(rows) => rows,
        Err(_) => return,
    };

    let (mut total, mut hit_file, mut hit_image, mut miss) = (0, 0, 0, 0);
    let mut examples = Vec::new();
    for (rel, base, _root) in rows.flatten() {
        if !is_hex_name(leaf(&rel)) {
            continue;
        }
        total += 1;
        let g = norm_guid(&base);
        if g.len() != 32 {
            miss += 1;
            continue;
        }
        // Sedí GUID ze jména na id_global NĚJAKÉHO souboru v katalogu?
        match file_guid.get(&g) {
            Some(orig) => {
                hit_file += 1;
                if examples.len() < 6 {
                    examples.push(format!("{base}.dng  →  originál: {orig}"));
                }
            }
            None => {
                if image_guid.contains(&g) {
                    hit_image += 1;
                } else {
                    miss += 1;
                }
            }
        }
    }

    println!("\nGUID-pojmenovaných DNG v hex složkách: {total}");
    println!("  jméno sedí na id_global JINÉHO souboru v katalogu: {hit_file}");
    println!("  jméno sedí jen na Adobe_images.id_global:          {hit_image}");
    println!("  nesedí na nic:                                     {miss}");
    for e in &examples {
        println!("    {e}");
    }
}
