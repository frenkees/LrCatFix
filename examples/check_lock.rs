//! Vypíše, co si nástroj myslí o zámcích u konkrétního katalogu, a proč.
//!
//! Diagnostická pomůcka pro případ „hlásí to, že je katalog otevřený v LrC,
//! ale není". Čistě read-only - jen `stat()` na sousední soubory, katalog
//! sám se ani neotevírá.
//!
//! ```text
//! cargo run --release --example check_lock -- <cesta.lrcat>
//! ```

use std::path::{Path, PathBuf};

use lrcatfix::catalog::open;

fn main() {
    let Some(catalog) = std::env::args().nth(1) else {
        eprintln!("použití: check_lock <cesta.lrcat>");
        std::process::exit(2);
    };
    let catalog = PathBuf::from(catalog);

    println!("katalog: {}", catalog.display());
    println!();

    for suffix in [".lock", "-journal", "-wal", "-shm"] {
        let mut p = catalog.as_os_str().to_os_string();
        p.push(suffix);
        let p = Path::new(&p);
        match std::fs::metadata(p) {
            Ok(m) => println!("  {suffix:<10} existuje, {} B", m.len()),
            Err(_) => println!("  {suffix:<10} neexistuje"),
        }
    }

    let lock = open::detect_lock(&catalog);
    println!();
    println!("  lock_file              = {}", lock.lock_file);
    println!(
        "  rollback_journal_bytes = {:?}",
        lock.rollback_journal_bytes
    );
    println!("  wal_bytes              = {:?}", lock.wal_bytes);
    println!("  has_pending_wal()      = {}", lock.has_pending_wal());
    println!();

    if lock.is_locked() {
        println!("VERDIKT: ZAMČENO - zápis se odmítne.");
        if lock.lock_file {
            println!("  důvod: existuje .lrcat.lock (vlastní zámek Lightroomu).");
            println!("  Pokud LrC neběží, zůstal tam po pádu - smaž ho ručně.");
        }
        if lock.rollback_journal_bytes.is_some_and(|n| n > 0) {
            println!("  důvod: -journal není prázdný = přerušená transakce.");
        }
        std::process::exit(1);
    } else {
        println!("VERDIKT: ODEMČENO - zápis může proběhnout.");
        if lock.has_pending_wal() {
            println!("  (-wal obsahuje nesloučená data; záloha ho zkopíruje s sebou)");
        }
    }
}
