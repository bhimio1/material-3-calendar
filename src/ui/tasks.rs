//! The tasks pane: open tasks from every visible list, a field to add one,
//! and a check circle to complete it.

use super::*;
use material_3_ui::egui::{pos2, vec2, CornerRadius, FontId, Sense, Stroke};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let p = widgets::palette(ui);
    egui::Frame::new()
        .fill(p.surface_container_low)
        .corner_radius(CornerRadius::same(24))
        .inner_margin(egui::Margin::symmetric(14, 14))
        .show(ui, |ui| {
            ui.set_min_size(ui.available_size());
            ui.horizontal(|ui| {
                widgets::icon(ui, ICON_TASK_ALT, 22.0, p.primary);
                ui.label(widgets::title_text("Tasks", 18.0).color(p.on_surface));
            });
            ui.add_space(10.0);
            let resp = material_3_ui::common::search_field(ui, &mut app.new_task, "Add a task", ui.available_width());
            if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) && !app.new_task.trim().is_empty() {
                let title = std::mem::take(&mut app.new_task);
                let q = crate::nlp::parse(&title, Local::now().naive_local());
                // "Pay rent friday" → due Friday.
                let has_date = q.start.date() != Local::now().date_naive();
                let task = Task {
                    title: if has_date { q.title } else { title.trim().to_string() },
                    due: has_date.then(|| utc_midnight(q.start.date())),
                    ..Default::default()
                };
                match crate::ops::save_task(&app.store, task) {
                    Ok(_) => app.changed(),
                    Err(e) => app.toasts.error(format!("{e:#}")),
                }
                resp.request_focus();
            }
            ui.add_space(6.0);
            let today = Local::now().date_naive();
            let names: std::collections::HashMap<String, String> = app.calendars.iter().map(|c| (c.id.clone(), c.name.clone())).collect();
            let mut complete = None;
            let mut remove = None;
            egui::ScrollArea::vertical().id_salt("tasks").auto_shrink([false, false]).show(ui, |ui| {
                if app.tasks.is_empty() {
                    ui.add_space(30.0);
                    material_3_ui::common::empty_hint(ui, "All done");
                }
                for t in &app.tasks {
                    let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 50.0), Sense::hover());
                    let c = pos2(r.left() + 14.0, r.top() + 17.0);
                    let check = ui.interact(egui::Rect::from_center_size(c, vec2(28.0, 28.0)), egui::Id::unique(("chk", &t.id)), Sense::click());
                    let hovered = check.hovered();
                    ui.painter().circle_stroke(c, 9.0, Stroke::new(2.0, if hovered { p.primary } else { p.outline }));
                    if hovered {
                        widgets::paint_icon(ui, ICON_CHECK, c, 14.0, p.primary);
                    }
                    if check.on_hover_text("Mark done").clicked() {
                        complete = Some(t.id.clone());
                    }
                    let g = widgets::truncated_galley(ui, &t.title, FontId::proportional(13.5), p.on_surface, r.width() - 36.0);
                    ui.painter().galley(pos2(r.left() + 32.0, r.top() + 8.0), g, p.on_surface);
                    let mut sub = names.get(&t.calendar_id).cloned().unwrap_or_default();
                    let mut sub_color = p.on_surface_variant;
                    if let Some(d) = t.due.map(|d| event_date(d, true)) {
                        let label = match (d - today).num_days() {
                            0 => "Today".to_string(),
                            1 => "Tomorrow".to_string(),
                            n if n < 0 => {
                                sub_color = p.error;
                                format!("Overdue · {}", d.format("%e %b"))
                            }
                            n if n < 7 => d.format("%A").to_string(),
                            _ => d.format("%e %b").to_string(),
                        };
                        sub = format!("{label} · {sub}");
                    }
                    let g = widgets::truncated_galley(ui, &sub, FontId::proportional(11.5), sub_color, r.width() - 36.0);
                    ui.painter().galley(pos2(r.left() + 32.0, r.top() + 27.0), g, sub_color);
                    resp.context_menu(|ui| {
                        if ui.button("Delete task").clicked() {
                            remove = Some(t.id.clone());
                            ui.close();
                        }
                    });
                }
            });
            if let Some(id) = complete {
                match crate::ops::set_task_completed(&app.store, &id, true) {
                    Ok(()) => {
                        app.toasts.push(toast_with("Task completed", "Undo", Action::UndoTask(id)));
                        app.changed();
                    }
                    Err(e) => app.toasts.error(format!("{e:#}")),
                }
            }
            if let Some(id) = remove {
                if let Err(e) = crate::ops::delete_task(&app.store, &id) {
                    app.toasts.error(format!("{e:#}"));
                }
                app.changed();
            }
        });
}
