//! `app/` - JEN UI (CLAUDE.md §5). Žádné SQL, žádné přímé I/O - jen volání
//! funkcí z `catalog/`, `fsops/`, `cluster/` (přes `lrcatfix::...`) a
//! vykreslování přes `egui`.

pub mod dialogs;
pub mod panel_files;
pub mod panel_plans;
pub mod panel_tree;
pub mod state;
pub mod widgets;

use std::path::PathBuf;

use lrcatfix::fsops::verify::FileStatus;

use dialogs::duplicates;
use state::AppState;

pub struct LrCatFixApp {
    state: AppState,
    tree_cache: Vec<panel_tree::FolderNode>,
}

impl LrCatFixApp {
    pub fn new(initial_catalog: Option<PathBuf>) -> Self {
        let state = AppState::new(initial_catalog);
        let tree_cache = build_tree_cache(&state);
        Self { state, tree_cache }
    }
}

fn build_tree_cache(state: &AppState) -> Vec<panel_tree::FolderNode> {
    state
        .loaded
        .as_ref()
        .map(panel_tree::build_tree)
        .unwrap_or_default()
}

impl eframe::App for LrCatFixApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.state.poll() {
            self.tree_cache = build_tree_cache(&self.state);
        }
        if self.state.poll_apply() {
            self.tree_cache = build_tree_cache(&self.state);
        }
        if self.state.loading || self.state.applying {
            // Vlákno běží na pozadí a nemá jak samo probudit UI smyčku -
            // dokud se nedokončí, žádáme si repaint každý frame.
            ctx.request_repaint();
        }

        show_toolbar(ctx, &mut self.state);
        show_status_bar(ctx, &self.state);
        show_confirmation_dialog(ctx, &mut self.state);
        show_folder_repoint_confirmation_dialog(ctx, &mut self.state);
        show_folder_move_back_confirmation_dialog(ctx, &mut self.state);
        show_consolidate_confirmation_dialog(ctx, &mut self.state);
        show_duplicate_picker_dialog(ctx, &mut self.state);
        show_resolved_confirmation_dialog(ctx, &mut self.state);

        egui::TopBottomPanel::bottom("plans_panel")
            .min_height(60.0)
            .show(ctx, |ui| {
                panel_plans::show(ui, &mut self.state);
            });

        egui::SidePanel::left("tree_panel")
            .resizable(true)
            .default_width(260.0)
            .show(ctx, |ui| {
                panel_tree::show(ui, &self.tree_cache, &mut self.state.selected_folder_key);
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            panel_files::show(ui, &mut self.state);
        });
    }
}

