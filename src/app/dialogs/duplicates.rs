//! Dialog: výběr z duplikátů (PROJECT.md §6.2). Otevře se při ruční resolvi
//! nebo když auto-match narazí na remízu (propojení do `panel_files.rs`
//! přijde s M4 úkolem #45).
//!
//! Rozdělení podle stejného vzoru jako `panel_files.rs`: čistá data +
//! testovatelná logika ([`DuplicatePickerState`], [`build`], formátovací
//! funkce) odděleně od `show()`, kterou kvůli `egui::Context` testovat nejde.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use lrcatfix::catalog::extract::CatalogFile;
use lrcatfix::fsops::attrs::{is_cloud_placeholder_path, size_and_modified};
use lrcatfix::fsops::exif_thumbnail::extract_jpeg_thumbnail;
use lrcatfix::fsops::hash::md5_hex;
use lrcatfix::fsops::shell::reveal_in_explorer;
use lrcatfix::matching::score::ScoredCandidate;

/// Zrcadlí `matching::score::MD5_MATCH_BONUS` (soukromá konstanta té
/// knihovny) - tady se používá jen k přeřazení kandidátů v dialogu po
/// dopočítání MD5, ne k perzistentnímu skóre z MATCH. Případný rozjezd obou
/// hodnot by pokazil jen pořadí v tomto dialogu, ne uložené skóre.
const MD5_MATCH_BONUS: i32 = 100;

#[derive(Debug, Clone, PartialEq)]
pub enum Md5State {
    NotComputed,
    Computed {
        hash: String,
        /// `None`, pokud katalog pro tenhle soubor MD5 vůbec nemá (§5.3 -
        /// signál pak není k dispozici, ne mismatch).
        matches_catalog: Option<bool>,
    },
    /// Cloud placeholder (§5.1.1) - hash se záměrně nespočítal, aby se
    /// nespustilo stažení (CLAUDE.md invariant 8).
    CloudPlaceholder,
    Error(String),
}

#[derive(Debug, Clone)]
pub struct CandidateRow {
    pub path: PathBuf,
    pub score: i32,
    pub size_bytes: Option<u64>,
    pub modified: Option<SystemTime>,
    pub md5: Md5State,
}

impl CandidateRow {
    /// Skóre pro řazení v dialogu - `score` z MATCH plus bonus za shodu MD5,
    /// pokud už je spočítaná (viz [`MD5_MATCH_BONUS`]).
    pub fn effective_score(&self) -> i32 {
        match &self.md5 {
            Md5State::Computed {
                matches_catalog: Some(true),
                ..
            } => self.score + MD5_MATCH_BONUS,
            _ => self.score,
        }
    }
}

#[derive(Clone)]
pub struct DuplicatePickerState {
    pub file_id: i64,
    pub filename: String,
    pub expected_path: PathBuf,
    /// Klíč složky (`panel_tree::folder_key`) - pro "použít stejnou volbu
    /// pro zbytek souborů ze stejné složky" (§6.2), řeší M4 úkol #45.
    pub folder_key: String,
    pub catalog_md5: Option<String>,
    /// Seřazeno sestupně podle [`CandidateRow::effective_score`].
    pub candidates: Vec<CandidateRow>,
    pub selected_path: Option<PathBuf>,
    pub apply_to_rest_of_folder: bool,
    pub md5_error: Option<String>,
    /// Náhled vybraného kandidáta (§6.2 "Náhled: vložený EXIF thumbnail") -
    /// `None` dokud uživatel neklikne na "Náhled". Nastavuje se přímo v
    /// `show()` (potřebuje `egui::Context` pro `load_texture`, nejde ho
    /// proto vyrobit v testovatelné čisté funkci jako zbytek modulu).
    pub preview: Option<Result<egui::TextureHandle, String>>,
}

/// Akce, kterou uživatel v dialogu potvrdil - `None` z [`show`] znamená, že
/// dialog je pořád otevřený a čeká.
#[derive(Debug, Clone, PartialEq)]
pub enum DialogAction {
    UseSelected {
        path: PathBuf,
        apply_to_rest_of_folder: bool,
    },
    Skip {
        apply_to_rest_of_folder: bool,
    },
    SkipAll,
    Cancel,
}

