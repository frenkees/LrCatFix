//! Jednorázový PRŮZKUMNÝ skript (CLAUDE.md §6 bod 2): kolik `AgLibraryFolder`
//! řádků nemá ŽÁDNÝ soubor, a kolik z nich navíc chybí i na disku.
//!
//! Takové složky projdou EXTRACTem bez povšimnutí - hlavní dotaz jde
//! `FROM AgLibraryFile` přes INNER JOIN, takže složka bez souborů v něm
//! nefiguruje. Tenhle skript zjišťuje, jestli je to teoretická mezera, nebo
//! reálná.
//!
//! Výhradně READ-ONLY (SQLite read-only + `stat()`). Spouštět jen nad KOPIÍ
//! katalogu, nikdy nad produkčním (CLAUDE.md §7).
//!
//! ```text
//! cargo run --release --example explore_folders -- <cesta.lrcat>
//! ```

use rusqlite::{Connection, OpenFlags};

fn main() {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("použití: explore_folders <cesta.lrcat>");
        std::process::exit(2);
    };

    let conn = match Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    ) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("nejde otevřít katalog read-only: {e}");
            std::process::exit(1);
        }
    };

    for (label, sql) in [
        (
            "AgLibraryFolder celkem",
            "SELECT COUNT(*) FROM AgLibraryFolder",
        ),
        (
            "z toho BEZ jediného souboru",
            "SELECT COUNT(*) FROM AgLibraryFolder fo \
             WHERE NOT EXISTS (SELECT 1 FROM AgLibraryFile fi WHERE fi.folder = fo.id_local)",
        ),
    ] {
        match conn.query_row(sql, [], |r| r.get::<_, i64>(0)) {
            Ok(n) => println!("{label:32} {n}"),
            Err(e) => println!("{label:32} (chyba: {e})"),
        }
    }

    // Prázdné složky, které navíc nejsou ani na disku - přesně ty, o kterých
    // by uživatel chtěl vědět a nástroj je dnes nevidí.
    let mut stmt = match conn.prepare(
        "SELECT rf.absolutePath || fo.pathFromRoot \
         FROM AgLibraryFolder fo \
         JOIN AgLibraryRootFolder rf ON rf.id_local = fo.rootFolder \
         WHERE NOT EXISTS (SELECT 1 FROM AgLibraryFile fi WHERE fi.folder = fo.id_local)",
    ) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("dotaz selhal: {e}");
            return;
        }
    };
    let Ok(rows) = stmt.query_map([], |r| r.get::<_, String>(0)) else {
        return;
    };

    let (mut on_disk, mut absent) = (0, 0);
    let mut examples = Vec::new();
    for path in rows.flatten() {
        let native = path.replace('/', "\\");
        if std::path::Path::new(&native).is_dir() {
            on_disk += 1;
        } else {
            absent += 1;
            if examples.len() < 8 {
                examples.push(native);
            }
        }
    }
    println!("\nprázdné složky, které na disku JSOU:   {on_disk}");
    println!("prázdné složky, které na disku NEJSOU: {absent}");
    for e in &examples {
        println!("  {e}");
    }
}