fn show_toolbar(ctx: &egui::Context, state: &mut AppState) {
    egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
        // `horizontal_wrapped` místo `horizontal`: v úzkém okně se prvky
        // zalomí na další řádek místo aby zmizely za okrajem.
        ui.horizontal_wrapped(|ui| {
            ui.label("Katalog:");
            // Textové pole si nesmí ukousnout celou šířku, jinak se tlačítka
            // za ním vytlačí mimo okno.
            let field_width = (ui.available_width() - 320.0).clamp(160.0, 420.0);
            ui.add(
                egui::TextEdit::singleline(&mut state.catalog_path_input)
                    .desired_width(field_width)
                    .hint_text("cesta k .lrcat, nebo použijte Procházet…"),
            );

            // Systémový dialog (§6.1) - dosud šlo cestu jen napsat ručně.
            if ui
                .add_enabled(!state.loading, egui::Button::new("📂 Procházet…"))
                .on_hover_text("Vybrat soubor .lrcat v systémovém dialogu")
                .clicked()
            {
                let current = {
                    let t = state.catalog_path_input.trim();
                    if t.is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(t))
                    }
                };
                if let Some(picked) = lrcatfix::fsops::dialog::pick_catalog(current.as_deref()) {
                    state.catalog_path_input = picked.display().to_string();
                    // Výběr souboru je jednoznačný záměr - načti rovnou, ať
                    // uživatel nemusí klikat podruhé.
                    state.start_load(picked);
                }
            }

            let open_clicked = ui.button("Otevřít katalog").clicked();
            let reload_clicked = ui
                .add_enabled(
                    state.loaded.is_some() && !state.loading,
                    egui::Button::new("Ověřit znovu"),
                )
                .clicked();

            if !state.loading && (open_clicked || reload_clicked) {
                let trimmed = state.catalog_path_input.trim();
                if trimmed.is_empty() {
                    state.error = Some("zadejte cestu ke katalogu".to_string());
                } else {
                    state.start_load(PathBuf::from(trimmed));
                }
            }

            if state.loading && ui.button("Zrušit").clicked() {
                state.cancel_loading();
            }

            if !state.elevated {
                ui.separator();
                let restart_response = ui
                    .add_enabled(
                        !state.loading && !state.applying,
                        egui::Button::new("Rychlý sken disku (vyžaduje správce) 🛡"),
                    )
                    .on_hover_text(
                        "Restartuje aplikaci s právy správce (Windows UAC), aby šlo použít rychlejší \
                         MFT sken disku místo procházení složek. Rozdělaná práce (otevřený katalog) \
                         se po restartu obnoví.",
                    );
                if restart_response.clicked() {
                    state.relaunch_elevated();
                }
            }

            if !state.resolved_choices.is_empty() {
                ui.separator();
                let busy = state.applying || state.loading;
                if state.write_mode {
                    if ui
                        .add_enabled(
                            !busy,
                            egui::Button::new(format!(
                                "Použít vyřešené ({})",
                                state.resolved_choices.len()
                            )),
                        )
                        .clicked()
                    {
                        state.confirm_apply_resolved = true;
                    }
                } else {
                    ui.label(format!(
                        "{} vyřešeno - zapněte ZÁPIS POVOLEN pro zápis",
                        state.resolved_choices.len()
                    ));
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Zrušit vyřešení"))
                    .clicked()
                {
                    state.resolved_choices.clear();
                    state.skipped_files.clear();
                    state.sticky_folder_choice.clear();
                }
            }

            ui.separator();
            // CLAUDE.md §11: „Uživatel nikdy nesmí být na pochybách, jestli
            // právě mění data" - proto posuvný přepínač s výraznou barvou,
            // ne zaškrtávací políčko (to má v obou stavech stejný tvar).
            let write_color = egui::Color32::from_rgb(0xd6, 0x33, 0x33);
            let preview_color = egui::Color32::from_rgb(0x1f, 0x6f, 0xd9);
            widgets::toggle_switch(ui, &mut state.write_mode, write_color).on_hover_text(
                "NÁHLED: nástroj do katalogu nezapíše nic.\n\
                 ZÁPIS POVOLEN: akce mohou měnit katalog a přesouvat soubory \
                 (vždy až po potvrzení souhrnu).",
            );
            if state.write_mode {
                ui.label(
                    egui::RichText::new("ZÁPIS POVOLEN")
                        .color(write_color)
                        .strong(),
                );
            } else {
                ui.label(
                    egui::RichText::new("NÁHLED")
                        .color(preview_color)
                        .strong(),
                );
            }
        });
    });
}

