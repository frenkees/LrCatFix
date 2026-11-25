//! Generátor testovacího katalogu + odpovídajícího stromu souborů
//! (CLAUDE.md §7). Používají ho integrační testy z `tests/`.
//!
//! Sdílený mutable fixture je zdroj flaky testů (CLAUDE.md §7) - proto
//! `build()` pokaždé vytvoří kompletně novou sadu souborů i nový `.lrcat`
//! v adresáři, který dostane od volajícího (ten by měl být čerstvý
//! `tempfile::TempDir`).

use std::path::{Path, PathBuf};

use rusqlite::Connection;

/// Test-only fallback nad `fsops::paths::to_verbatim` (produkční funkce, jediné
/// místo, které skládá `\\?\` prefix - CLAUDE.md „Cesty"). Tady jen ošetřujeme,
/// že mimo Windows nebo pro nekonvertovatelnou cestu použijeme cestu beze změny.
fn to_verbatim_or_self(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        lrcatfix::fsops::paths::to_verbatim(path).unwrap_or_else(|| path.to_path_buf())
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// Id_local hodnoty jednotlivých testovacích souborů/složek - pro asserce v testech.
///
/// `allow(dead_code)`: tenhle modul se přes `#[path]` vkládá do VÍCE test
/// binárek (`m0_pipeline`, `full_pipeline`) a každá používá jinou podmnožinu
/// polí. Bez toho by každá hlásila jako mrtvá ta pole, která zrovna nečte.
#[allow(dead_code)]
pub struct Fixture {
    pub catalog_path: PathBuf,
    pub root_dir: PathBuf,

    /// Existuje na disku přesně tam, kde ho katalog čeká.
    pub ok_file_id: i64,
    /// Katalog ho čeká ve složce, která existuje, ale soubor v ní chybí.
    pub missing_file_id: i64,
    /// Celá rodičovská složka na disku neexistuje.
    pub folder_missing_file_id: i64,
    /// Jméno v katalogu je NFC, fyzický soubor na disku je uložen NFD
    /// (simulace macOS/HFS+ zdroje, PROJECT.md §16.3) - musí se spárovat.
    pub nfd_disk_file_id: i64,
    /// RAW + JPEG jako dva samostatné `AgLibraryFile` řádky (PROJECT.md §3.5).
    pub raw_jpeg_raw_id: i64,
    pub raw_jpeg_jpeg_id: i64,
    /// RAW se sidecary `.xmp` + `.wav` (jeden `AgLibraryFile` řádek, PROJECT.md §3.5).
    pub sidecar_raw_id: i64,
    /// Soubor s celkovou délkou cesty > 260 znaků (PROJECT.md §16.1).
    pub long_path_file_id: i64,
    /// Root pro test macOS-style katalogu (§3.1) - cesta začíná `/Users/...`.
    pub foreign_root_id: i64,
    /// Prázdná složka, existuje na disku, žádný soubor na ni v katalogu neukazuje.
    pub empty_folder_path: PathBuf,

    /// Vzor A (§7.1): zdrojová složka, která na disku neexistuje, ale všechny
    /// její soubory leží pohromadě v [`Fixture::folder_moved_target`].
    pub folder_moved_folder_id: i64,
    pub folder_moved_file_ids: Vec<i64>,
    pub folder_moved_target: PathBuf,

    /// Vzor B (§7.2): zdrojová složka, jejíchž 6 souborů leží každý v jiné
    /// cílové složce.
    pub scattered_folder_id: i64,
    pub scattered_file_ids: Vec<i64>,

    /// Virtuální kopie (§3.2, CLAUDE.md §7) - řádek v `Adobe_images`
    /// s `copyName IS NOT NULL`, který ukazuje na `ok_file_id`. EXTRACT ho
    /// nesmí započítat jako další fyzický soubor.
    pub virtual_copy_image_id: i64,

    /// Kolekce pro vzor C (§7.3).
    pub collection_id: i64,
}

pub fn build(dir: &Path) -> Fixture {
    let root_dir = dir.join("Foto");
    std::fs::create_dir_all(&root_dir).unwrap();
    let root_path_catalog = format!("{}/", to_forward_slash(&root_dir));

    let catalog_path = dir.join("Test.lrcat");
    let conn = Connection::open(&catalog_path).unwrap();
    create_schema(&conn);

    let root_id = 1i64;
    insert_root_folder(&conn, root_id, &root_path_catalog, "Foto");

    // Cizí (macOS) root - jen řádek v katalogu, žádný odpovídající strom na
    // disku (PROJECT.md §3.1: detekce musí proběhnout DŘÍV, než se cokoli
    // pokusí ověřovat na disku).
    let foreign_root_id = 2i64;
    // Jméno je záměrně "nemožné", aby se cesta náhodou neshodovala se
    // skutečnou složkou na stroji, kde test běží (drive-relative fallback
    // v `exists_verbatim` by jinak mohl kolidovat s reálným uživatelským
    // profilem).
    insert_root_folder(
        &conn,
        foreign_root_id,
        "/Users/lrcatfix_test_user_that_never_exists/Pictures/",
        "Pictures",
    );

    let mut next_folder_id = 10i64;
    let mut next_file_id = 100i64;

    // --- OK soubor ve složce "2019/2019-07 Chorvatsko/" ---
    let folder_hr = next_folder_id;
    next_folder_id += 1;
    insert_folder(&conn, folder_hr, "2019/2019-07 Chorvatsko/", root_id);
    let hr_dir = root_dir.join("2019").join("2019-07 Chorvatsko");
    std::fs::create_dir_all(&hr_dir).unwrap();

    let ok_file_id = next_file_id;
    next_file_id += 1;
    write_file(&hr_dir.join("IMG_0001.CR2"), b"raw-data");
    insert_file(
        &conn,
        ok_file_id,
        folder_hr,
        "IMG_0001",
        "CR2",
        "IMG_0001.CR2",
        None,
    );

    // --- chybí jen soubor, složka existuje ---
    let missing_file_id = next_file_id;
    next_file_id += 1;
    insert_file(
        &conn,
        missing_file_id,
        folder_hr,
        "IMG_0002",
        "CR2",
        "IMG_0002.CR2",
        None,
    );

    // --- diakritika: katalog má NFC, disk má NFD ---
    let nfc_name = "Plitvička.CR2";
    let nfd_name = to_nfd(nfc_name);
    let nfd_disk_file_id = next_file_id;
    next_file_id += 1;
    write_file(&hr_dir.join(&nfd_name), b"raw-data-nfd");
    insert_file(
        &conn,
        nfd_disk_file_id,
        folder_hr,
        "Plitvička",
        "CR2",
        nfc_name,
        None,
    );

    // --- celá složka chybí na disku ---
    let folder_missing = next_folder_id;
    next_folder_id += 1;
    insert_folder(&conn, folder_missing, "2020/2020-03 Ztraceno/", root_id);
    let folder_missing_file_id = next_file_id;
    next_file_id += 1;
    insert_file(
        &conn,
        folder_missing_file_id,
        folder_missing,
        "IMG_0500",
        "CR2",
        "IMG_0500.CR2",
        None,
    );
    // Záměrně se pro `folder_missing` NEvytváří adresář na disku.

    // --- RAW + JPEG jako dva nezávislé AgLibraryFile řádky ---
    let folder_pair = next_folder_id;
    next_folder_id += 1;
    insert_folder(&conn, folder_pair, "2021/Pár/", root_id);
    let pair_dir = root_dir.join("2021").join("Pár");
    std::fs::create_dir_all(&pair_dir).unwrap();
    let raw_jpeg_raw_id = next_file_id;
    next_file_id += 1;
    let raw_jpeg_jpeg_id = next_file_id;
    next_file_id += 1;
    write_file(&pair_dir.join("IMG_2000.CR2"), b"raw");
    write_file(&pair_dir.join("IMG_2000.JPG"), b"jpeg");
    insert_file(
        &conn,
        raw_jpeg_raw_id,
        folder_pair,
        "IMG_2000",
        "CR2",
        "IMG_2000.CR2",
        None,
    );
    insert_file(
        &conn,
        raw_jpeg_jpeg_id,
        folder_pair,
        "IMG_2000",
        "JPG",
        "IMG_2000.JPG",
        None,
    );

    // --- RAW + sidecary .xmp + .wav (jeden AgLibraryFile řádek) ---
    let folder_sidecar = next_folder_id;
    next_folder_id += 1;
    insert_folder(&conn, folder_sidecar, "2021/Poznamky/", root_id);
    let sidecar_dir = root_dir.join("2021").join("Poznamky");
    std::fs::create_dir_all(&sidecar_dir).unwrap();
    let sidecar_raw_id = next_file_id;
    next_file_id += 1;
    write_file(&sidecar_dir.join("IMG_3000.CR2"), b"raw");
    write_file(&sidecar_dir.join("IMG_3000.xmp"), b"<xmp/>");
    write_file(&sidecar_dir.join("IMG_3000.wav"), b"RIFF");
    insert_file(
        &conn,
        sidecar_raw_id,
        folder_sidecar,
        "IMG_3000",
        "CR2",
        "IMG_3000.CR2",
        Some("xmp,wav"),
    );

    // --- cesta > 260 znaků ---
    let folder_long = next_folder_id;
    next_folder_id += 1;
    let mut long_rel = String::new();
    let mut long_dir = root_dir.clone();
    while (root_path_catalog.len() + long_rel.len()) < 270 {
        let segment = "velmi_dlouhy_nazev_slozky_pouzity_jen_pro_test_dlouhych_cest";
        long_rel.push_str(segment);
        long_rel.push('/');
        long_dir = long_dir.join(segment);
    }
    create_dir_all_verbatim(&long_dir);
    insert_folder(&conn, folder_long, &long_rel, root_id);
    let long_path_file_id = next_file_id;
    next_file_id += 1;
    write_file_verbatim(&long_dir.join("IMG_9000.CR2"), b"raw");
    insert_file(
        &conn,
        long_path_file_id,
        folder_long,
        "IMG_9000",
        "CR2",
        "IMG_9000.CR2",
        None,
    );
    assert!(
        (root_path_catalog.len() + long_rel.len() + "IMG_9000.CR2".len()) > 260,
        "fixture cesta musí přesáhnout 260 znaků, aby test §16.1 měl smysl"
    );

    // --- prázdná složka ---
    let empty_folder_path = root_dir.join("Prazdna");
    std::fs::create_dir_all(&empty_folder_path).unwrap();
    let folder_empty = next_folder_id;
    next_folder_id += 1;
    insert_folder(&conn, folder_empty, "Prazdna/", root_id);

    // --- VZOR A (§7.1): celá složka se "přestěhovala" do zálohy ---
    // Katalog čeká 4 soubory v `2018/2018-05 Vylet/`, ta složka na disku
    // NEEXISTUJE, ale všechny 4 leží pohromadě v `Zaloha/Vylet2018/` (mimo
    // katalogový root, aby je nenašel VERIFY, ale našel je INDEX).
    let folder_moved_src = next_folder_id;
    next_folder_id += 1;
    insert_folder(&conn, folder_moved_src, "2018/2018-05 Vylet/", root_id);
    // POD katalogovým rootem - přesun složky v rámci vlastního fotodisku je
    // reálný scénář, kdežto cíl mimo všechny rooty by `plan_folder_repoint`
    // (správně) odmítl s `NoMatchingRoot`: založení nového rootu je vědomě
    // mimo rozsah (viz `catalog::apply` doc).
    let folder_moved_target = root_dir.join("Zaloha").join("Vylet2018");
    std::fs::create_dir_all(&folder_moved_target).unwrap();
    let mut folder_moved_file_ids = Vec::new();
    for i in 1..=4 {
        let id = next_file_id;
        next_file_id += 1;
        let name = format!("IMG_4{i:03}.CR2");
        write_file(&folder_moved_target.join(&name), b"raw-presunuty");
        insert_file(
            &conn,
            id,
            folder_moved_src,
            &name.replace(".CR2", ""),
            "CR2",
            &name,
            None,
        );
        folder_moved_file_ids.push(id);
    }

    // --- VZOR B (§7.2): rozsypaná složka ---
    // 6 souborů z jedné zdrojové složky, každý v JINÉM cíli (k=6 >= 5,
    // medián 1 <= 2). Zdrojová složka na disku existuje, ale je prázdná.
    let scattered_src = next_folder_id;
    next_folder_id += 1;
    insert_folder(&conn, scattered_src, "2022/Export/", root_id);
    std::fs::create_dir_all(root_dir.join("2022").join("Export")).unwrap();
    let mut scattered_file_ids = Vec::new();
    for i in 1..=6 {
        let id = next_file_id;
        next_file_id += 1;
        let name = format!("IMG_5{i:03}.JPG");
        let target = root_dir.join("Rozsyp").join(format!("misto{i}"));
        std::fs::create_dir_all(&target).unwrap();
        write_file(&target.join(&name), b"jpeg-rozsypany");
        insert_file(
            &conn,
            id,
            scattered_src,
            &name.replace(".JPG", ""),
            "JPG",
            &name,
            None,
        );
        scattered_file_ids.push(id);
    }

    // --- md5 na existujícím souboru (§5.3 MD5 signál) ---
    // Skutečné MD5 obsahu `IMG_0001.CR2` ("raw-data"), aby `md5_signal`
    // vrátilo `Match`, ne jen `Unavailable`.
    set_md5(&conn, ok_file_id, &md5_hex_of(b"raw-data"));

    // --- Adobe_images vč. VIRTUÁLNÍ KOPIE (CLAUDE.md §7, PROJECT.md §3.2) ---
    // `ok_file_id` má normální obrázek + virtuální kopii. EXTRACT čte jen
    // `AgLibraryFile`, takže virtuální kopie NESMÍ zdvojit počet souborů.
    let image_ok = 900i64;
    let image_virtual_copy = 901i64;
    insert_image(&conn, image_ok, ok_file_id, None);
    insert_image(
        &conn,
        image_virtual_copy,
        ok_file_id,
        Some((image_ok, "Copy 1")),
    );

    // --- VZOR C (§7.3): album rozházené po mnoha složkách ---
    // POZOR: `album_scattered::detect` počítá `distinct_folders` podle
    // KATALOGOVÝCH `folder_id`, ne podle fyzických cílů - kolekce proto musí
    // sahat do >= 10 různých `AgLibraryFolder`, ne do 10 míst na disku.
    // 10 složek po jednom souboru, 4 z nich na disku vůbec nevzniknou
    // (missing_ratio 40 % >= 30 %).
    let collection_id = 1i64;
    insert_collection(&conn, collection_id, "Nejlepší 2023");
    for i in 1..=10i64 {
        // Odvozené z `i`, ne inkrementované čítače - clippy jinak (právem)
        // hlásí `explicit_counter_loop`.
        let image_id = 909 + i;
        let collection_image_id = i;
        let folder_id = next_folder_id;
        next_folder_id += 1;
        let rel = format!("2023/Album{i:02}/");
        insert_folder(&conn, folder_id, &rel, root_id);

        let file_id = next_file_id;
        next_file_id += 1;
        let name = format!("IMG_6{i:03}.CR2");
        // Prvních 6 na disku existuje, poslední 4 ne.
        if i <= 6 {
            let d = root_dir.join("2023").join(format!("Album{i:02}"));
            std::fs::create_dir_all(&d).unwrap();
            write_file(&d.join(&name), b"raw-album");
        }
        insert_file(
            &conn,
            file_id,
            folder_id,
            &name.replace(".CR2", ""),
            "CR2",
            &name,
            None,
        );

        insert_image(&conn, image_id, file_id, None);
        insert_collection_image(&conn, collection_image_id, collection_id, image_id);
    }

    Fixture {
        catalog_path,
        root_dir,
        ok_file_id,
        missing_file_id,
        folder_missing_file_id,
        nfd_disk_file_id,
        raw_jpeg_raw_id,
        raw_jpeg_jpeg_id,
        sidecar_raw_id,
        long_path_file_id,
        foreign_root_id,
        empty_folder_path,
        folder_moved_folder_id: folder_moved_src,
        folder_moved_file_ids,
        folder_moved_target,
        scattered_folder_id: scattered_src,
        scattered_file_ids,
        virtual_copy_image_id: image_virtual_copy,
        collection_id,
    }
}

fn md5_hex_of(data: &[u8]) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn create_schema(conn: &Connection) {
    conn.execute_batch(
        "CREATE TABLE AgLibraryRootFolder (
            id_local INTEGER PRIMARY KEY,
            id_global TEXT,
            absolutePath TEXT,
            name TEXT,
            relativePathFromCatalog TEXT
         );
         CREATE TABLE AgLibraryFolder (
            id_local INTEGER PRIMARY KEY,
            id_global TEXT,
            pathFromRoot TEXT,
            rootFolder INTEGER,
            visibility INTEGER
         );
         CREATE TABLE AgLibraryFile (
            id_local INTEGER PRIMARY KEY,
            id_global TEXT,
            folder INTEGER,
            baseName TEXT,
            extension TEXT,
            idx_filename TEXT,
            lc_idx_filename TEXT,
            originalFilename TEXT,
            importHash TEXT,
            md5 TEXT,
            modTime REAL,
            externalModTime TEXT,
            fileFormat TEXT,
            sidecarExtensions TEXT,
            errorMessage TEXT
         );
         CREATE TABLE Adobe_images (
            id_local INTEGER PRIMARY KEY,
            id_global TEXT,
            rootFile INTEGER,
            captureTime TEXT,
            fileFormat TEXT,
            masterImage INTEGER,
            copyName TEXT
         );
         CREATE TABLE AgLibraryCollection (
            id_local INTEGER PRIMARY KEY,
            id_global TEXT,
            name TEXT,
            creationId TEXT
         );
         CREATE TABLE AgLibraryCollectionImage (
            id_local INTEGER PRIMARY KEY,
            collection INTEGER,
            image INTEGER
         );",
    )
    .unwrap();
}

