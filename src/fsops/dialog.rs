//! Tenký FFI wrapper nad systémovým dialogem pro výběr souboru.
//!
//! Sem patří `unsafe` (CLAUDE.md §8: „`unsafe` je povolený jen v
//! `index/mft.rs` a v tenkých FFI wrapperech ve `fsops/`"). Volající z `app/`
//! dostane hotový [`PathBuf`] a s WinAPI vůbec nepřijde do styku.
//!
//! Používá `GetOpenFileNameW` místo novějšího `IFileOpenDialog`, protože
//! nepotřebuje inicializaci COM - a `app/` běží v UI vlákně, kde by
//! `CoInitializeEx` kolidovalo s tím, co si nastavuje `eframe`/`winit`.

use std::path::{Path, PathBuf};

/// Otevře systémový dialog „Otevřít soubor" filtrovaný na `.lrcat`.
///
/// `initial` je adresář, ve kterém se má dialog otevřít (typicky složka
/// naposledy použitého katalogu). `None` znamená „nech na systému".
///
/// Vrací `None`, když uživatel dialog zavřel bez výběru - to není chyba a
/// nesmí se jako chyba hlásit.
#[cfg(windows)]
pub fn pick_catalog(initial: Option<&Path>) -> Option<PathBuf> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};

    use windows::core::{PCWSTR, PWSTR};
    use windows::Win32::UI::Controls::Dialogs::{
        GetOpenFileNameW, OFN_EXPLORER, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_PATHMUSTEXIST,
        OPENFILENAMEW,
    };

    /// UTF-16 s ukončovací nulou. Cesty se předávají jako WTF-16 jednotky
    /// (`encode_wide`), takže se neztratí ani jméno, které není platné UTF-8
    /// (CLAUDE.md invariant 7 - normalizace slouží jen k porovnání).
    fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
        s.encode_wide().chain(std::iter::once(0)).collect()
    }

    // Filtr je dvojitě null-ukončený seznam dvojic popisek/maska.
    let filter: Vec<u16> = "Katalog Lightroom Classic (*.lrcat)\0*.lrcat\0Všechny soubory\0*.*\0\0"
        .encode_utf16()
        .collect();
    let title = wide(std::ffi::OsStr::new("Vyberte katalog Lightroom Classic"));

    let initial_dir = initial
        .and_then(|p| if p.is_dir() { Some(p) } else { p.parent() })
        .map(|p| wide(p.as_os_str()));

    // GetOpenFileNameW zapisuje výslednou cestu do tohoto bufferu. 32k u16
    // pokryje i cestu delší než MAX_PATH (§16.1).
    let mut file_buf: Vec<u16> = vec![0u16; 32 * 1024];

    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        lpstrFilter: PCWSTR(filter.as_ptr()),
        nFilterIndex: 1,
        lpstrFile: PWSTR(file_buf.as_mut_ptr()),
        nMaxFile: file_buf.len() as u32,
        lpstrTitle: PCWSTR(title.as_ptr()),
        lpstrInitialDir: initial_dir
            .as_ref()
            .map(|d| PCWSTR(d.as_ptr()))
            .unwrap_or(PCWSTR::null()),
        // NOCHANGEDIR: dialog nesmí přenastavit pracovní adresář procesu -
        // ostatní kód pracuje s relativními cestami vůči tomu, kde jsme
        // spustili, a tichá změna by ho rozbila.
        Flags: OFN_EXPLORER | OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR,
        ..Default::default()
    };

    // SAFETY: `ofn` je platně inicializovaná struktura, jejíž `lStructSize`
    // odpovídá skutečné velikosti typu. Všechny ukazatele v ní (`filter`,
    // `title`, `initial_dir`, `file_buf`) míří do bufferů, které žijí déle
    // než tohle volání - drží je lokální proměnné až do konce funkce.
    // `nMaxFile` odpovídá skutečné délce `file_buf`, takže WinAPI nemůže
    // zapsat za jeho konec.
    let ok = unsafe { GetOpenFileNameW(&mut ofn).as_bool() };
    if !ok {
        // Uživatel dal Storno (nebo dialog selhal). Obojí = „nic nevybral".
        return None;
    }

    let len = file_buf.iter().position(|&c| c == 0).unwrap_or(0);
    if len == 0 {
        return None;
    }
    Some(PathBuf::from(std::ffi::OsString::from_wide(
        &file_buf[..len],
    )))
}

/// Na jiných platformách dialog není - `app/` si s `None` poradí (uživatel
/// cestu napíše do textového pole).
#[cfg(not(windows))]
pub fn pick_catalog(_initial: Option<&Path>) -> Option<PathBuf> {
    None
}
