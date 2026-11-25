//! EXTRACT (PROJECT.md §3.1): vytáhne ze všech `AgLibraryFile` očekávané
//! absolutní cesty. Pracuje výhradně nad `AgLibraryFile`/`AgLibraryFolder`/
//! `AgLibraryRootFolder` - NIKDY nad `Adobe_images` (virtuální kopie by
//! duplikovaly fyzické soubory, viz PROJECT.md §3.2).
//!
//! Čistě read-only. Žádný `SELECT *` (CLAUDE.md invariant 4) - sloupce jsou
//! vyjmenované a odpovídají přesně tomu, co [`super::schema`] garduje.

use rusqlite::types::ValueRef;
use rusqlite::Connection;
use std::path::PathBuf;
use thiserror::Error;

use crate::fsops::paths::{expected_native_path, looks_like_windows_root};

/// `AgLibraryFile.externalModTime`, jak ho SQLite skutečně uložilo.
///
/// **OVĚŘENO na LrC 13.3** (reálný katalog, 2026-08-21): uloženo jako
/// `REAL`, hodnota je **Apple epocha** (2001-01-01), v sekundách. Vzorek:
/// min 189338440 → 2007-01-01, max 808263133 → 2026-08-12 (jako Unix epocha
/// by vyšly roky 1976 a 1995, což pro digitální fotky nedává smysl).
/// Runtime kalibrace ([`crate::matching::calibrate`]) tenhle formát na
/// stejném katalogu detekovala se 100% shodou.
///
/// Formát se ale mezi verzemi LrC liší, takže EXTRACT hodnotu pořád jen
/// věrně přenese a interpretaci nechává na kalibraci - ta si ho ověří proti
/// skutečným `mtime` existujících souborů, místo aby se spoléhala na tenhle
/// jeden ověřený případ.
#[derive(Debug, Clone, PartialEq)]
pub enum RawExternalModTime {
    Integer(i64),
    Real(f64),
    Text(String),
}