/// Modální potvrzovací dialog (CLAUDE.md §11: „Žádné tlačítko nesmí vyvolat
/// nevratnou operaci na jedno kliknutí bez souhrnu toho, co se stane").
fn show_confirmation_dialog(ctx: &egui::Context, state: &mut AppState) {
    let Some(suggestion) = state.pending_remap.clone() else {
        return;
    };

    let mut open = true;
    let mut confirmed = false;
    let mut cancelled = false;

    egui::Window::new("Potvrdit přepsání cesty v katalogu")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!("Root {}: „{}\u{201c}", suggestion.root_id, suggestion.original_absolute_path));
            ui.label(format!("→ nová cesta: „{}\u{201c}", suggestion.proposed_absolute_path));
            ui.label(format!(
                "Shoda vzorků: {}/{} ({:.0} %)",
                suggestion.matched_samples,
                suggestion.total_samples,
                suggestion.ratio() * 100.0
            ));
            ui.separator();
            ui.label("Co se stane: 1 řádek v databázi (AgLibraryRootFolder), 0 přesunů souborů na disku.");
            ui.label(
                egui::RichText::new("Před zápisem se automaticky vytvoří ověřená záloha katalogu.").weak(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Potvrdit a zapsat").clicked() {
                    confirmed = true;
                }
                if ui.button("Zrušit").clicked() {
                    cancelled = true;
                }
            });
        });

    if confirmed {
        state.pending_remap = None;
        state.start_apply_root_remap(suggestion);
    } else if cancelled || !open {
        state.pending_remap = None;
    }
}

/// Potvrzovací dialog pro vzor A (§7.1) - stejný vzor jako
/// [`show_confirmation_dialog`]. Souhrn počítá jen s levnou variantou
/// (repoint jedním UPDATE); pokud cílová složka mezitím v katalogu existuje
/// jako jiná, `apply_folder_repoint` sama spadne na N× `UpdateFileFolder` -
/// to `apply_success`/`apply_error` v status baru ohlásí až po zápisu.
fn show_folder_repoint_confirmation_dialog(ctx: &egui::Context, state: &mut AppState) {
    let Some(suggestion) = state.pending_folder_repoint.clone() else {
        return;
    };

    let mut open = true;
    let mut confirmed = false;
    let mut cancelled = false;

    egui::Window::new("Potvrdit přepsání cesty složky v katalogu")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!("Složka „{}\u{201c}", suggestion.source_rel_path));
            ui.label(format!("→ nová cesta: „{}\u{201c}", suggestion.target_dir.display()));
            ui.label(format!(
                "Shoda: {}/{} souborů se svým nejlepším kandidátem v cílové složce",
                suggestion.matched_in_target, suggestion.with_candidate
            ));
            if suggestion.target_name_differs {
                ui.colored_label(
                    egui::Color32::from_rgb(0xd9, 0x8a, 0x1f),
                    "⚠ Cílová složka má jiný název - ověř, že jde o tutéž kolekci fotek.",
                );
            }
            ui.separator();
            ui.label(
                "Co se stane: obvykle 1 řádek v databázi (AgLibraryFolder), 0 přesunů souborů na disku. \
                 Když cílová složka v katalogu už existuje jako jiná, provede se místo toho jeden UPDATE \
                 pro každý soubor ve zdrojové složce.",
            );
            ui.label(
                egui::RichText::new("Před zápisem se automaticky vytvoří ověřená záloha katalogu.").weak(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Potvrdit a zapsat").clicked() {
                    confirmed = true;
                }
                if ui.button("Zrušit").clicked() {
                    cancelled = true;
                }
            });
        });

    if confirmed {
        state.pending_folder_repoint = None;
        state.start_apply_folder_repoint(suggestion);
    } else if cancelled || !open {
        state.pending_folder_repoint = None;
    }
}