/// Postaví stav dialogu pro jeden soubor. Načte velikost/čas změny každého
/// kandidáta (levné, jen metadata - viz `fsops::attrs::size_and_modified`);
/// MD5 se nepočítá, dokud uživatel neklikne na "Porovnat MD5 všech"
/// ([`compute_all_md5`]).
pub fn build(
    file: &CatalogFile,
    candidates: &[ScoredCandidate],
    folder_key: String,
) -> DuplicatePickerState {
    let rows = candidates
        .iter()
        .map(|c| {
            let (size_bytes, modified) = size_and_modified(&c.path).unzip();
            CandidateRow {
                path: c.path.clone(),
                score: c.score,
                size_bytes,
                modified,
                md5: Md5State::NotComputed,
            }
        })
        .collect();

    DuplicatePickerState {
        file_id: file.file_id,
        filename: file.filename.clone(),
        expected_path: file.expected_path.clone(),
        folder_key,
        catalog_md5: file.md5.clone(),
        candidates: rows,
        selected_path: None,
        apply_to_rest_of_folder: false,
        md5_error: None,
        preview: None,
    }
}

fn compute_md5_for_candidate(path: &Path, catalog_md5: Option<&str>) -> Md5State {
    if is_cloud_placeholder_path(path) {
        return Md5State::CloudPlaceholder;
    }
    match md5_hex(path) {
        Ok(hash) => {
            let matches_catalog = catalog_md5.map(|c| c.eq_ignore_ascii_case(&hash));
            Md5State::Computed {
                hash,
                matches_catalog,
            }
        }
        Err(e) => Md5State::Error(e.to_string()),
    }
}

/// "Porovnat MD5 všech" (§6.2) - dopočítá hash pro kandidáty, kteří ho ještě
/// nemají, a přeřadí je podle [`CandidateRow::effective_score`]. Blokující -
/// volající (M4 úkol #45) ji musí spustit mimo UI vlákno stejným vzorem jako
/// `AppState::start_apply_root_remap` (CLAUDE.md „Souběžnost": hashování
/// i pár desítek RAW souborů přes síť může trvat vteřiny).
pub fn compute_all_md5(state: &mut DuplicatePickerState) {
    let catalog_md5 = state.catalog_md5.clone();
    for row in &mut state.candidates {
        if row.md5 == Md5State::NotComputed {
            row.md5 = compute_md5_for_candidate(&row.path, catalog_md5.as_deref());
        }
    }
    resort_by_effective_score(state);
}

pub fn resort_by_effective_score(state: &mut DuplicatePickerState) {
    state
        .candidates
        .sort_by_key(|c| std::cmp::Reverse(c.effective_score()));
}

/// PROJECT.md §6.3: "Otevřít v Průzkumníku" patří do každého dialogu.
/// Chyba se ukládá do `md5_error` jen jako obecný komunikační kanál dialogu -
/// ať uživatel vidí, že se něco nepovedlo, i mimo vlastní MD5 tlačítko.
pub fn open_selected_in_explorer(state: &mut DuplicatePickerState) {
    let Some(path) = state.selected_path.clone() else {
        return;
    };
    if let Err(e) = reveal_in_explorer(&path) {
        state.md5_error = Some(e.to_string());
    }
}

/// Vytáhne vložený EXIF náhled z `path` a dekóduje ho na pole pixelů -
/// čistá (testovatelná) část práce; nahrání do GPU textury dělá až
/// [`load_preview`], protože k tomu potřebuje `egui::Context`.
fn decode_preview_image(path: &Path) -> Result<egui::ColorImage, String> {
    let jpeg_bytes = extract_jpeg_thumbnail(path).map_err(|e| e.to_string())?;
    let decoded = image::load_from_memory(&jpeg_bytes).map_err(|e| e.to_string())?;
    let rgba = decoded.to_rgba8();
    let size = [rgba.width() as usize, rgba.height() as usize];
    Ok(egui::ColorImage::from_rgba_unmultiplied(
        size,
        rgba.as_raw(),
    ))
}

/// "Náhled" (§6.2) - nahraje dekódovaný EXIF thumbnail vybraného kandidáta
/// jako GPU texturu. Volá se synchronně z `show()` (na jedno kliknutí, ne
/// na pozadí) - vestavěné EXIF náhledy jsou v řádu jednotek až nízkých
/// desítek kB, dekódování je otázka milisekund, ne dlouhá operace ve
/// smyslu CLAUDE.md „Souběžnost".
pub fn load_preview(ctx: &egui::Context, state: &mut DuplicatePickerState) {
    let Some(path) = state.selected_path.clone() else {
        return;
    };
    state.preview = Some(match decode_preview_image(&path) {
        Ok(color_image) => {
            let texture = ctx.load_texture(
                format!("duplicate-preview-{}", state.file_id),
                color_image,
                egui::TextureOptions::default(),
            );
            Ok(texture)
        }
        Err(e) => Err(e),
    });
}