impl RawExternalModTime {
    /// Číselná hodnota bez interpretace jednotky/epochy - tu určuje až
    /// runtime kalibrace ([`crate::matching::calibrate`]).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            RawExternalModTime::Integer(i) => Some(*i as f64),
            RawExternalModTime::Real(r) => Some(*r),
            RawExternalModTime::Text(t) => t.trim().parse::<f64>().ok(),
        }
    }
}

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("chyba při čtení katalogu: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Jeden `AgLibraryRootFolder` řádek.
#[derive(Debug, Clone)]
pub struct RootFolder {
    pub root_id: i64,
    pub absolute_path: String,
    pub name: String,
}

/// Jeden fyzický soubor, jak ho eviduje katalog (jeden řádek `AgLibraryFile`).
#[derive(Debug, Clone)]
pub struct CatalogFile {
    pub file_id: i64,
    pub folder_id: i64,
    pub root_id: i64,
    /// `AgLibraryRootFolder.absolutePath`, přesně jak je v katalogu (dopředná lomítka).
    pub root_path: String,
    /// `AgLibraryFolder.pathFromRoot`, přesně jak je v katalogu.
    pub rel_path: String,
    /// `AgLibraryFile.idx_filename`.
    pub filename: String,
    pub base_name: String,
    pub extension: String,
    /// Nativní cesta (zpětná lomítka na Windows), sestavená z výše uvedeného
    /// přes [`expected_native_path`]. Tímhle se otevírá soubor / dělá se `read_dir`.
    pub expected_path: PathBuf,
    pub external_mod_time: Option<RawExternalModTime>,
    /// `AgLibraryFile.md5`.
    ///
    /// **OVĚŘENO na LrC 13.3** (reálný katalog, 2026-08-21): sloupec byl
    /// `NULL` u VŠECH 26 807 řádků. LrC ho tedy (aspoň pro tenhle způsob
    /// importu) nevyplňuje vůbec.
    ///
    /// **Důsledek pro skórování:** MD5 signál (+100, „jistota", §5.3) na
    /// takovém katalogu nikdy nenastane - `md5_signal` vrátí `Unavailable`.
    /// Reálně dosažitelné maximum skóre tím klesá na 63 bodů (25 sourozenci,
    /// 25 čas, 8 podobnost jména, 5 známá složka), což je POD prahem
    /// `auto_threshold = 70`. Auto-resolve se proto na takovém katalogu
    /// nikdy nespustí a všechno jde na ruční rozhodnutí.
    ///
    /// Chování je bezpečné (radši nic než špatně), ale práh z §5.3 byl
    /// zjevně navržen s předpokladem, že `md5` bývá vyplněné.
    ///
    /// **Náhradní signál je k dispozici:** viz `AgLibraryFile.importHash`
    /// (ověřený formát je popsaný u [`CatalogFile::file_size`]). Nese
    /// velikost souboru, kterou katalog má, i když `md5` chybí.
    pub md5: Option<String>,
    /// Velikost souboru v bajtech podle katalogu, vytažená z
    /// `AgLibraryFile.importHash`.
    ///
    /// **OVĚŘENO na LrC 13.3** (reálný katalog, 2026-08-21). `importHash` je
    /// TEXT ve tvaru tří polí oddělených dvojtečkou:
    ///
    /// ```text
    /// 189345639:S7301073.JPG:3299111
    /// ^ captureTime  ^ originalFilename  ^ velikost v bajtech
    /// (Apple epocha)
    /// ```
    ///
    /// Naměřeno na vzorku 5 000 řádků:
    /// - pole\[1\] == `Adobe_images.originalFilename` u 5000/5000
    ///   (proti `idx_filename` jen u 2735/5000 - po přejmenování v LrC se
    ///   liší, takže pole\[1\] je jméno PŘI IMPORTU, ne aktuální),
    /// - pole\[0\] interpretované jako Apple epocha padne do jednoho roku od
    ///   `Adobe_images.captureTime` u 5000/5000,
    /// - pole\[2\] == skutečná velikost na disku u 2953/2958 existujících
    ///   souborů (99,83 %).
    ///
    /// **Těch 5 odchylek není chyba měření**: šlo o `.dng`, `.png` a `.tif`,
    /// tedy formáty, do kterých LrC zapisuje metadata zpět do souboru. Rozdíl
    /// byl řádově kilobajty. Velikost proto smí být jen BONUS při shodě,
    /// nikdy veto při neshodě - stejné pravidlo jako u MD5 mismatche (§5.3).
    ///
    /// `None`, když `importHash` chybí, nemá tři pole, nebo se poslední pole
    /// nedá přečíst jako číslo. Nikdy se nehádá (CLAUDE.md §6).
    pub file_size: Option<u64>,
    /// `AgLibraryFile.sidecarExtensions` rozdělené na jednotlivé přípony,
    /// lowercase, bez teček - PROJECT.md §3.5 "zdroj pravdy" pro to, které
    /// doprovodné soubory (typicky `.xmp`) patří k tomuhle řádku a musí se
    /// přesouvat spolu s ním (`fsops::sidecars`). Prázdné, když katalog
    /// sloupec nemá vyplněný.
    pub sidecar_extensions: Vec<String>,
}

/// Vrátí všechny `AgLibraryRootFolder` řádky. Používá se pro §7.4 (sonda
/// svazků) i pro detekci cizího (macOS/POSIX) katalogu.
pub fn list_root_folders(conn: &Connection) -> Result<Vec<RootFolder>, ExtractError> {
    let mut stmt = conn.prepare("SELECT id_local, absolutePath, name FROM AgLibraryRootFolder")?;
    let rows = stmt.query_map([], |row| {
        Ok(RootFolder {
            root_id: row.get(0)?,
            absolute_path: row.get(1)?,
            name: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Jeden řádek "soubor je členem kolekce" - vstup pro `cluster::album_scattered`
/// (§7.3, ALBUM_SCATTERED). NEPOVINNÝ dotaz na rozdíl od [`extract`] - volej
/// jen po `catalog::schema::has_collection_schema`, protože schéma kolekcí
/// není na reálném katalogu ověřené (CLAUDE.md §6).
#[derive(Debug, Clone)]
pub struct CollectionMember {
    pub collection_id: i64,
    pub collection_name: String,
    pub file_id: i64,
}

/// §7.3 - přes `AgLibraryCollectionImage` → `Adobe_images.rootFile` na
/// `AgLibraryFile.id_local`. Virtuální kopie (§3.2) tu nevadí: zajímá nás
/// jen FYZICKÝ soubor, na který kolekce ukazuje, ne kolik "virtuálních"
/// políček v Lightroomu na něj odkazuje.
pub fn collection_membership(conn: &Connection) -> Result<Vec<CollectionMember>, ExtractError> {
    let mut stmt = conn.prepare(
        "SELECT ci.collection, c.name, ai.rootFile
         FROM AgLibraryCollectionImage ci
         JOIN AgLibraryCollection c ON ci.collection = c.id_local
         JOIN Adobe_images ai ON ci.image = ai.id_local",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(CollectionMember {
            collection_id: row.get(0)?,
            collection_name: row.get(1)?,
            file_id: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Root folders, jejichž `absolutePath` nevypadá jako windows cesta - katalog
/// pravděpodobně pochází z macOS (PROJECT.md §3.1). Volající musí tuto situaci
/// vyřešit (ruční mapování) DŘÍV, než spustí plný EXTRACT/VERIFY, jinak by
/// VERIFY nahlásila 100 % souborů jako chybějící.
#[cfg(windows)]
pub fn foreign_roots(roots: &[RootFolder]) -> Vec<&RootFolder> {
    roots
        .iter()
        .filter(|r| !looks_like_windows_root(&r.absolute_path))
        .collect()
}

/// Hlavní EXTRACT dotaz (PROJECT.md §3.1). ~30k řádků, cíl < 1 s.
pub fn extract(conn: &Connection) -> Result<Vec<CatalogFile>, ExtractError> {
    // `importHash` je NEPOVINNÝ sloupec (viz `schema::has_import_hash`).
    // Kdyby se vyjmenoval natvrdo, EXTRACT by na katalogu bez něj spadl -
    // proto se na jeho místo doplní literál NULL. Není to `SELECT *`:
    // sloupce zůstávají vyjmenované, jen jeden z nich je konstanta.
    let import_hash_col = if crate::catalog::schema::has_import_hash(conn) {
        "fi.importHash"
    } else {
        tracing::info!(
            "katalog nemá sloupec AgLibraryFile.importHash, signál velikosti bude vypnutý"
        );
        "NULL"
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT
            fi.id_local,
            fo.id_local,
            rf.id_local,
            rf.absolutePath,
            fo.pathFromRoot,
            fi.idx_filename,
            fi.baseName,
            fi.extension,
            fi.externalModTime,
            fi.md5,
            fi.sidecarExtensions,
            {import_hash_col}
         FROM AgLibraryFile fi
         JOIN AgLibraryFolder     fo ON fi.folder     = fo.id_local
         JOIN AgLibraryRootFolder rf ON fo.rootFolder = rf.id_local"
    ))?;

    let rows = stmt.query_map([], |row| {
        let root_path: String = row.get(3)?;
        let rel_path: String = row.get(4)?;
        let filename: String = row.get(5)?;
        let expected_path = expected_native_path(&root_path, &rel_path, &filename);
        let external_mod_time = match row.get_ref(8)? {
            ValueRef::Null => None,
            ValueRef::Integer(i) => Some(RawExternalModTime::Integer(i)),
            ValueRef::Real(r) => Some(RawExternalModTime::Real(r)),
            ValueRef::Text(t) => Some(RawExternalModTime::Text(
                String::from_utf8_lossy(t).into_owned(),
            )),
            // Blob nedává pro časové razítko smysl - katalog by tu měl
            // takovou hodnotu neočekávaně, radši ji ignorovat než hádat.
            ValueRef::Blob(_) => None,
        };
        let md5 = match row.get_ref(9)? {
            ValueRef::Null => None,
            ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
            ValueRef::Blob(b) => Some(b.iter().map(|byte| format!("{byte:02x}")).collect()),
            // Integer/Real by pro MD5 nedávaly smysl - katalog by tu měl
            // neočekávanou hodnotu, radši ji ignorovat než hádat formát.
            ValueRef::Integer(_) | ValueRef::Real(_) => None,
        };
        let sidecar_extensions = match row.get_ref(10)? {
            ValueRef::Text(t) => parse_sidecar_extensions(&String::from_utf8_lossy(t)),
            // NULL (nejčastější případ - žádné sidecary) i neočekávaný typ:
            // prázdný seznam, ne hádání.
            _ => Vec::new(),
        };
        let file_size = match row.get_ref(11)? {
            ValueRef::Text(t) => parse_import_hash_size(&String::from_utf8_lossy(t)),
            // NULL i neočekávaný typ: signál prostě není k dispozici.
            _ => None,
        };
        Ok(CatalogFile {
            file_id: row.get(0)?,
            folder_id: row.get(1)?,
            root_id: row.get(2)?,
            root_path,
            rel_path,
            filename,
            base_name: row.get(6)?,
            extension: row.get(7)?,
            expected_path,
            external_mod_time,
            md5,
            file_size,
            sidecar_extensions,
        })
    })?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// `AgLibraryFile.sidecarExtensions` je čárkou oddělený seznam (PROJECT.md
/// §3.5), typicky `xmp` nebo `xmp,jpg`. Normalizuje na lowercase bez teček -
/// samotné porovnání se sidecary na disku dělá `fsops::sidecars` přes
/// `match_key_str`, tady jde jen o rozparsování katalogové hodnoty.
fn parse_sidecar_extensions(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().trim_start_matches('.').to_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Vytáhne velikost souboru z `AgLibraryFile.importHash` - viz
/// [`CatalogFile::file_size`] pro ověřený formát.
///
/// Záměrně přísné: cokoli, co nemá přesně tři pole s číselným posledním
/// polem, vrátí `None`. Formát je ověřený jen na LrC 13.3, takže se u jiné
/// verze radši vzdáme signálu, než abychom si domýšleli jiný tvar
/// (CLAUDE.md §6). Jméno souboru uprostřed může obsahovat dvojtečku jen
/// stěží (na Windows je zakázaná), ale i tak se dělí zprava, aby případ
/// „dvojtečka ve jméně" ustřelil do `None` místo do špatného čísla.
fn parse_import_hash_size(raw: &str) -> Option<u64> {
    let (rest, size) = raw.rsplit_once(':')?;
    // `rest` musí obsahovat právě jednu dvojtečku - jinak nesedí počet polí.
    if rest.matches(':').count() != 1 {
        return None;
    }
    size.parse::<u64>().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_minimal() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER, id_global TEXT, absolutePath TEXT, name TEXT);
             CREATE TABLE AgLibraryFolder (id_local INTEGER, id_global TEXT, pathFromRoot TEXT, rootFolder INTEGER);
             CREATE TABLE AgLibraryFile (id_local INTEGER, id_global TEXT, folder INTEGER, baseName TEXT,
                                          extension TEXT, idx_filename TEXT, lc_idx_filename TEXT,
                                          externalModTime, md5 TEXT, sidecarExtensions TEXT,
                                          importHash TEXT);
             INSERT INTO AgLibraryRootFolder VALUES (1, 'A', 'C:/Foto/', 'Foto');
             INSERT INTO AgLibraryFolder VALUES (10, 'B', '2019/2019-07 HR/', 1);
             INSERT INTO AgLibraryFolder VALUES (11, 'C', '', 1);
             INSERT INTO AgLibraryFile VALUES (100, 'D', 10, 'IMG_0001', 'CR2', 'IMG_0001.CR2', 'img_0001.cr2', 1563091200.0, 'abc123', 'xmp', '189345639:IMG_0001.CR2:3299111');
             INSERT INTO AgLibraryFile VALUES (101, 'E', 11, 'IMG_9999', 'CR2', 'IMG_9999.CR2', 'img_9999.cr2', NULL, NULL, NULL, NULL);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn extract_builds_expected_native_paths() {
        let conn = open_minimal();
        let files = extract(&conn).unwrap();
        assert_eq!(files.len(), 2);
        let sub = files.iter().find(|f| f.file_id == 100).unwrap();
        #[cfg(windows)]
        assert_eq!(
            sub.expected_path,
            PathBuf::from(r"C:\Foto\2019\2019-07 HR\IMG_0001.CR2")
        );

        let root_level = files.iter().find(|f| f.file_id == 101).unwrap();
        #[cfg(windows)]
        assert_eq!(
            root_level.expected_path,
            PathBuf::from(r"C:\Foto\IMG_9999.CR2")
        );
    }

    #[test]
    fn extract_reads_external_mod_time_without_assuming_a_type() {
        let conn = open_minimal();
        let files = extract(&conn).unwrap();
        let with_time = files.iter().find(|f| f.file_id == 100).unwrap();
        assert_eq!(
            with_time.external_mod_time,
            Some(RawExternalModTime::Real(1563091200.0))
        );
        let without_time = files.iter().find(|f| f.file_id == 101).unwrap();
        assert_eq!(without_time.external_mod_time, None);
    }

    #[test]
    fn extract_reads_file_size_from_import_hash() {
        let conn = open_minimal();
        let files = extract(&conn).unwrap();
        let with_hash = files.iter().find(|f| f.file_id == 100).unwrap();
        assert_eq!(with_hash.file_size, Some(3_299_111));
        let without_hash = files.iter().find(|f| f.file_id == 101).unwrap();
        assert_eq!(without_hash.file_size, None);
    }

    /// Starší schéma sloupec `importHash` mít nemusí. EXTRACT na něm nesmí
    /// spadnout - signál se jen vypne.
    #[test]
    fn extract_works_on_catalog_without_import_hash_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER, id_global TEXT, absolutePath TEXT, name TEXT);
             CREATE TABLE AgLibraryFolder (id_local INTEGER, id_global TEXT, pathFromRoot TEXT, rootFolder INTEGER);
             CREATE TABLE AgLibraryFile (id_local INTEGER, id_global TEXT, folder INTEGER, baseName TEXT,
                                          extension TEXT, idx_filename TEXT, lc_idx_filename TEXT,
                                          externalModTime, md5 TEXT, sidecarExtensions TEXT);
             INSERT INTO AgLibraryRootFolder VALUES (1, 'A', 'C:/Foto/', 'Foto');
             INSERT INTO AgLibraryFolder VALUES (10, 'B', '2019/', 1);
             INSERT INTO AgLibraryFile VALUES (100, 'D', 10, 'IMG_0001', 'CR2', 'IMG_0001.CR2', 'img_0001.cr2', NULL, NULL, NULL);",
        )
        .unwrap();
        assert!(!crate::catalog::schema::has_import_hash(&conn));

        let files = extract(&conn).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].file_size, None);
    }

    #[test]
    fn parse_import_hash_size_reads_the_third_field() {
        assert_eq!(
            parse_import_hash_size("189345639:S7301073.JPG:3299111"),
            Some(3_299_111)
        );
    }

    #[test]
    fn parse_import_hash_size_refuses_unexpected_shapes() {
        // Jiný počet polí = neznámý formát, nehádáme (CLAUDE.md §6).
        assert_eq!(parse_import_hash_size("189345639:S7301073.JPG"), None);
        assert_eq!(parse_import_hash_size("a:b:c:1234"), None);
        assert_eq!(parse_import_hash_size("3299111"), None);
        assert_eq!(parse_import_hash_size(""), None);
        // Poslední pole není číslo.
        assert_eq!(parse_import_hash_size("189345639:S7301073.JPG:xyz"), None);
        // Záporná velikost nedává smysl a `u64` ji odmítne.
        assert_eq!(parse_import_hash_size("189345639:S7301073.JPG:-5"), None);
    }

    #[test]
    fn extract_reads_md5_as_plain_string() {
        let conn = open_minimal();
        let files = extract(&conn).unwrap();
        let with_md5 = files.iter().find(|f| f.file_id == 100).unwrap();
        assert_eq!(with_md5.md5.as_deref(), Some("abc123"));
        let without_md5 = files.iter().find(|f| f.file_id == 101).unwrap();
        assert_eq!(without_md5.md5, None);
    }

    #[test]
    fn extract_reads_sidecar_extensions() {
        let conn = open_minimal();
        let files = extract(&conn).unwrap();
        let with_sidecar = files.iter().find(|f| f.file_id == 100).unwrap();
        assert_eq!(with_sidecar.sidecar_extensions, vec!["xmp".to_string()]);
        let without_sidecar = files.iter().find(|f| f.file_id == 101).unwrap();
        assert!(without_sidecar.sidecar_extensions.is_empty());
    }

    #[test]
    fn collection_membership_joins_through_adobe_images() {
        let conn = open_minimal();
        conn.execute_batch(
            "CREATE TABLE AgLibraryCollection (id_local INTEGER, name TEXT);
             CREATE TABLE AgLibraryCollectionImage (collection INTEGER, image INTEGER);
             CREATE TABLE Adobe_images (id_local INTEGER, rootFile INTEGER);
             INSERT INTO AgLibraryCollection VALUES (1, 'Nejlepsi 2020');
             INSERT INTO Adobe_images VALUES (500, 100);
             INSERT INTO Adobe_images VALUES (501, 101);
             INSERT INTO AgLibraryCollectionImage VALUES (1, 500);
             INSERT INTO AgLibraryCollectionImage VALUES (1, 501);",
        )
        .unwrap();

        let members = collection_membership(&conn).unwrap();
        assert_eq!(members.len(), 2);
        assert!(members.iter().all(|m| m.collection_id == 1));
        assert!(members.iter().all(|m| m.collection_name == "Nejlepsi 2020"));
        let file_ids: Vec<i64> = members.iter().map(|m| m.file_id).collect();
        assert!(file_ids.contains(&100));
        assert!(file_ids.contains(&101));
    }

    #[test]
    fn parse_sidecar_extensions_normalizes_case_and_dots() {
        assert_eq!(
            parse_sidecar_extensions("XMP, .jpg ,wav"),
            vec!["xmp".to_string(), "jpg".to_string(), "wav".to_string()]
        );
        assert!(parse_sidecar_extensions("").is_empty());
        assert!(parse_sidecar_extensions("  ").is_empty());
    }

    #[test]
    fn list_root_folders_returns_all_roots() {
        let conn = open_minimal();
        let roots = list_root_folders(&conn).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].absolute_path, "C:/Foto/");
    }

    #[cfg(windows)]
    #[test]
    fn foreign_roots_detects_macos_style_path() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE AgLibraryRootFolder (id_local INTEGER, id_global TEXT, absolutePath TEXT, name TEXT);
             INSERT INTO AgLibraryRootFolder VALUES (1, 'A', '/Users/pavel/Pictures/', 'Pictures');",
        )
        .unwrap();
        let roots = list_root_folders(&conn).unwrap();
        let foreign = foreign_roots(&roots);
        assert_eq!(foreign.len(), 1);
    }
}
