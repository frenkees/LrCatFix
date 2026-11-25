//! Tenký FFI wrapper nad WinAPI pro zjištění připojených svazků a jejich typu.
//!
//! CLAUDE.md: `unsafe` je povolený jen tady a v `index/mft.rs`. Business logika
//! (sonda root remapu, výběr počtu I/O vláken) je jinde a volá jen bezpečné
//! funkce z tohoto modulu.
//!
//! Rozsah M0 (PROJECT.md §7.4.1): enumerace lokálních písmen disků stačí na
//! detekci "změna písmene disku odhalena bez skenování" - to je to, co M0
//! Definition of Done vyžaduje. Svazky připojené bez písmene (mount points)
//! jsou vědomě mimo rozsah M0 a patří do rozšíření sondy v pozdějším milníku.

#[cfg(windows)]
mod win {
    use windows::core::HSTRING;
    use windows::Win32::Storage::FileSystem::{
        GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDrives,
    };
    use windows::Win32::System::WindowsProgramming::DRIVE_REMOTE;

    /// Vrátí písmena všech aktuálně připojených lokálních svazků (A-Z).
    pub fn list_drive_letters() -> Vec<char> {
        // SAFETY: GetLogicalDrives nebere žádné ukazatele, jen vrací bitovou
        // masku (bit 0 = A:, bit 1 = B:, ...). Nemá způsob selhání, který by
        // vyžadoval kontrolu GetLastError.
        let mask = unsafe { GetLogicalDrives() };
        (0..26)
            .filter(|bit| mask & (1 << bit) != 0)
            .map(|bit| (b'A' + bit as u8) as char)
            .collect()
    }

    /// `true`, pokud `root` (např. `"D:\\"`) je síťový svazek (mapovaný
    /// disk / UNC). Používá se pro výběr počtu I/O vláken (PROJECT.md §5.1,
    /// CLAUDE.md „liší se pro lokální a síťové svazky").
    pub fn is_network_drive(root: &str) -> bool {
        let wide = HSTRING::from(root);
        // SAFETY: `wide` drží vlastní nulou ukončený buffer po celou dobu
        // volání; GetDriveTypeW ho jen čte a vrací kód typu, nemá selhání
        // signalizované přes GetLastError.
        let drive_type = unsafe { GetDriveTypeW(&wide) };
        drive_type == DRIVE_REMOTE
    }

    /// Bajty volné na svazku obsahujícím existující adresář `dir`
    /// (PROJECT.md §2 „Pre-flight kontrola místa"). `None`, pokud volání
    /// selže (např. `dir` neexistuje nebo je nedostupný).
    pub fn free_space_bytes(dir: &str) -> Option<u64> {
        let wide = HSTRING::from(dir);
        let mut free_available: u64 = 0;
        // SAFETY: `wide` je platný, nulou ukončený řetězec po celou dobu
        // volání. `free_available` je platná lokální `u64`, na kterou
        // WinAPI zapíše výsledek - žádný jiný ukazatel se nepředává (druhé
        // a třetí volitelné pole necháváme `None`, protože nás zajímá jen
        // místo dostupné volajícímu, ne celková/celkově volná kapacita).
        let result = unsafe { GetDiskFreeSpaceExW(&wide, Some(&mut free_available), None, None) };
        result.ok().map(|()| free_available)
    }
}

#[cfg(windows)]
pub use win::{is_network_drive, list_drive_letters};

/// Fallback pro ne-Windows build (Linux, PROJECT.md §4 „sekundárně Linux").
/// Sonda root remapu (§7.4) je stejně windows-specifická (písmena disků), na
/// Linuxu se INDEX fáze (§5.2) opře jen o backend C) walk.
#[cfg(not(windows))]
pub fn list_drive_letters() -> Vec<char> {
    Vec::new()
}

#[cfg(not(windows))]
pub fn is_network_drive(_root: &str) -> bool {
    false
}

/// Bajty volné na svazku obsahujícím `path` (PROJECT.md §2 pre-flight
/// kontrola místa před zálohou). `path` sám nemusí existovat - stačí, aby
/// existoval nějaký jeho předek (WinAPI potřebuje existující adresář).
pub fn free_space_bytes(path: &std::path::Path) -> Option<u64> {
    let existing = first_existing_ancestor(path)?;
    free_space_bytes_of_existing(&existing.to_string_lossy())
}

fn first_existing_ancestor(path: &std::path::Path) -> Option<std::path::PathBuf> {
    let mut current = Some(path);
    while let Some(p) = current {
        if p.exists() {
            return Some(p.to_path_buf());
        }
        current = p.parent();
    }
    None
}

#[cfg(windows)]
fn free_space_bytes_of_existing(dir: &str) -> Option<u64> {
    win::free_space_bytes(dir)
}

