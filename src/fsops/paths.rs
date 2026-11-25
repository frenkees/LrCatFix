//! Cesty: verbatim `\\?\` konverze pro WinAPI, normalizovaný klíč pro porovnání.
//!
//! Pravidlo (CLAUDE.md §3 bod 7, PROJECT.md §16.3): normalizovaný/lowercase
//! řetězec se používá VÝHRADNĚ pro porovnání, nikdy k otevření souboru.
//! Ke I/O se vždy použije bajtově přesná `PathBuf`, kterou vrátil filesystém
//! nebo katalog.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;

/// Normalizovaný klíč pro porovnávání jmen/cest. NIKDY nepoužívat k I/O.
///
/// PROJECT.md §16.3: NFC normalizace (macOS ukládá NFD) + lowercase
/// (Windows porovnává jména case-insensitive).
/// POŘADÍ JE ZÁMĚRNÉ: nejdřív `to_lowercase()`, teprve pak NFC. Opačně
/// (NFC → lowercase) by výstup nemusel být NFC, protože `to_lowercase()` umí
/// normalizaci rozbít - `İ` (U+0130) se lowercasuje na `i` + U+0307
/// (dekomponovaně). Pro češtinu na tom nezáleží (`Č` → `č` zůstává NFC), ale
/// invariant „klíč je vždy NFC" má platit bez výjimek.
pub fn match_key(name: &std::ffi::OsStr) -> String {
    match_key_str(&name.to_string_lossy())
}

/// Sestaví match_key z libovolného řetězce (relativní podcesta apod.) -
/// používá se při porovnávání relativních podcest v CLUSTER, §7.4.3.
/// Viz [`match_key`] pro zdůvodnění pořadí lowercase → NFC.
pub fn match_key_str(s: &str) -> String {
    s.to_lowercase().nfc().collect()
}