/// Potvrzovací dialog pro vzor A, druhou možnost (§7.1 bod 2, "Přesun
/// souborů zpět") - stejný vzor jako [`show_folder_repoint_confirmation_dialog`].
fn show_folder_move_back_confirmation_dialog(ctx: &egui::Context, state: &mut AppState) {
    let Some(suggestion) = state.pending_folder_move_back.clone() else {
        return;
    };

    let mut open = true;
    let mut confirmed = false;
    let mut cancelled = false;

    egui::Window::new("Potvrdit přesun souborů zpět")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!("Složka „{}\u{201c}", suggestion.source_rel_path));
            ui.label(format!("Zdroj: „{}\u{201c}", suggestion.target_dir.display()));
            ui.label(format!(
                "Shoda: {}/{} souborů se svým nejlepším kandidátem v této složce",
                suggestion.matched_in_target, suggestion.with_candidate
            ));
            if suggestion.target_name_differs {
                ui.colored_label(
                    egui::Color32::from_rgb(0xd9, 0x8a, 0x1f),
                    "⚠ Zdrojová složka má jiný název - ověř, že jde o tutéž kolekci fotek.",
                );
            }
            ui.separator();
            ui.label(format!(
                "Co se stane: až {} souborů (vč. sidecarů jako .xmp) se přesune na cestu, kterou katalog \
                 už teď očekává. Katalog se nezapisuje (0 řádků DB) - jen disk se srovná s katalogem.",
                suggestion.matched_in_target
            ));
            ui.label(
                egui::RichText::new(
                    "Před přesunem se automaticky vytvoří ověřená záloha katalogu (i když se nemění).",
                )
                .weak(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Potvrdit a přesunout").clicked() {
                    confirmed = true;
                }
                if ui.button("Zrušit").clicked() {
                    cancelled = true;
                }
            });
        });

    if confirmed {
        state.pending_folder_move_back = None;
        state.start_apply_folder_move_back(suggestion);
    } else if cancelled || !open {
        state.pending_folder_move_back = None;
    }
}

/// Potvrzovací dialog pro vzor B (§7.2, "Konsolidace") - stejný vzor jako
/// [`show_folder_move_back_confirmation_dialog`].
fn show_consolidate_confirmation_dialog(ctx: &egui::Context, state: &mut AppState) {
    let Some(suggestion) = state.pending_consolidate.clone() else {
        return;
    };

    let mut open = true;
    let mut confirmed = false;
    let mut cancelled = false;

    egui::Window::new("Potvrdit konsolidaci")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!("Složka „{}\u{201c}", suggestion.source_rel_path));
            ui.label(format!(
                "{} souborů rozházeno v {} různých složkách (medián {:.1} na cíl)",
                suggestion.with_candidate, suggestion.distinct_targets, suggestion.median_per_target
            ));
            ui.separator();
            ui.label(format!(
                "Co se stane: až {} souborů (vč. sidecarů jako .xmp) se přesune zpátky do „{}\u{201c} - \
                 na cestu, kterou katalog už teď očekává. Katalog se nezapisuje (0 řádků DB). Soubory, \
                 které by v cíli kolidovaly se stejným jménem, se přeskočí a nahlásí, nikdy nepřepíšou.",
                suggestion.with_candidate, suggestion.source_rel_path
            ));
            ui.label(
                egui::RichText::new(
                    "Před přesunem se automaticky vytvoří ověřená záloha katalogu (i když se nemění).",
                )
                .weak(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Potvrdit a konsolidovat").clicked() {
                    confirmed = true;
                }
                if ui.button("Zrušit").clicked() {
                    cancelled = true;
                }
            });
        });

    if confirmed {
        state.pending_consolidate = None;
        state.start_apply_consolidate(suggestion);
    } else if cancelled || !open {
        state.pending_consolidate = None;
    }
}

/// Dialog duplikátů (§6.2) - vykreslí ho, dokud je otevřený, a předá
/// výslednou akci `AppState::handle_duplicate_picker_action` (žádné SQL/I/O
/// tady - jen předání do `state`, CLAUDE.md §5).
fn show_duplicate_picker_dialog(ctx: &egui::Context, state: &mut AppState) {
    let Some(mut picker) = state.pending_duplicate_picker.take() else {
        return;
    };
    match duplicates::show(ctx, &mut picker) {
        Some(action) => state.handle_duplicate_picker_action(picker, action),
        None => state.pending_duplicate_picker = Some(picker),
    }
}

