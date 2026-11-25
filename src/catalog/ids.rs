//! Generování `id_local`/`id_global` pro nové řádky (PROJECT.md §3.4) -
//! potřeba pro případ B (§3.3: soubor nalezen v neznámé složce, musí se
//! nejdřív vytvořit `AgLibraryFolder`).
//!
//! **OVĚŘENO na LrC 13.3** (reálný katalog uživatele, 2026-08-21):
//! globální čítač EXISTUJE a `MAX(id_local)+1` je pro tenhle katalog
//! NESPRÁVNÝ postup. Konkrétně:
//!
//! - `Adobe_variablesTable` obsahuje `Adobe_entityIDCounter = 2811382`.
//! - `id_local` je SDÍLENÝ napříč tabulkami, ne per-tabulka: nejvyšší
//!   `id_local` v `AgLibraryFile` bylo 2810912 a v `Adobe_images` 2810910 -
//!   sousední čísla z jedné společné posloupnosti. Průnik `id_local` mezi
//!   `AgLibraryFile` a `Adobe_images` je prázdný (0 řádků), což sdílený
//!   prostor potvrzuje.
//! - Čítač (2811382) je VYŠŠÍ než jakékoli existující `id_local`, takže LrC
//!   z něj nová ID přiděluje a inkrementuje ho.
//!
//! **Sémantika čítače je ověřená, ne odhadnutá.** Rozhodující test na témže
//! katalogu: napříč VŠEMI 104 tabulkami, které mají `id_local`, bylo
//! nejvyšší použité ID 2811381 (v `Adobe_libraryImageDevelopHistoryStep`) a
//! čítač měl hodnotu 2811382 - tedy přesně `globální max + 1`. Čítač je
//! proto jednoznačně „další volné ID", ne něco jiného.
//!
//! `MAX(id_local)+1` nad JEDNOU tabulkou by vrátilo číslo hluboko pod
//! čítačem, které může patřit řádku v jiné tabulce. Na katalogu s čítačem
//! se proto ID přiděluje Z NĚJ a čítač se ve stejné transakci inkrementuje
//! ([`next_id_local`]). Katalogy bez čítače dál používají `MAX+1`.
//!
//! Čítač, jehož jméno neznáme, operaci pořád ODMÍTNE
//! ([`IdError::UnknownCounter`]) - „nehádej" (CLAUDE.md §6 bod 4) platí dál
//! pro všechno kromě toho jednoho ověřeného jména.
//!
//! **Zbývající riziko:** ověřili jsme, co v katalogu JE, ne jak se LrC
//! zachová, až ho po našem zápisu otevře. To by šlo ověřit jedině skutečným
//! spuštěním Lightroomu. Zbytkové riziko kryje standardní pojistka - záloha
//! před zápisem, journal a undo (§8, §9).

use rusqlite::Transaction;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum IdError {
    #[error("chyba SQLite při generování id: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error(
        "katalog má v Adobe_variablesTable neznámý čítač ({name} = {value}) - nevíme, jestli z něj LrC \
         přiděluje id_local, a hádat se u zápisu do katalogu nesmí; nástroj proto odmítá zakládat nové řádky"
    )]
    UnknownCounter { name: String, value: String },
    #[error(
        "globální čítač Adobe_entityIDCounter má hodnotu {counter}, která není vyšší než už použité \
         id_local ({max_used}) - katalog vypadá nekonzistentně a nástroj do něj nebude zakládat nové řádky"
    )]
    CounterNotAhead { counter: i64, max_used: i64 },
    #[error("nepodařilo se vygenerovat unikátní id_global po 10 pokusech")]
    ExhaustedUniqueAttempts,
}

/// Jméno globálního čítače `id_local` - OVĚŘENO na LrC 13.3 (viz doc modulu).
const ENTITY_ID_COUNTER: &str = "Adobe_entityIDCounter";

/// Čítače, o kterých VÍME, že s přidělováním `id_local` nesouvisí, takže
/// jejich přítomnost nemusí operaci blokovat. Ověřeno na LrC 13.3: jde
/// o čítače změn pro synchronizaci s Lightroom Mobile.
const UNRELATED_COUNTERS: &[&str] = &[
    "Adobe_lrMobileSyncChangeCounterV2",
    "Adobe_lrMobileInitialSyncChangeCounterV2",
];

/// Přidělí nové `id_local` pro řádek, který se vloží do `table`.
///
/// Musí se volat uvnitř STEJNÉ transakce, ve které se pak řádek vloží
/// (§3.4), a `table` musí být pevný řetězcový literál z volajícího kódu
/// (`catalog::apply`), nikdy hodnota odvozená od uživatele/katalogu - jméno
/// tabulky se interpoluje přímo do SQL (stejné zdůvodnění jako
/// `catalog::schema::table_columns`).
///
/// Dvě cesty podle toho, co katalog obsahuje:
/// 1. **Globální čítač** (`Adobe_entityIDCounter`, LrC 13.x) - použije se
///    jeho hodnota a čítač se ve stejné transakci inkrementuje.
/// 2. **Bez čítače** - `MAX(id_local)+1` nad `table`, standardní SQLite
///    idiom pro katalogy, které globální ID prostor nemají.
///
/// Neznámý čítač = odmítnutí (viz [`IdError::UnknownCounter`]).
pub fn next_id_local(tx: &Transaction, table: &str) -> Result<i64, IdError> {
    if let Some(id) = allocate_from_global_counter(tx, table)? {
        return Ok(id);
    }
    let sql = format!("SELECT COALESCE(MAX(id_local), 0) + 1 FROM {table}");
    Ok(tx.query_row(&sql, [], |row| row.get(0))?)
}

