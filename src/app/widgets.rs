//! Vlastní egui widgety. Jen vykreslování a vstup - žádná logika, žádné I/O
//! (CLAUDE.md §5).

/// Posuvný přepínač („Android style") místo zaškrtávacího políčka.
///
/// Používá se pro NÁHLED / ZÁPIS POVOLEN, kde CLAUDE.md §11 vyžaduje, aby
/// bylo na první pohled vidět, jestli se právě mění data. Zaškrtávací políčko
/// je na to slabé - má stejný tvar v obou stavech a liší se jen fajfkou.
/// Posuvník mění tvar i barvu a čte se přes celou šířku panelu.
///
/// `on_color` je barva zapnutého stavu; volající ji dodává, aby si widget
/// nepamatoval nic o tom, co přepíná.
pub fn toggle_switch(ui: &mut egui::Ui, on: &mut bool, on_color: egui::Color32) -> egui::Response {
    let desired = egui::vec2(44.0, 22.0);
    let (rect, mut response) = ui.allocate_exact_size(desired, egui::Sense::click());

    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::WidgetType::Checkbox, ui.is_enabled(), *on, "")
    });

    if !ui.is_rect_visible(rect) {
        return response;
    }

    // `animate_bool_responsive` drží animaci mezi snímky podle id widgetu,
    // takže přepnutí je plynulé a ne skokové.
    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let visuals = ui.style().interact_selectable(&response, *on);

    let radius = 0.5 * rect.height();
    let track = if *on {
        on_color
    } else {
        ui.visuals().widgets.inactive.bg_fill
    };
    ui.painter().rect(
        rect,
        radius,
        track,
        egui::Stroke::new(1.0_f32, ui.visuals().widgets.inactive.bg_stroke.color),
    );

    let knob_x = egui::lerp((rect.left() + radius)..=(rect.right() - radius), t);
    ui.painter().circle(
        egui::pos2(knob_x, rect.center().y),
        0.75 * radius,
        egui::Color32::WHITE,
        visuals.fg_stroke,
    );

    response
}
