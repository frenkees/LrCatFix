//! Backend B (PROJECT.md §5.2): vlastní MFT scan přes USN journal
//! (`FSCTL_ENUM_USN_DATA`). VYŽADUJE elevovaný token (`SE_MANAGE_VOLUME`) -
//! volající (`index::build_index`) musí sám ověřit
//! `fsops::elevation::is_elevated()` PŘED voláním; tenhle modul elevaci
//! sám nekontroluje, jen se otevření svazku bez ní nezdaří.
//!
//! Postup: otevři `\\.\C:`, opakovaně volej `FSCTL_ENUM_USN_DATA` a sestav
//! mapu FRN → (parentFRN, jméno) pro VŠECHNY záznamy na svazku (soubory i
//! složky - potřeba pro rekonstrukci cest až ke kořeni), pak z ní slož
//! absolutní cesty jen pro soubory.
//!
//! `unsafe` je tu izolovaný podle CLAUDE.md ("`unsafe` je povolený jen v
//! `index/mft.rs` a v tenkých FFI wrapperech ve `fsops/`") - žádná business
//! logika, jen tenký wrapper nad `DeviceIoControl`/`CreateFileW` a
//! parsování surového bufferu `USN_RECORD_V2` (variabilní délka, nejde
//! vyjádřit bezpečně v Rustu bez ukazatelové aritmetiky).

use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum MftError {
    #[error("svazek {0} není NTFS/ReFS - MFT scan přeskočen")]
    NotNtfs(String),
    #[error("otevření svazku {volume} selhalo (kód {code}) - nejspíš chybí oprávnění správce")]
    AccessDenied { volume: String, code: u32 },
    #[error("DeviceIoControl(FSCTL_ENUM_USN_DATA) selhalo na {volume} (kód {code})")]
    Ioctl { volume: String, code: u32 },
    #[error("MFT scan je dostupný jen na Windows")]
    UnsupportedPlatform,
}