/// Jeden řádek `Adobe_images`. `master_and_copy_name` = `Some((master_id,
/// "Copy 1"))` udělá VIRTUÁLNÍ KOPII (PROJECT.md §3.2, CLAUDE.md §7) -
/// EXTRACT ji nesmí započítat jako další fyzický soubor.
fn insert_image(
    conn: &Connection,
    id: i64,
    root_file: i64,
    master_and_copy_name: Option<(i64, &str)>,
) {
    let (master, copy_name) = match master_and_copy_name {
        Some((m, c)) => (Some(m), Some(c)),
        None => (None, None),
    };
    conn.execute(
        "INSERT INTO Adobe_images (id_local, id_global, rootFile, masterImage, copyName)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![id, format!("IMAGE{id}"), root_file, master, copy_name],
    )
    .unwrap();
}

fn insert_collection(conn: &Connection, id: i64, name: &str) {
    conn.execute(
        "INSERT INTO AgLibraryCollection (id_local, id_global, name) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, format!("COLL{id}"), name],
    )
    .unwrap();
}

fn insert_collection_image(conn: &Connection, id: i64, collection: i64, image: i64) {
    conn.execute(
        "INSERT INTO AgLibraryCollectionImage (id_local, collection, image) VALUES (?1, ?2, ?3)",
        rusqlite::params![id, collection, image],
    )
    .unwrap();
}

