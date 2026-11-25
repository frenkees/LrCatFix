//! Jednorázový PRŮZKUMNÝ skript (CLAUDE.md §6 bod 2): kdyby se fotky
//! vysypaly ze složek s hex jménem do jejich rodiče, kolik by jich narazilo
//! na kolizi jména?
//!
//! Tohle rozhoduje o proveditelnosti celé operace: kolize = odmítnutí
//! (`catalog::apply` pre-flight, §8 bod 0.4), takže vysoký počet kolizí by
//! znamenal, že se plán z velké části neprovede.
//!
//! Výhradně READ-ONLY. Spouštět jen nad KOPIÍ katalogu (CLAUDE.md §7).
//!
//! ```text
//! cargo run --release --example explore_misnested -- <cesta.lrcat>
//! ```

use std::collections::{HashMap, HashSet};

use rusqlite::{Connection, OpenFlags};

fn leaf(rel: &str) -> &str {
    rel.trim_end_matches('/').rsplit('/').next().unwrap_or("")
}

fn parent(rel: &str) -> &str {
    let t = rel.trim_end_matches('/');
    t.rsplit_once('/').map(|(p, _)| p).unwrap_or("")
}

/// 4 znaky, samé hex číslice, aspoň jedno písmeno A-F (jinak by prošly i roky).
fn is_hex_name(n: &str) -> bool {
    n.len() == 4
        && n.chars().all(|c| c.is_ascii_hexdigit())
        && n.chars().any(|c| c.is_ascii_alphabetic())
}

fn main() {
    let Some(catalog) = std::env::args().nth(1) else {
        eprintln!("použití: explore_misnested <cesta.lrcat>");
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

    let mut stmt = match conn.prepare(
        "SELECT rf.absolutePath, fo.pathFromRoot, fi.idx_filename \
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
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
        ))
    });
    let Ok(rows) = rows else { return };

    // Cílový adresář -> jména, která tam po sloučení skončí.
    let mut landing: HashMap<String, Vec<String>> = HashMap::new();
    // Jména, která v cílovém adresáři UŽ TEĎ jsou (soubory mimo hex složky).
    let mut existing: HashMap<String, HashSet<String>> = HashMap::new();
    let mut moved = 0usize;

    for (root, rel, filename) in rows.flatten() {
        let key = filename.to_lowercase();
        if is_hex_name(leaf(&rel)) {
            let dst = format!("{root}{}", parent(&rel));
            landing.entry(dst).or_default().push(key);
            moved += 1;
        } else {
            let dir = format!("{root}{}", rel.trim_end_matches('/'));
            existing.entry(dir).or_default().insert(key);
        }
    }

    let mut collide_with_existing = 0usize;
    let mut collide_among_moved = 0usize;
    let mut worst: Vec<(String, usize)> = Vec::new();

    for (dst, names) in &landing {
        let already = existing.get(dst);
        let mut seen: HashMap<&String, usize> = HashMap::new();
        for n in names {
            *seen.entry(n).or_default() += 1;
            if already.is_some_and(|s| s.contains(n)) {
                collide_with_existing += 1;
            }
        }
        let dupes: usize = seen.values().filter(|c| **c > 1).map(|c| c - 1).sum();
        collide_among_moved += dupes;
        if dupes > 0 {
            worst.push((dst.clone(), dupes));
        }
    }
    worst.sort_by_key(|(_, d)| std::cmp::Reverse(*d));

    println!("fotek v hex složkách (k přesunu):        {moved}");
    println!("cílových adresářů:                       {}", landing.len());
    println!("kolize s už existujícím souborem v cíli: {collide_with_existing}");
    println!("kolize mezi přesouvanými navzájem:       {collide_among_moved}");
    let total = collide_with_existing + collide_among_moved;
    println!(
        "\nprošlo by bez kolize: {} z {moved} ({:.1} %)",
        moved.saturating_sub(total),
        (moved.saturating_sub(total)) as f64 / moved.max(1) as f64 * 100.0
    );
    for (dst, d) in worst.iter().take(6) {
        println!("  nejhorší cíl: {d} kolizí  {}", dst.replace('/', "\\"));
    }
}