/// Velikost v Kč-lokálním formátu (desetinná čárka), např. `24,1 MB`
/// (PROJECT.md §6.2). Celé bajty pod 1 KB se zobrazují bez desetinné části.
pub fn format_size(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b < KB {
        return format!("{bytes} B");
    }
    let (value, unit) = if b >= GB {
        (b / GB, "GB")
    } else if b >= MB {
        (b / MB, "MB")
    } else {
        (b / KB, "KB")
    };
    format!("{value:.1} {unit}").replace('.', ",")
}

/// Datum poslední změny, jen den (PROJECT.md §6.2 mockup neukazuje čas).
pub fn format_modified(t: SystemTime) -> String {
    let datetime: chrono::DateTime<chrono::Local> = t.into();
    datetime.format("%Y-%m-%d").to_string()
}

/// Vykreslí modální okno (§6.2). Vrací `Some(akce)`, jakmile uživatel
/// potvrdí/zruší - volající pak `state` zahodí/aplikuje podle vrácené akce.
pub fn show(ctx: &egui::Context, state: &mut DuplicatePickerState) -> Option<DialogAction> {
    let mut action = None;
    let mut open = true;

    egui::Window::new(format!("Vyber správný soubor: {}", state.filename))
        .collapsible(false)
        .resizable(true)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!("Očekáváno: {}", state.expected_path.display()));
            ui.add_space(4.0);

            egui::Grid::new(("duplicate_picker_grid", state.file_id))
                .striped(true)
                .num_columns(5)
                .show(ui, |ui| {
                    ui.strong("");
                    ui.strong("Cesta");
                    ui.strong("Velikost");
                    ui.strong("Změněno");
                    ui.strong("Skóre / MD5");
                    ui.end_row();

                    for row in &state.candidates {
                        let is_selected =
                            state.selected_path.as_deref() == Some(row.path.as_path());
                        if ui.radio(is_selected, "").clicked() {
                            state.selected_path = Some(row.path.clone());
                            // Náhled patřil předchozímu výběru - u nového
                            // kandidáta by byl matoucí, dokud si uživatel
                            // znovu neklikne na "Náhled".
                            state.preview = None;
                        }
                        ui.label(row.path.display().to_string());
                        ui.label(
                            row.size_bytes
                                .map(format_size)
                                .unwrap_or_else(|| "—".to_string()),
                        );
                        ui.label(
                            row.modified
                                .map(format_modified)
                                .unwrap_or_else(|| "—".to_string()),
                        );
                        ui.label(md5_column_text(row));
                        ui.end_row();
                    }
                });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(state.selected_path.is_some(), egui::Button::new("Náhled"))
                    .clicked()
                {
                    load_preview(ctx, state);
                }
                if ui
                    .add_enabled(
                        state.selected_path.is_some(),
                        egui::Button::new("Otevřít v Průzkumníku"),
                    )
                    .clicked()
                {
                    open_selected_in_explorer(state);
                }
                if ui.button("Porovnat MD5 všech").clicked() {
                    compute_all_md5(state);
                }
            });
            if let Some(err) = &state.md5_error {
                ui.colored_label(egui::Color32::from_rgb(0xd6, 0x33, 0x33), err);
            }
            match &state.preview {
                Some(Ok(texture)) => {
                    let max_size = egui::vec2(240.0, 240.0);
                    let native_size = texture.size_vec2();
                    let scale = (max_size.x / native_size.x)
                        .min(max_size.y / native_size.y)
                        .min(1.0);
                    ui.add(egui::Image::new((texture.id(), native_size * scale)));
                }
                Some(Err(err)) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xd9, 0x8a, 0x1f),
                        format!("Náhled se nepovedl: {err}"),
                    );
                }
                None => {}
            }

            ui.add_space(6.0);
            ui.checkbox(
                &mut state.apply_to_rest_of_folder,
                "Použít stejnou volbu pro zbytek souborů ze stejné složky",
            );

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        state.selected_path.is_some(),
                        egui::Button::new("Použít vybraný"),
                    )
                    .clicked()
                {
                    // `app/` nesmí panikařit nikdy (CLAUDE.md §8) - tlačítko
                    // je sice `add_enabled` jen s výběrem, ale radši tichý
                    // no-op při nesrovnalosti stavu než pád GUI vlákna.
                    if let Some(path) = state.selected_path.clone() {
                        action = Some(DialogAction::UseSelected {
                            path,
                            apply_to_rest_of_folder: state.apply_to_rest_of_folder,
                        });
                    }
                }
                if ui.button("Přeskočit").clicked() {
                    action = Some(DialogAction::Skip {
                        apply_to_rest_of_folder: state.apply_to_rest_of_folder,
                    });
                }
                if ui.button("Přeskočit vše").clicked() {
                    action = Some(DialogAction::SkipAll);
                }
                if ui.button("Zrušit").clicked() {
                    action = Some(DialogAction::Cancel);
                }
            });
        });

    if action.is_none() && !open {
        action = Some(DialogAction::Cancel);
    }
    action
}