/// Potvrzovací dialog pro zápis ručně vyřešených souborů (§6.2 + CLAUDE.md
/// §11 - žádný jednoklik bez souhrnu toho, co se stane).
fn show_resolved_confirmation_dialog(ctx: &egui::Context, state: &mut AppState) {
    if !state.confirm_apply_resolved {
        return;
    }

    let mut open = true;
    let mut confirmed = false;
    let mut cancelled = false;
    let count = state.resolved_choices.len();

    egui::Window::new("Potvrdit zápis vyřešených souborů")
        .collapsible(false)
        .resizable(false)
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(format!("{count} ručně vyřešených souborů čeká na zápis."));
            ui.label("Co se stane: pro každý soubor jeden UPDATE (případně INSERT nové složky) v katalogu, 0 přesunů souborů na disku.");
            ui.label(
                egui::RichText::new("Před zápisem se automaticky vytvoří ověřená záloha katalogu.").weak(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("Potvrdit a zapsat").clicked() {
                    confirmed = true;
                }
                if ui.button("Zrušit").clicked() {
                    cancelled = true;
                }
            });
        });

    if confirmed {
        state.confirm_apply_resolved = false;
        state.start_apply_resolved();
    } else if cancelled || !open {
        state.confirm_apply_resolved = false;
    }
}

fn show_status_bar(ctx: &egui::Context, state: &AppState) {
    egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
        ui.horizontal(|ui| {
            if state.applying {
                ui.spinner();
                ui.label("Zapisuji (záloha + katalog)…");
            } else if let Some(err) = &state.apply_error {
                ui.colored_label(
                    egui::Color32::from_rgb(0xd6, 0x33, 0x33),
                    format!("Zápis selhal: {err}"),
                );
            } else if let Some(msg) = &state.apply_success {
                ui.colored_label(egui::Color32::from_rgb(0x2e, 0xa0, 0x4a), msg);
            } else if state.loading {
                ui.spinner();
                ui.label("Načítám katalog…");
            } else if let Some(err) = &state.error {
                ui.colored_label(egui::Color32::from_rgb(0xd6, 0x33, 0x33), err);
            } else if let Some(loaded) = &state.loaded {
                let ok = loaded
                    .files
                    .iter()
                    .filter(|f| loaded.status_of(f.file_id) == FileStatus::Ok)
                    .count();
                let unavailable = loaded
                    .files
                    .iter()
                    .filter(|f| loaded.status_of(f.file_id) == FileStatus::FolderUnavailable)
                    .count();
                ui.label(format!(
                    "{}: {} souborů celkem, {} v pořádku, {} chybí",
                    loaded.catalog_path.display(),
                    loaded.files.len(),
                    ok,
                    loaded.files.len() - ok - unavailable
                ));
                if unavailable > 0 {
                    ui.colored_label(
                        egui::Color32::from_rgb(0x1f, 0x6f, 0xd9),
                        format!(
                            "⚠ {unavailable} souborů na nedostupném disku - nejde ověřit, \
                             nenabízí se k opravě (připojte disk a dejte Ověřit znovu)"
                        ),
                    );
                }
                if loaded.lock_status.is_locked() {
                    ui.colored_label(
                        egui::Color32::from_rgb(0xd9, 0x8a, 0x1f),
                        "⚠ katalog vypadá otevřený v Lightroom Classic (čtení je bezpečné)",
                    );
                }
                let with_candidates = loaded
                    .match_run
                    .matches
                    .values()
                    .filter(|m| !m.candidates.is_empty())
                    .count();
                ui.label(format!(
                    "· MATCH ({:?}, {} položek indexu): {}/{} má kandidáta",
                    loaded.match_run.index_backend,
                    loaded.match_run.index_size,
                    with_candidates,
                    loaded.match_run.matches.len()
                ));
            } else {
                ui.label("Zadejte cestu ke katalogu a klikněte na „Otevřít katalog“.");
            }
        });
    });
}