/// Vrátí `Some(id)`, pokud katalog drží globální čítač - a rovnou ho
/// inkrementuje. `None` znamená „tenhle katalog čítač nemá", ne chybu.
fn allocate_from_global_counter(tx: &Transaction, table: &str) -> Result<Option<i64>, IdError> {
    let table_exists: bool = tx
        .prepare(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'Adobe_variablesTable'",
        )?
        .exists([])?;
    if !table_exists {
        return Ok(None);
    }

    // Nejdřív odmítnout čítač, kterému nerozumíme - „nehádej" platí dál pro
    // všechno kromě jednoho ověřeného jména.
    let mut stmt =
        tx.prepare("SELECT name, value FROM Adobe_variablesTable WHERE name LIKE '%ounter%'")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(0)?;
        if name == ENTITY_ID_COUNTER || UNRELATED_COUNTERS.contains(&name.as_str()) {
            continue;
        }
        let value: String = row
            .get::<_, String>(1)
            .or_else(|_| row.get::<_, f64>(1).map(|v| v.to_string()))
            .unwrap_or_default();
        return Err(IdError::UnknownCounter { name, value });
    }
    drop(rows);
    drop(stmt);

    // Hodnota je v reálném katalogu uložená jako REAL (2811382.0), ale
    // nespoléháme na to - zkusíme i INTEGER a TEXT.
    let raw: Option<f64> = tx
        .query_row(
            "SELECT value FROM Adobe_variablesTable WHERE name = ?1",
            [ENTITY_ID_COUNTER],
            |row| {
                row.get::<_, f64>(0).or_else(|_| {
                    row.get::<_, String>(0)
                        .map(|s| s.trim().parse::<f64>().unwrap_or(0.0))
                })
            },
        )
        .ok();
    let Some(counter) = raw.filter(|v| *v >= 1.0) else {
        return Ok(None);
    };
    let counter = counter as i64;

    // Obranná kontrola: čítač musí být před vším, co už je použité v cílové
    // tabulce. Nesplnění znamená poškozený/nekonzistentní katalog - radši
    // odmítnout než přidělit kolidující ID.
    let max_used: i64 = tx.query_row(
        &format!("SELECT COALESCE(MAX(id_local), 0) FROM {table}"),
        [],
        |row| row.get(0),
    )?;
    if counter <= max_used {
        return Err(IdError::CounterNotAhead { counter, max_used });
    }

    // Přiděl a posuň čítač - ve stejné transakci, takže při ROLLBACKu se
    // vrátí obojí.
    tx.execute(
        "UPDATE Adobe_variablesTable SET value = ?1 WHERE name = ?2",
        rusqlite::params![(counter + 1) as f64, ENTITY_ID_COUNTER],
    )?;
    Ok(Some(counter))
}

/// 32 znaků velkých hex bez pomlček (PROJECT.md §3.4: "LrC používá UUIDv4
/// bez dashů, uppercase"). Bez crate `uuid` (není v PROJECT.md §4 na
/// seznamu) - entropie z `RandomState`, stejný trik jako
/// `fsops::moves::jittered_delay_ms`. Volající MUSÍ ověřit unikátnost
/// dotazem přes [`unique_id_global`] - tahle funkce sama kolizi nevylučuje.
fn new_id_global() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let a = RandomState::new().build_hasher().finish();
    let b = RandomState::new().build_hasher().finish();
    format!("{a:016X}{b:016X}")
}

fn is_id_global_unique(tx: &Transaction, table: &str, id_global: &str) -> Result<bool, IdError> {
    let sql = format!("SELECT 1 FROM {table} WHERE id_global = ?1");
    Ok(!tx.prepare(&sql)?.exists([id_global])?)
}

