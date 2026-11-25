//! Pravý panel: virtualizovaná tabulka souborů (PROJECT.md §6.1, §11 - „Tabulka
//! s 30 000 řádky musí být virtualizovaná (`egui_extras::TableBuilder`).
//! Nikdy nerenderuj všechny řádky.").
//!
//! Sloupec „Kandidát" ukazuje nejlépe obodovaného kandidáta z MATCH (§5.3);
//! „—" znamená, že pro daný soubor INDEX nenašel žádného. Akční tlačítka
//! (Vyřešit/Přeskočit) přijdou až s M4 (dialog duplikátů) - do té doby je
//! tabulka čistě informativní.

use egui_extras::{Column, TableBuilder};

use lrcatfix::catalog::extract::CatalogFile;
use lrcatfix::fsops::paths::match_key_str;
use lrcatfix::fsops::verify::FileStatus;

use crate::app::panel_tree::folder_key;
use crate::app::state::{AppState, LoadedCatalog};

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    ui.horizontal(|ui| {
        ui.label("Filtr:");
        ui.text_edit_singleline(&mut state.filter_text);
        ui.checkbox(&mut state.only_unresolved, "Jen nevyřešené");
    });
    ui.separator();

    let Some(loaded) = &state.loaded else {
        ui.label("Katalog není načtený.");
        return;
    };

    let rows = visible_rows(
        loaded,
        state.selected_folder_key.as_deref(),
        &state.filter_text,
        state.only_unresolved,
    );

    ui.label(format!("{} souborů", rows.len()));

    let mut resolve_clicked: Option<i64> = None;

    TableBuilder::new(ui)
        .vscroll(true)
        .striped(true)
        .column(Column::remainder().at_least(200.0))
        .column(Column::auto().at_least(90.0))
        .column(Column::remainder().at_least(300.0))
        .column(Column::auto().at_least(70.0))
        .column(Column::auto().at_least(50.0))
        .column(Column::auto().at_least(90.0))
        .header(20.0, |mut header| {
            header.col(|ui| {
                ui.strong("Jméno");
            });
            header.col(|ui| {
                ui.strong("Stav");
            });
            header.col(|ui| {
                ui.strong("Očekávaná cesta");
            });
            header.col(|ui| {
                ui.strong("Kandidát");
            });
            header.col(|ui| {
                ui.strong("Skóre");
            });
            header.col(|ui| {
                ui.strong("Akce");
            });
        })
        .body(|body| {
            body.rows(18.0, rows.len(), |mut row| {
                let f = rows[row.index()];
                row.col(|ui| {
                    single_line(ui, &f.filename);
                });
                row.col(|ui| {
                    let (text, color) = status_label(loaded.status_of(f.file_id));
                    ui.colored_label(color, text);
                });
                row.col(|ui| {
                    single_line(ui, &f.expected_path.display().to_string());
                });
                let best = loaded
                    .matches_of(f.file_id)
                    .and_then(|m| m.candidates.first());
                row.col(|ui| match best {
                    Some(c) => {
                        single_line(ui, &c.path.display().to_string());
                    }
                    None => {
                        ui.label("—");
                    }
                });
                row.col(|ui| match best {
                    Some(c) => {
                        ui.label(c.score.to_string());
                    }
                    None => {
                        ui.label("—");
                    }
                });
                row.col(|ui| {
                    render_action_cell(ui, state, f, best.is_some(), &mut resolve_clicked);
                });
            });
        });

    if let Some(id) = resolve_clicked {
        state.open_duplicate_picker(id);
    }
}

fn render_action_cell(
    ui: &mut egui::Ui,
    state: &AppState,
    f: &CatalogFile,
    has_candidates: bool,
    resolve_clicked: &mut Option<i64>,
) {
    if let Some(chosen) = state.resolved_choices.get(&f.file_id) {
        let label = chosen
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| chosen.display().to_string());
        ui.colored_label(
            egui::Color32::from_rgb(0x2e, 0xa0, 0x4a),
            format!("✓ {label}"),
        );
        if ui.small_button("Upravit").clicked() {
            *resolve_clicked = Some(f.file_id);
        }
        return;
    }
    if state.skipped_files.contains(&f.file_id) {
        ui.label("⤫ přeskočeno");
        if ui.small_button("Vyřešit").clicked() {
            *resolve_clicked = Some(f.file_id);
        }
        return;
    }
    if has_candidates {
        if ui.small_button("Vyřešit").clicked() {
            *resolve_clicked = Some(f.file_id);
        }
    } else {
        ui.label("—");
    }
}

