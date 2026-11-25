//! "Otevřít v Průzkumníku" (PROJECT.md §6.3): "poslední instance, kde si
//! uživatel může ověřit, že nástroj nelže" - patří do každého dialogu, který
//! nabízí přesun/přepsání cesty (§6.2, §6.3), a do kontextového menu tabulky
//! (`Ctrl+Enter`).

use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ShellError {
    #[error("nepodařilo se otevřít Průzkumníka: {0}")]
    Failed(String),
}

#[cfg(windows)]
mod win {
    use super::{Path, ShellError};
    use windows::core::HSTRING;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    /// Otevře Průzkumníka a v něm rovnou označí `path` (`/select`, PROJECT.md
    /// §6.3) - na rozdíl od pouhého otevření rodičovské složky ukáže přesně
    /// ten soubor, o kterém dialog mluví.
    pub fn reveal_in_explorer(path: &Path) -> Result<(), ShellError> {
        let operation = HSTRING::from("open");
        let file = HSTRING::from("explorer.exe");
        let params = HSTRING::from(format!("/select,\"{}\"", path.display()));

        // SAFETY: `operation`/`file`/`params` jsou platné `HSTRING` hodnoty,
        // které žijí po celou dobu volání (lokální proměnné, zničené až po
        // něm). `hwnd` je `HWND::default()` (žádné rodičovské okno - dialog
        // v `egui` žádný nativní HWND handle nemá) a `lpdirectory` je
        // `None`, což `ShellExecuteW` bere jako "použij aktuální pracovní
        // adresář procesu" - obojí je zdokumentované platné použití.
        // `ShellExecuteW` jen spustí `explorer.exe` jako nový proces, nic
        // nezapisuje přes žádný z předaných ukazatelů zpátky.
        let result = unsafe {
            ShellExecuteW(
                HWND::default(),
                &operation,
                &file,
                &params,
                None,
                SW_SHOWNORMAL,
            )
        };

        // ShellExecuteW vrací hodnotu > 32 při úspěchu (historická WinAPI
        // konvence z 16bitového Windows, ne HRESULT/bool).
        if result.0 as isize > 32 {
            Ok(())
        } else {
            Err(ShellError::Failed(format!(
                "ShellExecuteW selhalo (návratový kód {})",
                result.0 as isize
            )))
        }
    }

    /// Restart aplikace s právy správce (`runas`) - PROJECT.md §5.2
    /// "Elevace": jediné místo, kde nástroj smí požádat o UAC, a jen na
    /// explicitní kliknutí uživatele, nikdy automaticky při startu. `args`
    /// se předávají jako parametry nového procesu (typicky `--resume
    /// <session.json>`, aby uživatel po restartu neztratil rozdělanou
    /// práci).
    pub fn relaunch_elevated(exe: &Path, args: &[String]) -> Result<(), ShellError> {
        let operation = HSTRING::from("runas");
        let file = HSTRING::from(exe.to_string_lossy().as_ref());
        let params_str = args
            .iter()
            .map(|a| {
                if a.contains(' ') {
                    format!("\"{a}\"")
                } else {
                    a.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(" ");
        let params = HSTRING::from(params_str.as_str());

        // SAFETY: stejné zdůvodnění jako `reveal_in_explorer` výš -
        // `operation`/`file`/`params` žijí po celou dobu volání, `hwnd` je
        // výchozí (žádné rodičovské okno), `lpdirectory` `None`.
        let result = unsafe {
            ShellExecuteW(
                HWND::default(),
                &operation,
                &file,
                &params,
                None,
                SW_SHOWNORMAL,
            )
        };

        if result.0 as isize > 32 {
            Ok(())
        } else {
            Err(ShellError::Failed(format!(
                "ShellExecuteW(runas) selhalo (návratový kód {}) - uživatel UAC prompt \
                 nejspíš odmítl",
                result.0 as isize
            )))
        }
    }
}

#[cfg(windows)]
pub use win::{relaunch_elevated, reveal_in_explorer};

#[cfg(not(windows))]
pub fn reveal_in_explorer(path: &Path) -> Result<(), ShellError> {
    let dir = path.parent().unwrap_or(path);
    std::process::Command::new("xdg-open")
        .arg(dir)
        .spawn()
        .map(|_| ())
        .map_err(|e| ShellError::Failed(e.to_string()))
}

#[cfg(not(windows))]
pub fn relaunch_elevated(_exe: &Path, _args: &[String]) -> Result<(), ShellError> {
    Err(ShellError::Failed(
        "restart s právy správce je dostupný jen na Windows".to_string(),
    ))
}