fn set_md5(conn: &Connection, file_id: i64, md5_hex: &str) {
    conn.execute(
        "UPDATE AgLibraryFile SET md5 = ?1 WHERE id_local = ?2",
        rusqlite::params![md5_hex, file_id],
    )
    .unwrap();
}

fn insert_root_folder(conn: &Connection, id: i64, absolute_path: &str, name: &str) {
    conn.execute(
        "INSERT INTO AgLibraryRootFolder (id_local, id_global, absolutePath, name) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![id, format!("ROOT{id}"), absolute_path, name],
    )
    .unwrap();
}

fn insert_folder(conn: &Connection, id: i64, path_from_root: &str, root_folder: i64) {
    conn.execute(
        "INSERT INTO AgLibraryFolder (id_local, id_global, pathFromRoot, rootFolder, visibility) \
         VALUES (?1, ?2, ?3, ?4, 1)",
        rusqlite::params![id, format!("FOLDER{id}"), path_from_root, root_folder],
    )
    .unwrap();
}

#[allow(clippy::too_many_arguments)]
fn insert_file(
    conn: &Connection,
    id: i64,
    folder: i64,
    base_name: &str,
    extension: &str,
    idx_filename: &str,
    sidecar_extensions: Option<&str>,
) {
    conn.execute(
        "INSERT INTO AgLibraryFile
            (id_local, id_global, folder, baseName, extension, idx_filename, lc_idx_filename,
             originalFilename, sidecarExtensions)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?8)",
        rusqlite::params![
            id,
            format!("FILE{id}"),
            folder,
            base_name,
            extension,
            idx_filename,
            idx_filename.to_lowercase(),
            sidecar_extensions,
        ],
    )
    .unwrap();
}

fn to_forward_slash(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn to_nfd(s: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    s.nfd().collect()
}

fn write_file(path: &Path, contents: &[u8]) {
    std::fs::write(path, contents).unwrap();
}

fn write_file_verbatim(path: &Path, contents: &[u8]) {
    std::fs::write(to_verbatim_or_self(path), contents).unwrap();
}

fn create_dir_all_verbatim(path: &Path) {
    // `create_dir_all` na verbatim cestě vyžaduje, aby existovali i rodiče -
    // stavíme postupně od kořene tempdiru, který je krátký a jistě existuje.
    let mut acc = PathBuf::new();
    for component in path.components() {
        acc.push(component);
        let verbatim = to_verbatim_or_self(&acc);
        if !verbatim.exists() {
            let _ = std::fs::create_dir(&verbatim);
        }
    }
}