#[cfg(not(windows))]
fn free_space_bytes_of_existing(_dir: &str) -> Option<u64> {
    None
}

/// Kolik I/O vláken použít pro VERIFY nad `catalog_path` (PROJECT.md §5.1 bod
/// 4, CLAUDE.md „liší se pro lokální a síťové svazky"). Sdílené mezi CLI a GUI
/// vstupním bodem, ať se logika nezdvojuje.
pub fn pick_io_thread_count(
    catalog_path: &std::path::Path,
    cfg: &crate::config::ScanConfig,
) -> usize {
    let absolute =
        std::fs::canonicalize(catalog_path).unwrap_or_else(|_| catalog_path.to_path_buf());
    if is_network_path(&absolute) {
        cfg.threads_network
    } else {
        cfg.threads_local
    }
}

/// `true`, pokud je svazek, na kterém `path` leží, vůbec dostupný.
///
/// **Proč to nestačí poznat z chyby `read_dir`:** odpojené písmeno disku
/// (`R:` bez připojeného disku) hlásí Windows jako `ERROR_PATH_NOT_FOUND`,
/// což se v Rustu mapuje na `io::ErrorKind::NotFound` - úplně stejně jako
/// skutečně smazaná složka. Ověřeno na reálném katalogu (LrC 13.3, 2026-08):
/// `os.listdir("Q:\\FOTO")` na nepřipojeném svazku vrací winerror 3.
///
/// Bez téhle kontroly by odpojený externí disk nebo NAS vypadal jako
/// smazané fotky a CLUSTER by nabídl přepointovat katalog na zálohu -
/// přesně to tiché poškození, kterému má nástroj bránit (CLAUDE.md „Zlaté
/// pravidlo"). U katalogu, kde 99 % souborů leží na jednom externím disku,
/// je to rozdíl mezi „nic se neděje" a „nabídnu ti přepsat celý katalog".
///
/// Když typ cesty nejde určit (relativní cesta, exotický prefix), vrací
/// `true` - tedy zachová dosavadní chování a nechá rozhodnout `read_dir`.
pub fn is_volume_available(path: &std::path::Path) -> bool {
    match volume_root_of(path) {
        Some(root) => crate::fsops::paths::exists_verbatim(&root),
        None => true,
    }
}

/// Kořen svazku, na kterém `path` leží - `R:\` pro lokální disk,
/// `\\server\share` pro UNC.
#[cfg(windows)]
fn volume_root_of(path: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::path::{Component, Prefix};
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                Some(std::path::PathBuf::from(format!("{}:\\", letter as char)))
            }
            Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
                let mut p = std::ffi::OsString::from(r"\\");
                p.push(server);
                p.push(r"\");
                p.push(share);
                Some(std::path::PathBuf::from(p))
            }
            _ => None,
        },
        _ => None,
    }
}

#[cfg(not(windows))]
fn volume_root_of(_path: &std::path::Path) -> Option<std::path::PathBuf> {
    None
}

/// Kolik I/O vláken je vhodné pro svazek, na kterém `path` leží.
///
/// UNC cesta (`\\server\share\...`) je z definice síťová, takže se pozná
/// rovnou z tvaru cesty - `GetDriveTypeW` se na ni ptát nemusí (a ani nedá,
/// nemá písmeno disku). Bez tohohle rozlišení by síťový share dostal
/// `threads_local` (16) místo `threads_network` (4), což PROJECT.md §5.1
/// bod 4 explicitně nechce.
#[cfg(windows)]
fn is_network_path(path: &std::path::Path) -> bool {
    use std::path::{Component, Prefix};
    match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _) => true,
            Prefix::Disk(letter) | Prefix::VerbatimDisk(letter) => {
                is_network_drive(&format!("{}:\\", letter as char))
            }
            _ => false,
        },
        _ => false,
    }
}

#[cfg(not(windows))]
fn is_network_path(_path: &std::path::Path) -> bool {
    false
}

#[cfg(not(windows))]
fn drive_root_of(_path: &std::path::Path) -> Option<String> {
    None
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn list_drive_letters_includes_system_drive() {
        let drives = list_drive_letters();
        // Testovací stroj vždy má aspoň jeden svazek (typicky C:).
        assert!(!drives.is_empty());
    }

    #[test]
    fn free_space_bytes_reports_something_plausible_for_temp_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let free = free_space_bytes(tmp.path()).expect("temp dir musí ležet na reálném svazku");
        assert!(free > 0);
    }

    #[test]
    fn free_space_bytes_walks_up_to_existing_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let nonexistent = tmp.path().join("neexistuje").join("ani-tohle");
        assert!(free_space_bytes(&nonexistent).is_some());
    }
}
