//! Backend A (PROJECT.md §5.2): Everything. Implementováno jako subprocess
//! `es.exe` (CLI klient Everything) - PROJECT.md §5.2 to výslovně nabízí
//! jako variantu vedle IPC/`WM_COPYDATA` a DLL SDK.
//!
//! **Vědomá volba oproti raw IPC/DLL:** CLAUDE.md povoluje `unsafe` jen
//! v `index/mft.rs` a v tenkých FFI wrapperech ve `fsops/`. IPC přes okenní
//! zprávy (`WM_COPYDATA`) by unsafe do `index/` přineslo, subprocess
//! `es.exe` ho nepotřebuje vůbec. Když `es.exe` není na `PATH` (Everything
//! není nainstalované, nebo je nainstalované bez CLI klienta), `try_index`
//! vrátí `None` a volající tiše spadne na backend C (walk) - to není tichý
//! fallback měnící data, je to jen volba zdroje pro read-only INDEX.

use std::collections::HashSet;
use std::path::PathBuf;
use std::process::Command;

use super::{is_excluded, NameIndex};
use crate::fsops::paths::{match_key, match_key_str};

/// Kolik jmen se pošle v jednom dotazu (PROJECT.md §5.2: "OR dotazy po ~50 jménech").
const BATCH_SIZE: usize = 50;

/// Zkusí dohledat `filenames` přes `es.exe`. Vrací `None`, pokud `es.exe`
/// není dostupné vůbec (žádný dotaz neprošel), nebo pokud jeho výstup
/// neprojde sebekontrolou kódování (viz níž) - v obou případech volající
/// spadne na backend C (walk).
///
/// **Sebekontrola kódování místo domněnky (CLAUDE.md §6):** kódování stdout
/// `es.exe` není ověřené - podle verze a nastavení konzole to může být UTF-8
/// i konzolová codepage, a v tom druhém případě by se česká diakritika
/// rozsypala. Místo hádání (nebo přidání `-utf8` přepínače, o kterém nevíme,
/// jestli ho uživatelova verze podporuje) se výsledek ověří empiricky:
/// dotazujeme se na PŘESNÁ jména, takže se aspoň jedno vrácené jméno musí
/// shodovat s některým hledaným. Stejný princip jako runtime kalibrace času
/// v §5.4 - ověř na datech, nespoléhej na dokumentaci.
pub fn try_index(filenames: &HashSet<String>, exclude: &[String]) -> Option<NameIndex> {
    if filenames.is_empty() {
        return None;
    }

    let names: Vec<&String> = filenames.iter().collect();
    let requested_keys: HashSet<String> = filenames.iter().map(|n| match_key_str(n)).collect();

    let mut index = NameIndex::default();
    let mut any_batch_succeeded = false;
    let mut returned_paths = 0usize;
    let mut matching_paths = 0usize;

    for batch in names.chunks(BATCH_SIZE) {
        let query = build_query(batch);
        match run_query(&query) {
            Some(paths) => {
                any_batch_succeeded = true;
                for path in paths {
                    returned_paths += 1;
                    // Sebekontrola kódování (viz doc komentář funkce).
                    if path
                        .file_name()
                        .map(|n| requested_keys.contains(&match_key(n)))
                        .unwrap_or(false)
                    {
                        matching_paths += 1;
                    }
                    let forward = path.to_string_lossy().replace('\\', "/");
                    if !is_excluded(&forward, exclude) {
                        index.insert(path);
                    }
                }
            }
            None => {
                tracing::debug!("es.exe dávkový dotaz selhal, přeskočeno");
            }
        }
    }

    if !any_batch_succeeded {
        return None;
    }

    // Dotazovali jsme se na PŘESNÁ jména (`file:exact:`), takže cokoli, co se
    // vrátí, musí odpovídat některému z nich. Když se nevrátí ani jedna
    // shoda, je výstup `es.exe` pro nás nepoužitelný - typicky proto, že ho
    // vypsal v jiném kódování, než čekáme (konzolová codepage místo UTF-8;
    // česká diakritika se pak rozsype). Bez téhle kontroly bychom vrátili
    // neprázdný, ale nepoužitelný index a na walk backend už nikdy nespadli.
    if returned_paths > 0 && matching_paths == 0 {
        tracing::warn!(
            returned_paths,
            "es.exe vrátil výsledky, ale ani jeden neodpovídá hledaným jménům \
             (pravděpodobně jiné kódování výstupu) - backend se přeskakuje"
        );
        return None;
    }

    Some(index)
}

fn build_query(names: &[&String]) -> String {
    names
        .iter()
        .map(|name| format!("file:exact:\"{}\"", escape(name)))
        .collect::<Vec<_>>()
        .join("|")
}

/// Windows jména souborů nesmí obsahovat `"` vůbec, ale radši ho pro jistotu
/// odstraníme, než abychom sestavili nevalidní/nebezpečně sestavený dotaz.
fn escape(name: &str) -> String {
    name.replace('"', "")
}

fn run_query(query: &str) -> Option<Vec<PathBuf>> {
    let output = Command::new("es.exe")
        .arg("-sort")
        .arg("name")
        .arg(query)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(
        text.lines()
            .filter(|line| !line.is_empty())
            .map(PathBuf::from)
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_filename_set_needs_no_query() {
        assert!(try_index(&HashSet::new(), &[]).is_none());
    }

    #[test]
    fn build_query_ors_exact_filename_clauses() {
        let a = "IMG_0001.CR2".to_string();
        let b = "IMG_0002.CR2".to_string();
        let query = build_query(&[&a, &b]);
        assert_eq!(
            query,
            "file:exact:\"IMG_0001.CR2\"|file:exact:\"IMG_0002.CR2\""
        );
    }

    #[test]
    fn falls_back_gracefully_when_es_exe_is_unavailable() {
        // Testovací stroj nemá Everything/es.exe nainstalované - tenhle test
        // tedy skutečně cvičí "backend nedostupný" větev, ne mock.
        let mut names = HashSet::new();
        names.insert("IMG_0001.CR2".to_string());
        assert!(try_index(&names, &[]).is_none());
    }
}