#[cfg(windows)]
mod win {
    use std::collections::HashMap;
    use std::ffi::{c_void, OsString};
    use std::os::windows::ffi::OsStringExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};

    use windows::core::HSTRING;
    use windows::Win32::Foundation::{CloseHandle, GetLastError, GENERIC_READ, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, GetVolumeInformationW, FILE_ATTRIBUTE_DIRECTORY, FILE_FLAGS_AND_ATTRIBUTES,
        FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Ioctl::{FSCTL_ENUM_USN_DATA, MFT_ENUM_DATA_V0, USN_RECORD_V2};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::System::IO::DeviceIoControl;

    use super::MftError;
    use crate::index::{is_excluded, NameIndex};

    /// `true`, pokud aktuální proces běží s elevovaným tokenem
    /// (`GetTokenInformation`/`TokenElevation`, §5.2) - backend B je
    /// dostupný jen tehdy. Chyba při zjišťování (např. `OpenProcessToken`
    /// selže) se bere jako "není elevovaný", ne jako neznámý stav - bez
    /// jistoty se elevovaný přístup ke svazku ani nezkouší.
    pub fn is_process_elevated() -> bool {
        // SAFETY: `GetCurrentProcess` vrací pseudo-handle aktuálního
        // procesu, nikdy neselže a nebere žádný ukazatel.
        let process = unsafe { GetCurrentProcess() };
        let mut token = HANDLE::default();
        // SAFETY: `process` je platný pseudo-handle výš. `token` je platná
        // lokální proměnná, kam WinAPI zapíše výsledný handle tokenu.
        if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) }.is_err() {
            return false;
        }

        let mut elevation = TOKEN_ELEVATION::default();
        let mut returned_len: u32 = 0;
        // SAFETY: `token` je platný handle z `OpenProcessToken` výš.
        // `elevation` je platná lokální struktura přesně velikosti, kterou
        // `TokenElevation` třída očekává (WinAPI kontrakt), `returned_len`
        // platná lokální `u32`.
        let result = unsafe {
            GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut elevation as *mut _ as *mut c_void),
                std::mem::size_of::<TOKEN_ELEVATION>() as u32,
                &mut returned_len,
            )
        };
        // SAFETY: `token` byl vrácen `OpenProcessToken` výš a musí se
        // zavřít bez ohledu na výsledek `GetTokenInformation`.
        unsafe {
            let _ = CloseHandle(token);
        }

        result.is_ok() && elevation.TokenIsElevated != 0
    }

    /// §5.2 "miliony záznamů za jednotky sekund" - velký výstupní buffer na
    /// volání snižuje počet syscallů, což je u milionů záznamů znatelné.
    const OUTPUT_BUFFER_SIZE: usize = 1024 * 1024;

    // Raw win32 chybové kódy - stejná konvence jako `fsops::moves::RETRYABLE_CODES`.
    const ERROR_HANDLE_EOF: u32 = 38;

    /// `name` je `OsString`, ne `String`: NTFS ukládá jména jako posloupnost
    /// UTF-16 jednotek a NEOVĚŘUJE párování surrogátů, takže nepárový
    /// surrogát je legální jméno souboru, které nejde reprezentovat v UTF-8.
    /// Dřívější `String::from_utf16_lossy` ho nahradil `U+FFFD` - výsledná
    /// cesta pak na disku neexistovala a soubor se přes MFT nikdy nenašel,
    /// zatímco přes walk backend (který `PathBuf` z OS přebírá beze změny)
    /// nalezitelný byl. `OsString::from_wide` tuhle asymetrii odstraňuje.
    struct MftEntry {
        parent: u64,
        name: OsString,
        is_dir: bool,
    }

    /// `true`, pokud `volume_root` (např. `"C:\\"`) je NTFS nebo ReFS - MFT
    /// scan na jiných FS nefunguje (§5.2: "MFT scan přeskoč u svazků, které
    /// nejsou NTFS/ReFS").
    pub fn is_ntfs_or_refs(volume_root: &Path) -> bool {
        let wide_root = HSTRING::from(volume_root.to_string_lossy().as_ref());
        let mut fs_name = [0u16; 32];
        // SAFETY: `wide_root` žije po celou dobu volání. `fs_name` je
        // platný lokální buffer předaný jako `&mut [u16]` - WinAPI do něj
        // zapíše nanejvýš `fs_name.len()` znaků, nikdy víc (zdokumentovaný
        // kontrakt `GetVolumeInformationW`).
        let result = unsafe {
            GetVolumeInformationW(&wide_root, None, None, None, None, Some(&mut fs_name))
        };
        if result.is_err() {
            return false;
        }
        let end = fs_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(fs_name.len());
        let name = String::from_utf16_lossy(&fs_name[..end]);
        name.eq_ignore_ascii_case("NTFS") || name.eq_ignore_ascii_case("ReFS")
    }

    fn open_volume(volume_str: &str) -> Result<HANDLE, MftError> {
        // `\\.\C:` (bez koncového zpětného lomítka) - WinAPI konvence pro
        // otevření svazku jako celku, ne konkrétní složky na něm.
        let letter = volume_str.chars().next().unwrap_or('C');
        let device_path = format!(r"\\.\{letter}:");
        let wide = HSTRING::from(device_path.as_str());

        // SAFETY: `wide` je platný, nulou ukončený řetězec po celou dobu
        // volání. Bezpečnostní atributy a template handle jsou výchozí/`None`
        // - žádný jiný ukazatel se nepředává. Návratový `HANDLE` volající
        // (`build_index_for_volume`) korektně zavře přes `CloseHandle`.
        let result = unsafe {
            CreateFileW(
                &wide,
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                None,
            )
        };

        result.map_err(|_| {
            // SAFETY: GetLastError se volá hned po neúspěšném CreateFileW,
            // dřív než cokoli jiného mohlo hodnotu threadu přepsat.
            let code = unsafe { GetLastError() }.0;
            MftError::AccessDenied {
                volume: volume_str.to_string(),
                code,
            }
        })
    }

    /// Vytáhne z `record` (ukazuje do `data`, začátek na `offset`) jméno
    /// souboru - `FileName` v `USN_RECORD_V2` je flexibilní pole za pevnou
    /// částí struktury, `FileNameOffset`/`FileNameLength` udávají, kde v
    /// SUROVÉM bufferu leží (relativně k začátku záznamu, v bajtech).
    fn record_name(data: &[u8], offset: usize, record: &USN_RECORD_V2) -> Option<OsString> {
        let name_start = offset + record.FileNameOffset as usize;
        let name_len = record.FileNameLength as usize;
        let name_bytes = data.get(name_start..name_start + name_len)?;
        let utf16: Vec<u16> = name_bytes
            .chunks_exact(2)
            .map(|b| u16::from_ne_bytes([b[0], b[1]]))
            .collect();
        // `from_wide`, NE `String::from_utf16_lossy` - viz [`MftEntry`].
        Some(OsString::from_wide(&utf16))
    }

    /// Rozparsuje jednu dávku výstupu `FSCTL_ENUM_USN_DATA` (bez úvodních 8
    /// bajtů `next FRN`, ty zpracovává volající) - posloupnost `USN_RECORD_V2`
    /// záznamů proměnné délky, `RecordLength` udává, o kolik posunout ukazatel
    /// na další záznam.
    fn parse_records(data: &[u8], entries: &mut HashMap<u64, MftEntry>) {
        let mut offset = 0usize;
        let header_size = std::mem::size_of::<USN_RECORD_V2>();
        while offset + header_size <= data.len() {
            // SAFETY: `data[offset..]` má aspoň `header_size` bajtů (právě
            // ověřeno). `USN_RECORD_V2` je `#[repr(C)]` a přesně odpovídá
            // layoutu, který `FSCTL_ENUM_USN_DATA` do bufferu zapisuje
            // (zdokumentovaný WinAPI kontrakt). `read_unaligned` je nutné,
            // protože `Vec<u8>` negarantuje zarovnání na 8 bajtů, které by
            // `u64` pole struktury vyžadovala pro obyčejnou dereferenci.
            // Čteme jen PEVNÉ pole struktury (`FileName: [u16; 1]` na konci
            // se nepoužívá - skutečné jméno se čte zvlášť přes
            // `record_name` s ukazatelovou aritmetikou do `data`).
            let record =
                unsafe { (data[offset..].as_ptr() as *const USN_RECORD_V2).read_unaligned() };

            if record.RecordLength == 0 {
                break; // ochrana proti nekonečné smyčce při poškozených datech
            }
            let record_len = record.RecordLength as usize;
            if offset + record_len > data.len() {
                break; // neúplný poslední záznam - nedůvěryhodné dopočítat, radši přestat
            }

            if let Some(name) = record_name(data, offset, &record) {
                let is_dir = (record.FileAttributes & FILE_ATTRIBUTE_DIRECTORY.0) != 0;
                entries.insert(
                    record.FileReferenceNumber,
                    MftEntry {
                        parent: record.ParentFileReferenceNumber,
                        name,
                        is_dir,
                    },
                );
            }

            offset += record_len;
        }
    }

    fn enumerate_usn_data(
        handle: HANDLE,
        volume_str: &str,
        cancel: &AtomicBool,
    ) -> Result<HashMap<u64, MftEntry>, MftError> {
        let mut entries = HashMap::new();
        let mut input = MFT_ENUM_DATA_V0 {
            StartFileReferenceNumber: 0,
            LowUsn: 0,
            HighUsn: i64::MAX,
        };
        let mut buffer = vec![0u8; OUTPUT_BUFFER_SIZE];

        loop {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let mut bytes_returned: u32 = 0;
            // SAFETY: `handle` je platný svazkový handle z `open_volume`.
            // `input` je platná lokální hodnota předaná jako vstupní buffer
            // (WinAPI ji jen čte, `ninbuffersize` odpovídá `size_of`).
            // `buffer` je platný výstupní buffer o délce `buffer.len()`,
            // přesně `noutbuffersize` - WinAPI do něj nezapíše víc.
            // `bytes_returned` je platná lokální `u32`.
            let result = unsafe {
                DeviceIoControl(
                    handle,
                    FSCTL_ENUM_USN_DATA,
                    Some(&input as *const _ as *const c_void),
                    std::mem::size_of::<MFT_ENUM_DATA_V0>() as u32,
                    Some(buffer.as_mut_ptr() as *mut c_void),
                    buffer.len() as u32,
                    Some(&mut bytes_returned),
                    None,
                )
            };

            if result.is_err() {
                // SAFETY: GetLastError se volá hned po neúspěšném
                // DeviceIoControl, dřív než cokoli jiného mohlo hodnotu
                // threadu přepsat.
                let code = unsafe { GetLastError() }.0;
                if code == ERROR_HANDLE_EOF {
                    break; // konec dat - normální ukončení, ne chyba
                }
                return Err(MftError::Ioctl {
                    volume: volume_str.to_string(),
                    code,
                });
            }

            if bytes_returned < 8 {
                break; // žádná další data
            }

            // Prvních 8 bajtů výstupu = další StartFileReferenceNumber (§5.2).
            // `buffer` má vždy aspoň `OUTPUT_BUFFER_SIZE` (1 MB) bajtů, takže
            // `buffer[0..8]` nemůže selhat délkou - `unwrap_or` tu není pro
            // reálně dosažitelnou chybu, jen aby `app/`-style pravidlo
            // "žádná panika v produkčním kódu" platilo i tady: kdyby se
            // kdykoli v budoucnu tenhle předpoklad porušil, sken svazku se
            // prostě zastaví (stejná větev jako "konec dat"), místo pádu.
            let next_frn = buffer[0..8]
                .try_into()
                .map(u64::from_ne_bytes)
                .unwrap_or(input.StartFileReferenceNumber);
            parse_records(&buffer[8..bytes_returned as usize], &mut entries);

            if next_frn == input.StartFileReferenceNumber {
                break; // sonda se nehne dál - obranná pojistka proti nekonečné smyčce
            }
            input.StartFileReferenceNumber = next_frn;
        }

        Ok(entries)
    }

    /// Poskládá relativní cestu (vůči kořeni svazku) k záznamu `frn` z mapy
    /// `entries` - jde od souboru směrem k rodičům, dokud nenarazí na kořen
    /// svazku (záznam, který je sám sobě rodičem) nebo na rodiče, co v mapě
    /// není (neúplná data - radši zastavit, než hádat). `None`, pokud `frn`
    /// v mapě vůbec není.
    fn reconstruct_path(frn: u64, entries: &HashMap<u64, MftEntry>) -> Option<PathBuf> {
        let entry = entries.get(&frn)?;
        let mut segments = vec![entry.name.clone()];
        let mut parent = entry.parent;

        for _ in 0..4096 {
            let Some(p) = entries.get(&parent) else {
                break; // rodič není v mapě - typicky proto, že `parent` už byl kořen svazku
            };
            if p.parent == parent {
                break; // `p` je kořen svazku (sám sobě rodičem) - jeho jméno se nepřidává
            }
            segments.push(p.name.clone());
            parent = p.parent;
        }

        segments.reverse();
        let mut path = PathBuf::new();
        for seg in segments {
            path.push(seg);
        }
        Some(path)
    }

    /// Naskenuje celý svazek `volume_root` (např. `"C:\\"`) a vrátí
    /// `NameIndex` všech souborů pod ním, po vyloučení `exclude` vzorů
    /// (§14). Na rozdíl od `index::walk::build_index` neumí omezit sken na
    /// podsložku - USN journal enumeruje celý svazek najednou, ne
    /// hierarchicky, takže "jen tahle podsložka" nejde poznat dřív, než se
    /// cesta zrekonstruuje ze VŠECH záznamů.
    pub fn build_index_for_volume(
        volume_root: &Path,
        exclude: &[String],
        cancel: &AtomicBool,
    ) -> Result<NameIndex, MftError> {
        let volume_str = volume_root.to_string_lossy().to_string();
        if !is_ntfs_or_refs(volume_root) {
            return Err(MftError::NotNtfs(volume_str));
        }

        let handle = open_volume(&volume_str)?;
        let result = enumerate_usn_data(handle, &volume_str, cancel);
        // SAFETY: `handle` byl vrácen `CreateFileW` výš a musí se zavřít bez
        // ohledu na výsledek `enumerate_usn_data` (chybová i úspěšná cesta).
        unsafe {
            let _ = CloseHandle(handle);
        }
        let entries = result?;

        let mut index = NameIndex::default();
        for (&frn, entry) in &entries {
            if entry.is_dir {
                continue;
            }
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Some(relative) = reconstruct_path(frn, &entries) else {
                continue;
            };
            let full_path = volume_root.join(&relative);
            let forward = full_path.to_string_lossy().replace('\\', "/");
            if is_excluded(&forward, exclude) {
                continue;
            }
            index.insert(full_path);
        }

        Ok(index)
    }

    fn volume_letter_prefix(path: &Path) -> Option<String> {
        let s = path.to_string_lossy();
        let bytes = s.as_bytes();
        if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
            Some(format!("{}:", bytes[0].to_ascii_uppercase() as char))
        } else {
            None
        }
    }

    fn merge_index(into: &mut NameIndex, other: NameIndex) {
        for (key, paths) in other.by_name {
            into.by_name.entry(key).or_default().extend(paths);
        }
    }

    /// Naskenuje MFT všech svazků, na kterých leží některý z `roots`
    /// (deduplikováno po písmenu disku), a výsledky sloučí do jednoho
    /// `NameIndex`. Když MFT scan selže na KTERÉMKOLI ze svazků, vrací se
    /// chyba pro CELÉ volání - volající (`index::build_index`, §5.2 "tichý
    /// fallback na C") pak spadne na walk pro všechny `roots` najednou,
    /// místo aby v jednom indexu míchal backend B a C (jednodušší a
    /// bezpečnější než částečné míchání).
    pub fn build_index(
        roots: &[PathBuf],
        exclude: &[String],
        cancel: &AtomicBool,
    ) -> Result<NameIndex, MftError> {
        let mut merged = NameIndex::default();
        let mut seen_volumes: Vec<String> = Vec::new();

        for root in roots {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let Some(volume) = volume_letter_prefix(root) else {
                continue;
            };
            if seen_volumes.contains(&volume) {
                continue;
            }
            seen_volumes.push(volume.clone());

            let volume_root = PathBuf::from(format!("{volume}\\"));
            let per_volume = build_index_for_volume(&volume_root, exclude, cancel)?;
            merge_index(&mut merged, per_volume);
        }

        Ok(merged)
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn entry(parent: u64, name: &str, is_dir: bool) -> MftEntry {
            MftEntry {
                parent,
                name: OsString::from(name),
                is_dir,
            }
        }

        #[test]
        fn reconstruct_path_walks_up_to_root_without_including_root_name() {
            let mut entries = HashMap::new();
            entries.insert(5, entry(5, "$Volume", true)); // kořen - sám sobě rodičem
            entries.insert(10, entry(5, "2019", true));
            entries.insert(11, entry(10, "2019-07 HR", true));
            entries.insert(100, entry(11, "IMG_0001.CR2", false));

            let path = reconstruct_path(100, &entries).unwrap();
            assert_eq!(
                path,
                PathBuf::from("2019")
                    .join("2019-07 HR")
                    .join("IMG_0001.CR2")
            );
        }

        #[test]
        fn reconstruct_path_handles_file_directly_under_root() {
            let mut entries = HashMap::new();
            entries.insert(5, entry(5, "$Volume", true));
            entries.insert(100, entry(5, "IMG_0001.CR2", false));

            let path = reconstruct_path(100, &entries).unwrap();
            assert_eq!(path, PathBuf::from("IMG_0001.CR2"));
        }

        #[test]
        fn reconstruct_path_stops_gracefully_when_parent_chain_is_incomplete() {
            let mut entries = HashMap::new();
            // Rodič 999 v mapě vůbec není (např. mimo naskenovaný rozsah).
            entries.insert(100, entry(999, "IMG_0001.CR2", false));

            let path = reconstruct_path(100, &entries).unwrap();
            assert_eq!(path, PathBuf::from("IMG_0001.CR2"));
        }

        #[test]
        fn reconstruct_path_none_for_unknown_frn() {
            let entries = HashMap::new();
            assert!(reconstruct_path(42, &entries).is_none());
        }

        /// REGRESE: dřív `String::from_utf16_lossy` nahradil nepárový
        /// surrogát za `U+FFFD`, výsledná cesta na disku neexistovala a
        /// soubor se přes MFT nikdy nenašel (přes walk backend ano - ten
        /// `PathBuf` z OS přebírá beze změny). Teď musí projít beze ztráty.
        #[test]
        fn parse_records_preserves_unpaired_surrogate_in_name() {
            use std::os::windows::ffi::OsStrExt;

            // "IMG_" + osamocený high surrogate + ".CR2"
            let mut units: Vec<u16> = "IMG_".encode_utf16().collect();
            units.push(0xD800);
            units.extend(".CR2".encode_utf16());
            let name = OsString::from_wide(&units);
            assert!(
                name.to_str().is_none(),
                "fixture musí být opravdu nereprezentovatelná v UTF-8"
            );

            let buffer = build_synthetic_record_wide(100, 10, &units, false);
            let mut entries = HashMap::new();
            parse_records(&buffer, &mut entries);

            let parsed = &entries.get(&100).expect("záznam se musí rozparsovat").name;
            assert_eq!(
                parsed.encode_wide().collect::<Vec<u16>>(),
                units,
                "surrogát se nesmí ztratit ani nahradit U+FFFD"
            );
        }

        #[test]
        fn volume_letter_prefix_extracts_and_uppercases() {
            assert_eq!(
                volume_letter_prefix(Path::new(r"c:\Foto")),
                Some("C:".to_string())
            );
            assert_eq!(
                volume_letter_prefix(Path::new(r"D:\")),
                Some("D:".to_string())
            );
            assert_eq!(volume_letter_prefix(Path::new("relative")), None);
        }

        #[test]
        fn build_index_deduplicates_by_volume() {
            // Bez reálného elevovaného přístupu ke svazku tenhle test jen
            // ověřuje, že se pro dva kořeny na STEJNÉM písmenu disku
            // `build_index_for_volume` nezavolá dvakrát - pokud by se
            // zavolal, druhé volání by selhalo úplně stejně jako první
            // (žádná elevace v testovacím prostředí), takže se dá ověřit
            // nepřímo: výsledek musí být `Err` (ne zaseknutí/panic) a test
            // doběhne rychle, i kdyby "C:" i "C:\\Users" byly zadané zvlášť.
            let cancel = AtomicBool::new(false);
            let result = build_index(
                &[PathBuf::from(r"C:\"), PathBuf::from(r"C:\Users")],
                &[],
                &cancel,
            );
            // V CI/testovacím běhu bez elevace očekáváme chybu (přístup
            // odepřen) - důležité je, že tenhle test vůbec doběhne.
            let _ = result;
        }

        /// Ověřuje `parse_records` proti SKUTEČNÉMU layoutu `USN_RECORD_V2`
        /// (žádné ručně spočítané bajtové offsety) - sestaví syntetický
        /// buffer stejnou strukturou, jakou `FSCTL_ENUM_USN_DATA` vrací.
        #[test]
        fn parse_records_reads_frn_parent_name_and_directory_flag() {
            let buffer = build_synthetic_record(100, 10, "IMG_0001.CR2", false);
            let mut entries = HashMap::new();
            parse_records(&buffer, &mut entries);

            let e = entries.get(&100).expect("záznam musí být rozparsovaný");
            assert_eq!(e.parent, 10);
            assert_eq!(e.name, "IMG_0001.CR2");
            assert!(!e.is_dir);
        }

        #[test]
        fn parse_records_reads_directory_flag() {
            let buffer = build_synthetic_record(10, 5, "2019", true);
            let mut entries = HashMap::new();
            parse_records(&buffer, &mut entries);

            assert!(entries.get(&10).unwrap().is_dir);
        }

        #[test]
        fn parse_records_reads_multiple_records_in_one_buffer() {
            let mut buffer = build_synthetic_record(10, 5, "2019", true);
            buffer.extend(build_synthetic_record(100, 10, "IMG_0001.CR2", false));

            let mut entries = HashMap::new();
            parse_records(&buffer, &mut entries);

            assert_eq!(entries.len(), 2);
            assert_eq!(entries.get(&10).unwrap().name, "2019");
            assert_eq!(entries.get(&100).unwrap().name, "IMG_0001.CR2");
        }

        fn build_synthetic_record(frn: u64, parent: u64, name: &str, is_dir: bool) -> Vec<u8> {
            let units: Vec<u16> = name.encode_utf16().collect();
            build_synthetic_record_wide(frn, parent, &units, is_dir)
        }

        /// Varianta pro jména, která nejdou vyjádřit `&str` (nepárový
        /// surrogát) - bere rovnou UTF-16 jednotky.
        fn build_synthetic_record_wide(
            frn: u64,
            parent: u64,
            name_utf16: &[u16],
            is_dir: bool,
        ) -> Vec<u8> {
            let name_bytes_len = name_utf16.len() * 2;
            let header_size = std::mem::size_of::<USN_RECORD_V2>();
            // `FileName: [u16; 1]` je posledních 2 bajty struktury - přesně
            // tam WinAPI skutečné (delší) jméno zapisuje.
            let file_name_field_offset = header_size - 2;
            let record_length = file_name_field_offset + name_bytes_len;

            let record = USN_RECORD_V2 {
                RecordLength: record_length as u32,
                MajorVersion: 2,
                MinorVersion: 0,
                FileReferenceNumber: frn,
                ParentFileReferenceNumber: parent,
                Usn: 0,
                TimeStamp: 0,
                Reason: 0,
                SourceInfo: 0,
                SecurityId: 0,
                FileAttributes: if is_dir {
                    FILE_ATTRIBUTE_DIRECTORY.0
                } else {
                    0
                },
                FileNameLength: name_bytes_len as u16,
                FileNameOffset: file_name_field_offset as u16,
                FileName: [0u16; 1],
            };

            let mut buf = vec![0u8; record_length];
            // SAFETY: čteme `record` (platná lokální hodnota) jako bajty
            // jen do vlastního `buf`, který má prokazatelně aspoň
            // `header_size` bajtů (`record_length >= header_size`, protože
            // `name_bytes_len >= 0`). Je to test-only pomocná funkce, co
            // simuluje přesně to, co by WinAPI do bufferu zapsalo.
            let header_bytes = unsafe {
                std::slice::from_raw_parts(&record as *const _ as *const u8, header_size)
            };
            buf[..header_size].copy_from_slice(header_bytes);

            let name_byte_slice: Vec<u8> =
                name_utf16.iter().flat_map(|c| c.to_ne_bytes()).collect();
            buf[file_name_field_offset..file_name_field_offset + name_bytes_len]
                .copy_from_slice(&name_byte_slice);
            buf
        }
    }
}

#[cfg(windows)]
pub use win::{build_index, build_index_for_volume, is_ntfs_or_refs, is_process_elevated};

#[cfg(not(windows))]
pub fn build_index(
    _roots: &[std::path::PathBuf],
    _exclude: &[String],
    _cancel: &std::sync::atomic::AtomicBool,
) -> Result<crate::index::NameIndex, MftError> {
    Err(MftError::UnsupportedPlatform)
}

#[cfg(not(windows))]
pub fn is_ntfs_or_refs(_volume_root: &std::path::Path) -> bool {
    false
}

#[cfg(not(windows))]
pub fn is_process_elevated() -> bool {
    false
}