fn visible_rows<'a>(
    loaded: &'a LoadedCatalog,
    selected_folder_key: Option<&str>,
    filter_text: &str,
    only_unresolved: bool,
) -> Vec<&'a CatalogFile> {
    let filter_key = match_key_str(filter_text.trim());

    loaded
        .files
        .iter()
        .filter(|f| match selected_folder_key {
            Some(key) => {
                let own_key = folder_key(f.root_id, &f.rel_path);
                own_key == key || own_key.starts_with(&format!("{key}/"))
            }
            None => true,
        })
        .filter(|f| filter_key.is_empty() || match_key_str(&f.filename).contains(&filter_key))
        .filter(|f| !only_unresolved || loaded.status_of(f.file_id) != FileStatus::Ok)
        .collect()
}

/// Text na JEDEN řádek, useknutý na šířku sloupce, s celou hodnotou
/// v tooltipu.
///
/// Bez tohohle egui dlouhou cestu zalomí a jeden soubor pak zabere v tabulce
/// klidně pět řádků - u 30 000 řádků se v tom nedá orientovat. Zároveň to
/// drží řádky stejně vysoké, což virtualizace (`body.rows`) předpokládá.
fn single_line(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(text)
            .wrap_mode(egui::TextWrapMode::Truncate)
            .selectable(false),
    )
    .on_hover_text(text);
}

fn status_label(status: FileStatus) -> (&'static str, egui::Color32) {
    match status {
        FileStatus::Ok => ("✓ ok", egui::Color32::from_rgb(0x2e, 0xa0, 0x4a)),
        FileStatus::Missing => ("✗ chybí", egui::Color32::from_rgb(0xd9, 0x8a, 0x1f)),
        FileStatus::FolderMissing => ("✗ složka chybí", egui::Color32::from_rgb(0xd6, 0x33, 0x33)),
        // Modrá, ne červená/oranžová - tohle NENÍ chyba katalogu ani ztracený
        // soubor, jen nedostupný svazek. Uživatel to musí odlišit na první
        // pohled, aby nehledal opravu tam, kde stačí připojit disk.
        FileStatus::FolderUnavailable => (
            "⚠ disk nedostupný",
            egui::Color32::from_rgb(0x1f, 0x6f, 0xd9),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use lrcatfix::catalog::extract::RootFolder;
    use lrcatfix::catalog::open::LockStatus;
    use lrcatfix::index::IndexBackend;
    use lrcatfix::matching::{calibrate::Calibration, MatchRun};

    use super::*;

    fn file(id: i64, rel_path: &str, filename: &str) -> CatalogFile {
        CatalogFile {
            file_id: id,
            folder_id: 1,
            root_id: 1,
            root_path: "C:/Foto/".to_string(),
            rel_path: rel_path.to_string(),
            filename: filename.to_string(),
            base_name: filename.to_string(),
            extension: String::new(),
            expected_path: PathBuf::from(filename),
            external_mod_time: None,
            md5: None,
            file_size: None,
            sidecar_extensions: Vec::new(),
        }
    }

    fn loaded(files: Vec<CatalogFile>, statuses: &[(i64, FileStatus)]) -> LoadedCatalog {
        LoadedCatalog {
            catalog_path: PathBuf::new(),
            lock_status: LockStatus::default(),
            roots: vec![RootFolder {
                root_id: 1,
                absolute_path: "C:/Foto/".to_string(),
                name: "Foto".to_string(),
            }],
            files,
            statuses: statuses.iter().copied().collect::<HashMap<_, _>>(),
            remap_suggestions: Vec::new(),
            folder_moved_suggestions: Vec::new(),
            scattered_suggestions: Vec::new(),
            album_scattered_suggestions: Vec::new(),
            match_run: MatchRun {
                index_backend: IndexBackend::Walk,
                index_size: 0,
                calibration: Calibration::disabled(),
                matches: HashMap::new(),
            },
        }
    }

    #[test]
    fn folder_filter_includes_subfolders() {
        let data = loaded(
            vec![file(1, "2019/HR/", "a.CR2"), file(2, "2020/", "b.CR2")],
            &[(1, FileStatus::Ok), (2, FileStatus::Ok)],
        );
        let rows = visible_rows(&data, Some("1/2019"), "", false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_id, 1);
    }

    #[test]
    fn text_filter_matches_case_insensitively() {
        let data = loaded(
            vec![file(1, "", "Plitvička.CR2"), file(2, "", "Other.CR2")],
            &[],
        );
        let rows = visible_rows(&data, None, "PLITVIČKA", false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_id, 1);
    }

    #[test]
    fn only_unresolved_hides_ok_files() {
        let data = loaded(
            vec![file(1, "", "a.CR2"), file(2, "", "b.CR2")],
            &[(1, FileStatus::Ok), (2, FileStatus::Missing)],
        );
        let rows = visible_rows(&data, None, "", true);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_id, 2);
    }
}