fn md5_column_text(row: &CandidateRow) -> String {
    match &row.md5 {
        Md5State::NotComputed => row.score.to_string(),
        Md5State::CloudPlaceholder => format!("{} (cloud - nelze porovnat)", row.score),
        Md5State::Error(e) => format!("{} (MD5 chyba: {e})", row.score),
        Md5State::Computed {
            hash,
            matches_catalog,
        } => match matches_catalog {
            Some(true) => format!("{} ✓ MD5 shoda ({hash})", row.effective_score()),
            Some(false) => format!("{} ✗ MD5 neshoda ({hash})", row.score),
            None => format!("{} (MD5 {hash}, katalog MD5 nemá)", row.score),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn decode_preview_image_round_trip_via_real_jpeg_thumbnail() {
        // Malý skutečný JPEG (2x2 červený čtverec) přes crate `image`.
        let mut jpeg_bytes = Vec::new();
        {
            let img = image::RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0]));
            let mut cursor = std::io::Cursor::new(&mut jpeg_bytes);
            img.write_to(&mut cursor, image::ImageFormat::Jpeg)
                .expect("kódování testovacího JPEG nesmí selhat");
        }

        // Zabalí ho jako EXIF thumbnail (IFD1) - stejný postup jako test v
        // `fsops::exif_thumbnail`, tentokrát se SKUTEČNÝMI JPEG bajty, aby
        // šlo ověřit i dekódování, ne jen extrakci.
        let image_desc = exif::Field {
            tag: exif::Tag::ImageDescription,
            ifd_num: exif::In::PRIMARY,
            value: exif::Value::Ascii(vec![b"test".to_vec()]),
        };
        let mut writer = exif::experimental::Writer::new();
        writer.push_field(&image_desc);
        writer.set_jpeg(&jpeg_bytes, exif::In::THUMBNAIL);
        let mut tiff_buf = std::io::Cursor::new(Vec::new());
        writer
            .write(&mut tiff_buf, false)
            .expect("Writer::write nesmí selhat pro platná data");

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("synteticky.cr2");
        std::fs::write(&path, tiff_buf.into_inner()).unwrap();

        let color_image = decode_preview_image(&path).unwrap();
        assert_eq!(color_image.size, [2, 2]);
    }

    #[test]
    fn decode_preview_image_reports_error_for_file_without_exif() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("prazdny.CR2");
        std::fs::write(&path, b"neni to EXIF").unwrap();
        assert!(decode_preview_image(&path).is_err());
    }

    fn candidate(path: &str, score: i32) -> ScoredCandidate {
        ScoredCandidate {
            path: PathBuf::from(path),
            score,
        }
    }

    fn file(id: i64, filename: &str, md5: Option<&str>) -> CatalogFile {
        CatalogFile {
            file_id: id,
            folder_id: 1,
            root_id: 1,
            root_path: "C:/Foto/".to_string(),
            rel_path: "2019/".to_string(),
            filename: filename.to_string(),
            base_name: filename.to_string(),
            extension: String::new(),
            expected_path: PathBuf::from("C:/Foto/2019/").join(filename),
            external_mod_time: None,
            md5: md5.map(str::to_string),
            file_size: None,
            sidecar_extensions: Vec::new(),
        }
    }

    #[test]
    fn build_reads_size_and_modified_for_existing_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("IMG_1234.CR2");
        std::fs::write(&path, b"0123456789").unwrap();

        let f = file(1, "IMG_1234.CR2", None);
        let candidates = vec![candidate(path.to_str().unwrap(), 92)];
        let state = build(&f, &candidates, "1/2019".to_string());

        assert_eq!(state.candidates.len(), 1);
        assert_eq!(state.candidates[0].size_bytes, Some(10));
        assert!(state.candidates[0].modified.is_some());
        assert_eq!(state.candidates[0].md5, Md5State::NotComputed);
    }

    #[test]
    fn build_leaves_metadata_none_for_missing_candidate() {
        let f = file(1, "IMG_1234.CR2", None);
        let candidates = vec![candidate(r"D:\neexistuje\IMG_1234.CR2", 50)];
        let state = build(&f, &candidates, "1/2019".to_string());

        assert_eq!(state.candidates[0].size_bytes, None);
        assert_eq!(state.candidates[0].modified, None);
    }

    #[test]
    fn compute_all_md5_marks_match_against_catalog_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.bin");
        std::fs::write(&path, b"hello world").unwrap();

        let f = file(1, "a.bin", Some("5eb63bbbe01eeed093cb22bb8f5acdc3"));
        let candidates = vec![candidate(path.to_str().unwrap(), 40)];
        let mut state = build(&f, &candidates, "1/2019".to_string());

        compute_all_md5(&mut state);

        assert_eq!(
            state.candidates[0].md5,
            Md5State::Computed {
                hash: "5eb63bbbe01eeed093cb22bb8f5acdc3".to_string(),
                matches_catalog: Some(true),
            }
        );
        assert_eq!(state.candidates[0].effective_score(), 40 + MD5_MATCH_BONUS);
    }

    #[test]
    fn compute_all_md5_marks_mismatch_without_bonus() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.bin");
        std::fs::write(&path, b"something else").unwrap();

        let f = file(1, "a.bin", Some("5eb63bbbe01eeed093cb22bb8f5acdc3"));
        let candidates = vec![candidate(path.to_str().unwrap(), 40)];
        let mut state = build(&f, &candidates, "1/2019".to_string());

        compute_all_md5(&mut state);

        assert_eq!(
            state.candidates[0].md5,
            Md5State::Computed {
                hash: md5_hex(&path).unwrap(),
                matches_catalog: Some(false),
            }
        );
        assert_eq!(state.candidates[0].effective_score(), 40);
    }

    #[test]
    fn compute_all_md5_does_not_recompute_already_computed_rows() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.bin");
        std::fs::write(&path, b"v1").unwrap();

        let f = file(1, "a.bin", None);
        let candidates = vec![candidate(path.to_str().unwrap(), 40)];
        let mut state = build(&f, &candidates, "1/2019".to_string());
        state.candidates[0].md5 = Md5State::Error("simulace předchozího výsledku".to_string());

        compute_all_md5(&mut state);

        assert_eq!(
            state.candidates[0].md5,
            Md5State::Error("simulace předchozího výsledku".to_string())
        );
    }

    #[test]
    fn resort_by_effective_score_moves_md5_match_to_front() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.bin");
        let b = tmp.path().join("b.bin");
        std::fs::write(&a, b"hello world").unwrap();
        std::fs::write(&b, b"other content").unwrap();

        let f = file(1, "a.bin", Some("5eb63bbbe01eeed093cb22bb8f5acdc3"));
        // `b` má vyšší MATCH skóre, ale nehoduje MD5 - po dopočítání MD5 musí
        // `a` (nižší MATCH skóre, ale MD5 shoda) skončit první.
        let candidates = vec![
            candidate(b.to_str().unwrap(), 80),
            candidate(a.to_str().unwrap(), 40),
        ];
        let mut state = build(&f, &candidates, "1/2019".to_string());
        assert_eq!(state.candidates[0].path, b);

        compute_all_md5(&mut state);

        assert_eq!(state.candidates[0].path, a);
    }

    #[test]
    fn format_size_uses_comma_decimal_separator() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1536), "1,5 KB");
        assert_eq!(format_size(1_572_864), "1,5 MB");
    }

    #[test]
    fn format_modified_renders_date_only() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
        let s = format_modified(t);
        assert_eq!(s.len(), 10);
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
    }

    #[test]
    fn effective_score_ignores_mismatch_and_cloud_and_error() {
        let row_mismatch = CandidateRow {
            path: PathBuf::from("x"),
            score: 10,
            size_bytes: None,
            modified: None,
            md5: Md5State::Computed {
                hash: "x".to_string(),
                matches_catalog: Some(false),
            },
        };
        assert_eq!(row_mismatch.effective_score(), 10);

        let row_cloud = CandidateRow {
            md5: Md5State::CloudPlaceholder,
            ..row_mismatch.clone()
        };
        assert_eq!(row_cloud.effective_score(), 10);

        let row_error = CandidateRow {
            md5: Md5State::Error("x".to_string()),
            ..row_mismatch.clone()
        };
        assert_eq!(row_error.effective_score(), 10);
    }
}