/// Jediné místo v kódu, které skládá cestu do WinAPI verbatim tvaru (PROJECT.md §16.1).
///
/// Vstup musí být absolutní a už normalizovaný (žádné `.` / `..`, žádná dvojitá
/// lomítka) — verbatim prefix vypíná normalizaci v OS. Volající je zodpovědný
/// za to, že `path` pochází buď z `std::fs` (canonicalizovaná), nebo byla
/// sestavena přes [`expected_native_path`].
///
/// Vrací `None`, pokud cesta není absolutní lokální (`C:\...`) ani UNC
/// (`\\server\share\...`) — takovou cestu nelze do verbatim tvaru převést
/// bezpečně a volající to musí ošetřit jako chybu, ne hádat.
///
/// Pracuje nad syrovými WTF-16 jednotkami (`OsStrExt::encode_wide`), ne nad
/// `&str` (§16.3). NTFS neověřuje párování surrogátů, takže nepárový
/// surrogát je legální jméno souboru, které nejde vyjádřit v UTF-8 -
/// dřívější implementace přes `path.to_str()?` na něm vracela `None` a cesta
/// tiše přišla o verbatim prefix, a tím o podporu délky > 260 znaků.
///
/// Celý řetězec MFT → `NameIndex` → sem je proto WTF-8/WTF-16 čistý:
/// `index::mft` skládá jména přes `OsString::from_wide` (ne
/// `String::from_utf16_lossy`) a `NameIndex` je drží v `PathBuf`.
///
/// **Zbývající omezení:** `index::cache` serializuje `PathBuf` přes serde,
/// které na non-UTF-8 cestě chybuje (`serde_core`: „path contains invalid
/// UTF-8 characters") - jediný takový soubor na disku tedy shodí uložení
/// CELÉ cache indexu. Chyba je zachycená a zalogovaná (index se pak jen
/// staví znovu při každém spuštění), takže jde o výkonnostní, ne
/// korektnostní problém. Oprava = vlastní serde reprezentace cest
/// (`Vec<u16>`), včetně změny formátu cache.
#[cfg(windows)]
pub fn to_verbatim(path: &Path) -> Option<PathBuf> {
    use std::ffi::OsString;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    const SLASH: u16 = b'/' as u16;
    const BACKSLASH: u16 = b'\\' as u16;
    const COLON: u16 = b':' as u16;
    const QUESTION: u16 = b'?' as u16;

    fn is_ascii_alpha(c: u16) -> bool {
        (b'A' as u16..=b'Z' as u16).contains(&c) || (b'a' as u16..=b'z' as u16).contains(&c)
    }
    fn is_sep(c: u16) -> bool {
        c == SLASH || c == BACKSLASH
    }
    fn wide(prefix: &str, rest: &[u16]) -> PathBuf {
        let mut out: Vec<u16> = prefix.encode_utf16().collect();
        // Verbatim prefix VYPÍNÁ normalizaci v OS, takže dopředná lomítka se
        // za ním už nepřeloží - musí se převést TEĎ, jinak WinAPI selže.
        out.extend(rest.iter().map(|&c| if c == SLASH { BACKSLASH } else { c }));
        PathBuf::from(OsString::from_wide(&out))
    }

    // Pracujeme nad WTF-16 jednotkami, ne nad `&str`: názvy z NTFS nemusí
    // být platné UTF-8 (nepárový surrogát je legální jméno souboru) a
    // `to_str()` by u nich vrátil `None` - cesta by tiše přišla o verbatim
    // prefix, a tím o podporu délky > 260 znaků (§16.1, §16.3).
    let w: Vec<u16> = path.as_os_str().encode_wide().collect();

    // Cesta už verbatim je - nech beze změny.
    if w.len() >= 4
        && w[0] == BACKSLASH
        && w[1] == BACKSLASH
        && w[2] == QUESTION
        && w[3] == BACKSLASH
    {
        return Some(path.to_path_buf());
    }

    // UNC: \\server\share\... -> \\?\UNC\server\share\...
    // Přijímá i tvar s dopřednými lomítky (`//server/share/`), protože přesně
    // takhle UNC kořeny ukládá katalog (`AgLibraryRootFolder.absolutePath`) a
    // v tomhle tvaru se sem dostane cesta ke katalogu z CLI/GUI.
    if w.len() > 2 && ((w[0] == BACKSLASH && w[1] == BACKSLASH) || (w[0] == SLASH && w[1] == SLASH))
    {
        return Some(wide(r"\\?\UNC\", &w[2..]));
    }

    // Lokální absolutní cesta musí začínat "X:\".
    if w.len() >= 3 && is_ascii_alpha(w[0]) && w[1] == COLON && is_sep(w[2]) {
        return Some(wide(r"\\?\", &w));
    }

    None
}

/// Sestaví cestu, kterou katalog očekává, z `AgLibraryRootFolder.absolutePath`,
/// `AgLibraryFolder.pathFromRoot` a `AgLibraryFile.idx_filename` (PROJECT.md §3.1).
///
/// `root_path` a `rel_path` jsou ve tvaru s dopřednými lomítky a koncovým
/// lomítkem (nebo `rel_path` prázdný pro kořen). Výsledek je nativní `PathBuf`
/// s `\` na Windows. Nedělá se zde žádná normalizace za účelem porovnání -
/// pro to použij [`match_key`] na výsledné jméno/cestu.
pub fn expected_native_path(root_path: &str, rel_path: &str, filename: &str) -> PathBuf {
    let mut combined = String::with_capacity(root_path.len() + rel_path.len() + filename.len());
    combined.push_str(root_path);
    if !rel_path.is_empty() {
        combined.push_str(rel_path);
    }
    combined.push_str(filename);

    #[cfg(windows)]
    {
        PathBuf::from(combined.replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from(combined)
    }
}

/// Cesta vhodná pro přímé volání do `std::fs` - na Windows přes
/// [`to_verbatim`], pokud lze (viz tamní omezení - absolutní, už
/// normalizovaná cesta), jinak `path` beze změny. Sankcionované místo, kudy
/// `fsops::verify` a `fsops::moves` prohání cesty před I/O kvůli §16.1
/// (cesty > 260 znaků) - nespoléhá na to, že je v registru zapnuté
/// `LongPathsEnabled`.
pub fn to_verbatim_for_io(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        to_verbatim(path).unwrap_or_else(|| path.to_path_buf())
    }
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
}

/// Ověří existenci `path` - viz [`to_verbatim_for_io`].
pub fn exists_verbatim(path: &Path) -> bool {
    to_verbatim_for_io(path).exists()
}

/// `read_dir` - viz [`to_verbatim_for_io`].
pub fn read_dir_verbatim(path: &Path) -> std::io::Result<std::fs::ReadDir> {
    std::fs::read_dir(to_verbatim_for_io(path))
}

/// Detekuje, jestli `absolutePath` z katalogu vypadá jako cesta ze systému,
/// který neodpovídá aktuálnímu (PROJECT.md §3.1): katalog z macOS otevřený na
/// Windows, nebo naopak.
///
/// Vrací `true`, pokud cesta VYPADÁ jako windows-style kořen (`C:/...` nebo
/// `//server/share/...`). `false` znamená, že jde pravděpodobně o POSIX cestu
/// (`/Users/...`, `/Volumes/...`) a mapování na lokální disk nedává smysl bez
/// ručního zásahu uživatele.
pub fn looks_like_windows_root(absolute_path: &str) -> bool {
    let bytes = absolute_path.as_bytes();
    if bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && bytes[2] == b'/' {
        return true;
    }
    absolute_path.starts_with("//")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_key_normalizes_nfd_to_nfc_and_lowercases() {
        use unicode_normalization::UnicodeNormalization;
        let nfc = "Plitvička.CR2"; // precomposed (Windows/Linux)
        let nfd: String = nfc.nfd().collect(); // decomposed, as macOS would store it
        assert_ne!(
            nfc.as_bytes(),
            nfd.as_bytes(),
            "test fixture must actually differ byte-wise"
        );
        assert_eq!(
            match_key(std::ffi::OsStr::new(nfc)),
            match_key(std::ffi::OsStr::new(&nfd))
        );
    }

    #[test]
    fn expected_native_path_joins_and_converts_separators() {
        let p = expected_native_path("C:/Foto/", "2019/2019-07 HR/", "IMG_1234.CR2");
        #[cfg(windows)]
        assert_eq!(p, PathBuf::from(r"C:\Foto\2019\2019-07 HR\IMG_1234.CR2"));
    }

    #[test]
    fn expected_native_path_handles_empty_rel_path() {
        let p = expected_native_path("C:/Foto/", "", "IMG_0001.CR2");
        #[cfg(windows)]
        assert_eq!(p, PathBuf::from(r"C:\Foto\IMG_0001.CR2"));
    }

    #[cfg(windows)]
    #[test]
    fn to_verbatim_local_path() {
        let p = to_verbatim(Path::new(r"C:\Foto\2019\IMG_0001.CR2")).unwrap();
        assert_eq!(p, PathBuf::from(r"\\?\C:\Foto\2019\IMG_0001.CR2"));
    }

    #[cfg(windows)]
    #[test]
    fn to_verbatim_unc_path() {
        let p = to_verbatim(Path::new(r"\\nas\photo\IMG_0001.CR2")).unwrap();
        assert_eq!(p, PathBuf::from(r"\\?\UNC\nas\photo\IMG_0001.CR2"));
    }

    #[cfg(windows)]
    #[test]
    fn to_verbatim_rejects_relative_path() {
        assert!(to_verbatim(Path::new("Foto/IMG_0001.CR2")).is_none());
    }

    /// Verbatim prefix vypíná normalizaci v OS - dopředné lomítko za ním
    /// WinAPI nepřeloží a volání selže. Platí i pro UNC větev, ne jen pro
    /// lokální disk.
    #[cfg(windows)]
    #[test]
    fn to_verbatim_converts_forward_slashes_in_unc_path() {
        let p = to_verbatim(Path::new(r"\\nas/photo/IMG_0001.CR2")).unwrap();
        assert_eq!(p, PathBuf::from(r"\\?\UNC\nas\photo\IMG_0001.CR2"));
        assert!(
            !p.to_string_lossy().contains('/'),
            "za verbatim prefixem nesmí zůstat dopředné lomítko"
        );
    }

    /// Katalog ukládá UNC kořeny s dopřednými lomítky (`//nas/photo/`) - v
    /// tomhle tvaru se cesta dostane i z CLI/GUI, takže ji `to_verbatim` musí
    /// poznat, jinak tiše přijde o podporu cest > 260 znaků.
    #[cfg(windows)]
    #[test]
    fn to_verbatim_accepts_catalog_style_unc_with_forward_slashes() {
        let p = to_verbatim(Path::new("//nas/photo/IMG_0001.CR2")).unwrap();
        assert_eq!(p, PathBuf::from(r"\\?\UNC\nas\photo\IMG_0001.CR2"));
    }

    /// NTFS neověřuje párování surrogátů, takže nepárový surrogát je legální
    /// jméno souboru, které NEJDE reprezentovat v UTF-8. Dřívější
    /// implementace přes `path.to_str()?` na něm vracela `None` → cesta tiše
    /// přišla o verbatim prefix, a tím o podporu délky > 260 znaků.
    #[cfg(windows)]
    #[test]
    fn to_verbatim_preserves_unpaired_surrogate_in_filename() {
        use std::ffi::OsString;
        use std::os::windows::ffi::{OsStrExt, OsStringExt};

        // "C:\Foto\A" + osamocený high surrogate + ".CR2"
        let mut units: Vec<u16> = r"C:\Foto\A".encode_utf16().collect();
        units.push(0xD800);
        units.extend(".CR2".encode_utf16());
        let path = PathBuf::from(OsString::from_wide(&units));

        assert!(
            path.to_str().is_none(),
            "fixture musí být opravdu nereprezentovatelná v UTF-8"
        );

        let verbatim = to_verbatim(&path).expect("WTF-16 cesta musí jít převést na verbatim");
        let out: Vec<u16> = verbatim.as_os_str().encode_wide().collect();

        let mut expected: Vec<u16> = r"\\?\C:\Foto\A".encode_utf16().collect();
        expected.push(0xD800);
        expected.extend(".CR2".encode_utf16());
        assert_eq!(out, expected, "surrogát se musí přenést beze změny");
    }

    #[cfg(windows)]
    #[test]
    fn to_verbatim_leaves_already_verbatim_path_untouched() {
        let p = to_verbatim(Path::new(r"\\?\C:\Foto\IMG_0001.CR2")).unwrap();
        assert_eq!(p, PathBuf::from(r"\\?\C:\Foto\IMG_0001.CR2"));
    }

    #[cfg(windows)]
    #[test]
    fn to_verbatim_rejects_bare_double_separator() {
        assert!(to_verbatim(Path::new(r"\\")).is_none());
        assert!(to_verbatim(Path::new("//")).is_none());
    }

    #[test]
    fn match_key_output_is_always_nfc_even_when_lowercasing_decomposes() {
        // `İ` (U+0130) se lowercasuje na `i` + U+0307 (dekomponovaně).
        // Při pořadí NFC → lowercase by výsledek NFC nebyl.
        let key = match_key_str("İ");
        let renormalized: String = key.nfc().collect();
        assert_eq!(
            key, renormalized,
            "match_key musí vracet NFC i pro znaky, které lowercase rozkládá"
        );
    }

    #[test]
    fn match_key_still_matches_czech_nfd_and_nfc() {
        // Regrese k původnímu chování - změna pořadí nesmí rozbít češtinu.
        let nfc = "Plitvička.CR2";
        let nfd: String = nfc.nfd().collect();
        assert_eq!(match_key_str(nfc), match_key_str(&nfd));
        assert_eq!(match_key_str("PLITVIČKA.CR2"), match_key_str(nfc));
    }

    #[cfg(windows)]
    #[test]
    fn exists_verbatim_handles_paths_over_260_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let mut deep = tmp.path().to_path_buf();
        // Vytvoř zanořenou strukturu, dokud celková délka cesty nepřesáhne 260 znaků.
        while deep.as_os_str().len() < 280 {
            deep = deep.join("a_very_long_directory_name_used_only_for_testing_long_paths");
            let verbatim = to_verbatim(&deep).unwrap();
            std::fs::create_dir(&verbatim).unwrap();
        }
        let file = deep.join("dlouhy_soubor.CR2");
        std::fs::write(to_verbatim(&file).unwrap(), b"x").unwrap();

        assert!(file.as_os_str().len() > 260);
        assert!(exists_verbatim(&file));
        assert!(!exists_verbatim(&deep.join("neexistuje.CR2")));
    }

    #[cfg(windows)]
    #[test]
    fn read_dir_verbatim_lists_entries_over_260_chars() {
        let tmp = tempfile::tempdir().unwrap();
        let mut deep = tmp.path().to_path_buf();
        while deep.as_os_str().len() < 280 {
            deep = deep.join("a_very_long_directory_name_used_only_for_testing_long_paths");
            std::fs::create_dir(to_verbatim(&deep).unwrap()).unwrap();
        }
        std::fs::write(to_verbatim(&deep.join("x.CR2")).unwrap(), b"x").unwrap();

        let names: Vec<String> = read_dir_verbatim(&deep)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert_eq!(names, vec!["x.CR2".to_string()]);
    }

    #[test]
    fn looks_like_windows_root_detects_posix_paths() {
        assert!(looks_like_windows_root("C:/Foto/"));
        assert!(looks_like_windows_root("//nas/photo/"));
        assert!(!looks_like_windows_root("/Users/pavel/Pictures/"));
        assert!(!looks_like_windows_root("/Volumes/Disk/"));
    }
}
