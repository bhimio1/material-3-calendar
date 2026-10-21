//! The left side: new event and quick add, the mini month, calendars by
//! account, and the way to Accounts and Preferences.

use super::*;
use material_3_ui::calendar;
use material_3_ui::egui::{pos2, vec2, CornerRadius, FontId, Rect, Sense, Stroke, StrokeKind};

pub fn show(app: &mut App, ui: &mut egui::Ui) {
    let p = widgets::palette(ui);
    ui.horizontal(|ui| {
        ui.add_space(4.0);
        widgets::icon(ui, ICON_CALENDAR_MONTH, 26.0, p.primary);
        ui.label(widgets::title_text("Calendar", 20.0).color(p.on_surface));
    });
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        if widgets::button(ui, ButtonKind::Filled, Some(ICON_ADD), "New event").on_hover_text("N").clicked() {
            app.new_event_at(app.cursor, Some(9 * 60));
        }
    });
    ui.add_space(8.0);
    let resp = material_3_ui::common::search_field(ui, &mut app.quick_add, "Quick add: lunch tomorrow 1pm", ui.available_width());
    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        app.quick_add_now();
    }
    ui.add_space(10.0);
    mini_month(app, ui);
    ui.add_space(6.0);

    let bottom_h = 96.0;
    egui::ScrollArea::vertical()
        .id_salt("calendars")
        .max_height((ui.available_height() - bottom_h).max(60.0))
        .auto_shrink([false, true])
        .show(ui, |ui| calendar_list(app, ui));

    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        let errors = app.accounts.list.iter().filter(|a| app.store.account_status(&a.id).error.is_some()).count();
        let badge = if errors > 0 { Some("!") } else { None };
        if widgets::nav_item(ui, ICON_SETTINGS, "Preferences", app.page == Page::Preferences, None).clicked() {
            app.page = Page::Preferences;
        }
        if widgets::nav_item(ui, ICON_ACCOUNT_CIRCLE, "Accounts", app.page == Page::Accounts, badge).clicked() {
            app.page = Page::Accounts;
        }
    });
}

fn mini_month(app: &mut App, ui: &mut egui::Ui) {
    app.ensure_busy_days();
    let busy = &app.busy_days;
    if let Some(d) = calendar::mini_month(ui, &mut app.mini_month, app.cursor, app.settings.week_start.weekday(), |d| busy.contains(&d)) {
        app.cursor = d;
        app.page = Page::Calendar;
        app.scroll_to_hour = true;
    }
}

fn calendar_list(app: &mut App, ui: &mut egui::Ui) {
    let p = widgets::palette(ui);
    let accounts = app.accounts.list.clone();
    let mut toggle = None;
    let mut only = None;
    let mut make_default = None;
    let mut delete = None;
    for account in &accounts {
        let cals: Vec<Calendar> = app.calendars.iter().filter(|c| c.account_id == account.id).cloned().collect();
        if cals.is_empty() {
            continue;
        }
        let status = app.store.account_status(&account.id);
        ui.horizontal(|ui| {
            widgets::section_header(ui, &account.name);
            if status.error.is_some() {
                widgets::icon(ui, ICON_ERROR, 16.0, p.error).on_hover_text(status.error.clone().unwrap_or_default());
            }
        });
        for cal in cals {
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
            let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
            if resp.hovered() {
                ui.painter().rect_filled(r, CornerRadius::same(16), p.on_surface.gamma_multiply(0.06));
            }
            let color = hex(&cal.color);
            let b = Rect::from_center_size(pos2(r.left() + 20.0, r.center().y), vec2(18.0, 18.0));
            if cal.visible {
                ui.painter().rect_filled(b, CornerRadius::same(5), color);
                widgets::paint_icon(ui, ICON_CHECK, b.center(), 15.0, text_on(color));
            } else {
                ui.painter().rect_stroke(b, CornerRadius::same(5), Stroke::new(2.0, color), StrokeKind::Inside);
            }
            let icon_w = if cal.kind == crate::model::CalendarKind::Tasks { 20.0 } else { 0.0 };
            let g = widgets::truncated_galley(ui, &cal.name, FontId::proportional(13.5), p.on_surface, r.width() - 48.0 - icon_w);
            ui.painter().galley(pos2(r.left() + 38.0, r.center().y - g.size().y / 2.0), g, p.on_surface);
            if icon_w > 0.0 {
                widgets::paint_icon(ui, ICON_CHECKLIST, pos2(r.right() - 14.0, r.center().y), 16.0, p.on_surface_variant);
            } else if cal.read_only {
                widgets::paint_icon(ui, ICON_LOCK, pos2(r.right() - 14.0, r.center().y), 14.0, p.on_surface_variant);
            }
            if resp.clicked() {
                toggle = Some((cal.id.clone(), !cal.visible));
            }
            resp.context_menu(|ui| {
                if ui.button("Show only this").clicked() {
                    only = Some(cal.id.clone());
                    ui.close();
                }
                if cal.kind == crate::model::CalendarKind::Events && !cal.read_only && ui.button("Use for new events").clicked() {
                    make_default = Some(cal.id.clone());
                    ui.close();
                }
                ui.menu_button("Color", |ui| {
                    ui.horizontal_wrapped(|ui| {
                        ui.set_max_width(160.0);
                        for c in crate::model::CALENDAR_COLORS {
                            let (r, resp) = ui.allocate_exact_size(vec2(22.0, 22.0), Sense::click());
                            ui.painter().circle_filled(r.center(), 10.0, hex(c));
                            if resp.clicked() {
                                let _ = app.store.set_calendar_color(&cal.id, c);
                                ui.close();
                            }
                        }
                    });
                });
                if cal.account_id == "local" {
                    if ui.button("Export as .ics").clicked() {
                        let dir = dirs::download_dir().unwrap_or_else(material_3_ui::sys::paths::home);
                        let path = dir.join(format!("{}.ics", cal.name.replace('/', "-")));
                        match crate::sync::export_ics(&app.store, &cal.id, &path) {
                            Ok(n) => app.toasts.success(format!("Exported {n} events to {}", material_3_ui::sys::paths::tilde(&path))),
                            Err(e) => app.toasts.error(format!("{e:#}")),
                        }
                        ui.close();
                    }
                    if ui.button("Delete calendar").clicked() {
                        delete = Some(cal.id.clone());
                        ui.close();
                    }
                }
            });
        }
    }
    if let Some((id, v)) = toggle {
        let _ = app.store.set_calendar_visible(&id, v);
    }
    if let Some(id) = only {
        for c in &app.calendars {
            let _ = app.store.set_calendar_visible(&c.id, c.id == id || c.kind != app.calendar(&id).map(|x| x.kind).unwrap_or_default());
        }
    }
    if let Some(id) = make_default {
        app.settings.default_calendar = id;
        app.save_settings();
        app.toasts.info("New events go to this calendar now");
    }
    if let Some(id) = delete {
        let local_events = app.calendars.iter().filter(|c| c.account_id == "local" && c.kind == crate::model::CalendarKind::Events).count();
        if local_events <= 1 && app.calendar(&id).is_some_and(|c| c.kind == crate::model::CalendarKind::Events) && !app.accounts.list.iter().any(|a| !a.is_local()) {
            app.toasts.error("Keep at least one calendar");
        } else {
            let _ = app.store.delete_calendar(&id);
            app.toasts.info("Calendar deleted");
        }
    }
    app.reload_if_changed();
}

impl App {
    pub fn reload_if_changed(&mut self) {
        if self.store.revision() != self.rev {
            self.reload();
        }
    }
}
