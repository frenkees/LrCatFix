//! Dolní panel: návrhy z fáze CLUSTER (PROJECT.md §6, §7).
//!
//! Vzor D (§7.4 - sonda + odvození z MATCH) a vzor A (§7.1, FOLDER_MOVED)
//! mají akční tlačítko s potvrzovacím dialogem. Vzor B (§7.2, SCATTERED) je
//! od M5 jen informativní karta - fyzická konsolidace vyžaduje infrastrukturu
//! skupinových přesunů (§3.5), která přichází až s M6. Vzor C
//! (ALBUM_SCATTERED, §7.3) taky patří do M6.
//!
//! Tlačítka se aktivují jen v režimu ZÁPIS POVOLEN a samy nic nezapíší - jen
//! otevřou potvrzovací dialog (CLAUDE.md §11: „Žádné tlačítko nesmí vyvolat
//! nevratnou operaci na jedno kliknutí bez souhrnu toho, co se stane").

use lrcatfix::cluster;

use crate::app::state::AppState;

pub fn show(ui: &mut egui::Ui, state: &mut AppState) {
    let Some(loaded) = &state.loaded else {
        return;
    };

    if loaded.remap_suggestions.is_empty()
        && loaded.folder_moved_suggestions.is_empty()
        && loaded.scattered_suggestions.is_empty()
        && loaded.album_scattered_suggestions.is_empty()
    {
        ui.label("Žádné návrhy.");
        return;
    }

    let remap_suggestions = loaded.remap_suggestions.clone();
    let folder_moved_suggestions = loaded.folder_moved_suggestions.clone();
    let scattered_suggestions = loaded.scattered_suggestions.clone();
    let album_scattered_suggestions = loaded.album_scattered_suggestions.clone();
    let write_mode = state.write_mode;
    let busy = state.applying || state.loading;

    egui::ScrollArea::vertical().id_salt("plans_scroll").max_height(160.0).show(ui, |ui| {
        for s in &remap_suggestions {
            ui.group(|ui| {
                ui.label(format!(
                    "⚠ Root {}: „{}\u{201c} nenalezen, ale {}/{} vzorků odpovídá „{}\u{201c} ({:.0} %)",
                    s.root_id,
                    s.original_absolute_path,
                    s.matched_samples,
                    s.total_samples,
                    s.proposed_absolute_path,
                    s.ratio() * 100.0
                ));
                if write_mode {
                    if ui.add_enabled(!busy, egui::Button::new("Přepsat cestu v katalogu")).clicked() {
                        state.pending_remap = Some(s.clone());
                    }
                } else {
                    ui.label(write_mode_hint());
                }
            });
        }

        for s in &folder_moved_suggestions {
            ui.group(|ui| {
                ui.label(format!(
                    "⚠ Složka „{}\u{201c}: {}/{} souborů nalezeno v „{}\u{201c}",
                    cluster::display_source_path(&s.source_rel_path),
                    s.matched_in_target,
                    s.with_candidate,
                    s.target_dir.display()
                ));
                if s.target_name_differs {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xd9, 0x8a, 0x1f),
                        "⚠ Cílová složka má jiný název - ověř, že jde o tutéž kolekci fotek.",
                    );
                }
                if write_mode {
                    ui.horizontal(|ui| {
                        if ui.add_enabled(!busy, egui::Button::new("Přepsat cestu v katalogu")).clicked() {
                            state.pending_folder_repoint = Some(s.clone());
                        }
                        if ui.add_enabled(!busy, egui::Button::new("Přesunout soubory zpět")).clicked() {
                            state.pending_folder_move_back = Some(s.clone());
                        }
                    });
                } else {
                    ui.label(write_mode_hint());
                }
            });
        }

        for s in &scattered_suggestions {
            ui.group(|ui| {
                ui.label(format!(
                    "⚠ Složka „{}\u{201c}: {} souborů rozházeno v {} různých složkách (medián {:.1} na cíl)",
                    cluster::display_source_path(&s.source_rel_path), s.with_candidate, s.distinct_targets, s.median_per_target
                ));
                if write_mode {
                    if ui.add_enabled(!busy, egui::Button::new("Konsolidovat zpět do složky")).clicked() {
                        state.pending_consolidate = Some(s.clone());
                    }
                } else {
                    ui.label(write_mode_hint());
                }
            });
        }

        for s in &album_scattered_suggestions {
            ui.group(|ui| {
                ui.label(format!(
                    "⚠ Kolekce „{}\u{201c}: {}/{} fotek chybí, fyzicky rozházeno v {} různých složkách",
                    s.collection_name, s.missing_files, s.total_files, s.distinct_folders
                ));
                ui.label(
                    egui::RichText::new(
                        "Sloučení do jednoho adresáře je nejdestruktivnější operace a vyžaduje \
                         vlastní dialog s dvojím potvrzením - přijde v pozdějším milníku.",
                    )
                    .weak()
                    .italics(),
                );
            });
        }
    });
}

fn write_mode_hint() -> egui::RichText {
    egui::RichText::new("Zapněte „ZÁPIS POVOLEN\u{201c} v horní liště, aby šlo návrh použít.")
        .weak()
        .italics()
}