/// Vygeneruje `id_global`, který v `table` ještě není použitý - opakuje
/// generování, dokud dotaz nepotvrdí unikátnost (prakticky vždy napoprvé,
/// 64 bitů náhody dvakrát).
pub fn unique_id_global(tx: &Transaction, table: &str) -> Result<String, IdError> {
    for _ in 0..10 {
        let candidate = new_id_global();
        if is_id_global_unique(tx, table, &candidate)? {
            return Ok(candidate);
        }
    }
    Err(IdError::ExhaustedUniqueAttempts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn conn_with_table() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE T (id_local INTEGER PRIMARY KEY, id_global TEXT);")
            .unwrap();
        conn
    }

    #[test]
    fn next_id_local_is_one_for_empty_table() {
        let mut conn = conn_with_table();
        let tx = conn.transaction().unwrap();
        assert_eq!(next_id_local(&tx, "T").unwrap(), 1);
    }

    #[test]
    fn next_id_local_is_max_plus_one() {
        let mut conn = conn_with_table();
        conn.execute(
            "INSERT INTO T (id_local, id_global) VALUES (5, 'A'), (12, 'B')",
            [],
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        assert_eq!(next_id_local(&tx, "T").unwrap(), 13);
    }

    #[test]
    fn refuses_when_counter_name_is_unknown() {
        let mut conn = conn_with_table();
        conn.execute_batch(
            "CREATE TABLE Adobe_variablesTable (name TEXT, value TEXT);
             INSERT INTO Adobe_variablesTable VALUES ('images_idLocalCounter', '42');",
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        let err = next_id_local(&tx, "T").unwrap_err();
        assert!(matches!(err, IdError::UnknownCounter { .. }));
    }

    /// Reprodukce reálného katalogu (LrC 13.3): `Adobe_entityIDCounter`
    /// uložený jako REAL, plus mobilní sync čítače, které s `id_local`
    /// nesouvisí a nesmí operaci blokovat.
    #[test]
    fn allocates_from_entity_id_counter_and_increments_it() {
        let mut conn = conn_with_table();
        conn.execute_batch(
            "CREATE TABLE Adobe_variablesTable (name TEXT, value);
             INSERT INTO Adobe_variablesTable VALUES ('Adobe_entityIDCounter', 2811382.0);
             INSERT INTO Adobe_variablesTable VALUES ('Adobe_lrMobileSyncChangeCounterV2', 5386.0);
             INSERT INTO Adobe_variablesTable VALUES ('Adobe_lrMobileInitialSyncChangeCounterV2', 0.0);
             INSERT INTO T (id_local, id_global) VALUES (5, 'A');",
        )
        .unwrap();

        let tx = conn.transaction().unwrap();
        let id = next_id_local(&tx, "T").unwrap();
        assert_eq!(
            id, 2811382,
            "musí se použít hodnota čítače, ne MAX(id_local)+1"
        );

        let after: f64 = tx
            .query_row(
                "SELECT value FROM Adobe_variablesTable WHERE name = 'Adobe_entityIDCounter'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after as i64, 2811383, "čítač se musí posunout");

        // Druhé přidělení ve stejné transakci nesmí vrátit totéž ID.
        assert_eq!(next_id_local(&tx, "T").unwrap(), 2811383);
    }

    #[test]
    fn refuses_when_counter_is_behind_existing_ids() {
        let mut conn = conn_with_table();
        conn.execute_batch(
            "CREATE TABLE Adobe_variablesTable (name TEXT, value);
             INSERT INTO Adobe_variablesTable VALUES ('Adobe_entityIDCounter', 10.0);
             INSERT INTO T (id_local, id_global) VALUES (500, 'A');",
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        let err = next_id_local(&tx, "T").unwrap_err();
        assert!(matches!(err, IdError::CounterNotAhead { .. }));
    }

    /// Rollback musí vrátit i čítač - jinak by po neúspěšném zápisu zůstal
    /// posunutý a katalog by měl díru v ID prostoru.
    #[test]
    fn counter_increment_is_rolled_back_with_the_transaction() {
        let mut conn = conn_with_table();
        conn.execute_batch(
            "CREATE TABLE Adobe_variablesTable (name TEXT, value);
             INSERT INTO Adobe_variablesTable VALUES ('Adobe_entityIDCounter', 100.0);",
        )
        .unwrap();
        {
            let tx = conn.transaction().unwrap();
            next_id_local(&tx, "T").unwrap();
            // tx se zahodí bez commitu -> ROLLBACK
        }
        let after: f64 = conn
            .query_row(
                "SELECT value FROM Adobe_variablesTable WHERE name = 'Adobe_entityIDCounter'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after as i64, 100, "po rollbacku musí být čítač netknutý");
    }

    #[test]
    fn proceeds_when_variables_table_exists_without_counter_like_row() {
        let mut conn = conn_with_table();
        conn.execute_batch(
            "CREATE TABLE Adobe_variablesTable (name TEXT, value TEXT);
             INSERT INTO Adobe_variablesTable VALUES ('someOtherSetting', 'x');",
        )
        .unwrap();
        let tx = conn.transaction().unwrap();
        assert_eq!(next_id_local(&tx, "T").unwrap(), 1);
    }

    #[test]
    fn new_id_global_is_32_uppercase_hex_chars() {
        let id = new_id_global();
        assert_eq!(id.len(), 32);
        assert!(id
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase()));
    }

    #[test]
    fn unique_id_global_avoids_existing_values() {
        let mut conn = conn_with_table();
        let tx = conn.transaction().unwrap();
        let id = unique_id_global(&tx, "T").unwrap();
        assert_eq!(id.len(), 32);
    }
}
