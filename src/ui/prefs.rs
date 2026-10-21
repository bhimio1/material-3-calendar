//! Preferences: appearance (shared with the other Material 3 apps),
//! calendar defaults, background sync, and the OAuth client ids.

use super::*;
use crate::config::WeekStart;
use material_3_ui::common::{self, card, row};

const SERVICE: &str = "m3-calendar.service";

/// (enabled, active), asked at most every few seconds.
fn service_state() -> (bool, bool) {
    use std::cell::Cell;
    use std::time::{Duration, Instant};
    thread_local!(static CACHE: Cell<Option<(Instant, (bool, bool))>> = const { Cell::new(None) });
    if let Some((at, v)) = CACHE.get() {
        if at.elapsed() < Duration::from_secs(3) {
            return v;
        }
    }
    let v = query_service();
    CACHE.set(Some((Instant::now(), v)));
    v
}

fn query_service() -> (bool, bool) {
    let q = |what: &str| material_3_ui::sys::cmd::run("systemctl", &["--user", what, SERVICE]).map(|s| s.trim().to_string());
    let enabled = q("is-enabled").is_ok_and(|s| s == "enabled");
    let active = q("is-active").is_ok_and(|s| s == "active");
    (enabled, active)
}

pub fn page(app: &mut App, ui: &mut egui::Ui) {
    let mut changed = false;
    common::page(ui, ICON_SETTINGS, "Preferences", "", |ui| {
        if material_3_ui::prefs::appearance_card(ui, &mut app.settings.appearance, &app.theme) {
            app.theme.apply(&app.settings.appearance);
            changed = true;
        }
        card(ui, "Calendar", |ui| {
            row(ui, "Week starts on", "", |ui| {
                changed |= widgets::segmented(
                    ui,
                    &mut app.settings.week_start,
                    &[(WeekStart::Saturday, ICON_CALENDAR_TODAY, "Sat"), (WeekStart::Sunday, ICON_CALENDAR_TODAY, "Sun"), (WeekStart::Monday, ICON_CALENDAR_TODAY, "Mon")],
                );
            });
            ui.separator();
            row(ui, "Open in", "The view Calendar starts with.", |ui| {
                changed |= widgets::segmented(
                    ui,
                    &mut app.settings.view,
                    &[
                        (ViewKind::Agenda, ICON_VIEW_AGENDA, "Agenda"),
                        (ViewKind::Day, ICON_CALENDAR_VIEW_DAY, "Day"),
                        (ViewKind::Week, ICON_CALENDAR_VIEW_WEEK, "Week"),
                        (ViewKind::Month, ICON_CALENDAR_VIEW_MONTH, "Month"),
                    ],
                );
            });
            ui.separator();
            row(ui, "24-hour clock", "", |ui| {
                changed |= widgets::switch(ui, &mut app.settings.time_24h).changed();
            });
            ui.separator();
            row(ui, "Week numbers", "In the month view.", |ui| {
                changed |= widgets::switch(ui, &mut app.settings.show_week_numbers).changed();
            });
            ui.separator();
            row(ui, "Default reminder", "For new timed events.", |ui| {
                egui::ComboBox::from_id_salt("def-rem")
                    .width(180.0)
                    .selected_text(super::editor::reminder_label(app.settings.default_reminder))
                    .show_ui(ui, |ui| {
                        for (v, l) in super::editor::reminder_choices() {
                            changed |= ui.selectable_value(&mut app.settings.default_reminder, *v, *l).changed();
                        }
                    });
            });
            ui.separator();
            row(ui, "Default length", "For new events.", |ui| {
                egui::ComboBox::from_id_salt("def-len")
                    .width(180.0)
                    .selected_text(format!("{} minutes", app.settings.default_duration))
                    .show_ui(ui, |ui| {
                        for m in [15, 30, 45, 60, 90, 120] {
                            changed |= ui.selectable_value(&mut app.settings.default_duration, m, format!("{m} minutes")).changed();
                        }
                    });
            });
            ui.separator();
            row(ui, "New events go to", "", |ui| {
                let cals: Vec<Calendar> = app.calendars.iter().filter(|c| c.kind == crate::model::CalendarKind::Events && !c.read_only).cloned().collect();
                let current = cals.iter().find(|c| c.id == app.settings.default_calendar).map(|c| c.name.clone()).unwrap_or_else(|| "First visible calendar".into());
                egui::ComboBox::from_id_salt("def-cal").width(220.0).selected_text(current).show_ui(ui, |ui| {
                    for c in &cals {
                        changed |= ui.selectable_value(&mut app.settings.default_calendar, c.id.clone(), &c.name).changed();
                    }
                });
            });
        });
        card(ui, "Background sync", |ui| {
            let (enabled, active) = service_state();
            row(
                ui,
                "Sync and remind in the background",
                if active { "The calendar service is running: it syncs, answers the shell, and shows reminders even when this window is closed." } else { "Turn on to get reminders and keep the shell's calendar up to date while Calendar is closed." },
                |ui| {
                    let mut on = enabled || active;
                    if widgets::switch(ui, &mut on).changed() {
                        let args: &[&str] = if on { &["--user", "enable", "--now", SERVICE] } else { &["--user", "disable", "--now", SERVICE] };
                        match material_3_ui::sys::cmd::run("systemctl", args) {
                            Ok(_) => {
                                app.daemon = on;
                                query_service();
                            }
                            Err(e) => app.toasts.error(format!("systemctl: {e}")),
                        }
                    }
                },
            );
            ui.separator();
            row(ui, "Sync every", "", |ui| {
                egui::ComboBox::from_id_salt("sync-min").width(140.0).selected_text(format!("{} minutes", app.settings.sync_minutes)).show_ui(ui, |ui| {
                    for m in [5u64, 15, 30, 60] {
                        changed |= ui.selectable_value(&mut app.settings.sync_minutes, m, format!("{m} minutes")).changed();
                    }
                });
            });
        });
        card(ui, "Sign-in clients", |ui| {
            common::note(ui, "Google and Microsoft sign-in use OAuth clients you register yourself (Accounts shows how). The environment variables M3_CALENDAR_GOOGLE_CLIENT_ID and M3_CALENDAR_MS_CLIENT_ID override these.");
            row(ui, "Google client ID", "", |ui| {
                changed |= ui.add(egui::TextEdit::singleline(&mut app.settings.google_client_id).desired_width(300.0)).lost_focus();
            });
            ui.separator();
            row(ui, "Microsoft client ID", "", |ui| {
                changed |= ui.add(egui::TextEdit::singleline(&mut app.settings.microsoft_client_id).desired_width(300.0)).lost_focus();
            });
        });
    });
    if changed {
        app.save_settings();
        app.reload();
    }
}
