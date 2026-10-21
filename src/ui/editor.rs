//! The event dialog: title, calendar, times, repeat, reminder, place and
//! notes. Recurring events are edited as a whole series; a single
//! occurrence can be skipped from here.

use super::*;
use crate::model::{CalendarKind, Event};
use crate::recur::Repeat;
use chrono::{NaiveTime, TimeZone};
use material_3_ui::common::{dialog_buttons, modal};
use material_3_ui::egui::{vec2, RichText};

pub struct Editor {
    pub ev: Event,
    /// The occurrence it was opened from, for "skip this one".
    pub occurrence: Option<i64>,
    pub is_new: bool,
    start_date: String,
    start_time: String,
    end_date: String,
    end_time: String,
    repeat: Repeat,
    reminder: Option<i64>,
    confirm_delete: bool,
    error: Option<String>,
    focus_title: bool,
}

const REMINDERS: &[(Option<i64>, &str)] = &[
    (None, "No reminder"),
    (Some(0), "At the start"),
    (Some(5), "5 minutes before"),
    (Some(10), "10 minutes before"),
    (Some(15), "15 minutes before"),
    (Some(30), "30 minutes before"),
    (Some(60), "1 hour before"),
    (Some(120), "2 hours before"),
    (Some(1440), "1 day before"),
    (Some(10080), "1 week before"),
];

pub fn reminder_label(m: Option<i64>) -> String {
    REMINDERS
        .iter()
        .find(|(v, _)| *v == m)
        .map(|(_, l)| l.to_string())
        .unwrap_or_else(|| format!("{} minutes before", m.unwrap_or(0)))
}

pub fn reminder_choices() -> &'static [(Option<i64>, &'static str)] {
    REMINDERS
}

fn local_parts(ts: i64, all_day: bool) -> (String, String) {
    if all_day {
        (event_date(ts, true).format("%Y-%m-%d").to_string(), "09:00".into())
    } else {
        let t = Local.timestamp_opt(ts, 0).single().unwrap_or_default();
        (t.format("%Y-%m-%d").to_string(), t.format("%H:%M").to_string())
    }
}

/// "13:30", or anything quick add understands ("1:30pm", "noon").
fn parse_time(s: &str) -> Option<NaiveTime> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    NaiveTime::parse_from_str(s, "%H:%M").ok().or_else(|| {
        let q = crate::nlp::parse(&format!("x {s}"), Local::now().naive_local());
        (!q.all_day).then(|| q.start.time())
    })
}

impl Editor {
    pub fn new(ev: Event, occurrence: Option<i64>, is_new: bool, _settings: &Settings) -> Self {
        let (start_date, start_time) = local_parts(ev.start, ev.all_day);
        // All-day ends are exclusive; show the last day.
        let end_ts = if ev.all_day { (ev.end - 86400).max(ev.start) } else { ev.end };
        let (end_date, end_time) = local_parts(end_ts, ev.all_day);
        Self {
            repeat: Repeat::from_rule(ev.rrule.as_deref()),
            reminder: ev.reminders.first().copied(),
            ev,
            occurrence,
            is_new,
            start_date,
            start_time,
            end_date,
            end_time,
            confirm_delete: false,
            error: None,
            focus_title: is_new,
        }
    }

    /// Reads the form back into an event.
    fn build(&self) -> Result<Event, String> {
        let mut ev = self.ev.clone();
        let date = |s: &str| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| format!("“{}” isn't a date (YYYY-MM-DD)", s.trim()));
        let sd = date(&self.start_date)?;
        let ed = date(&self.end_date)?;
        if ev.all_day {
            if ed < sd {
                return Err("The event ends before it starts".into());
            }
            ev.start = utc_midnight(sd);
            ev.end = utc_midnight(ed + Duration::days(1));
            ev.tz = String::new();
        } else {
            let st = parse_time(&self.start_time).ok_or_else(|| format!("“{}” isn't a time", self.start_time))?;
            let et = parse_time(&self.end_time).ok_or_else(|| format!("“{}” isn't a time", self.end_time))?;
            let to_ts = |d: NaiveDate, t: NaiveTime| Local.from_local_datetime(&d.and_time(t)).earliest().map(|x| x.timestamp());
            ev.start = to_ts(sd, st).ok_or("That time doesn't exist (a DST change?)")?;
            ev.end = to_ts(ed, et).ok_or("That time doesn't exist (a DST change?)")?;
            if ev.end < ev.start {
                return Err("The event ends before it starts".into());
            }
            if ev.tz.is_empty() {
                ev.tz = crate::recur::local_zone_name();
            }
        }
        if self.repeat != Repeat::Custom {
            ev.rrule = self.repeat.rule().map(String::from);
        }
        if self.reminder != ev.reminders.first().copied() {
            ev.reminders = self.reminder.into_iter().collect();
        }
        Ok(ev)
    }
}

enum Outcome {
    None,
    Close,
    Save(Box<Event>),
    Delete,
    Skip(i64),
}

pub fn show(app: &mut App, ctx: &egui::Context) {
    let Some(ed) = app.editor.as_mut() else { return };
    let p = app.theme.palette().clone();
    let cals: Vec<Calendar> = app.calendars.iter().filter(|c| c.kind == CalendarKind::Events && (!c.read_only || c.id == ed.ev.calendar_id)).cloned().collect();
    let read_only = app.calendars.iter().find(|c| c.id == ed.ev.calendar_id).is_some_and(|c| c.read_only);
    let mut outcome = Outcome::None;
    let title = if ed.is_new { "New event" } else if read_only { "Event" } else { "Edit event" };
    let resp = modal(ctx, &p, "event-editor", ICON_EVENT, title, |ui| {
        ui.set_width(460.0);
        ui.add_enabled_ui(!read_only, |ui| {
            let t = ui.add(
                egui::TextEdit::singleline(&mut ed.ev.title)
                    .hint_text("Add title")
                    .font(widgets::title_font(18.0))
                    .desired_width(f32::INFINITY)
                    .margin(vec2(10.0, 8.0)),
            );
            if ed.focus_title {
                t.request_focus();
                ed.focus_title = false;
            }
            ui.add_space(10.0);
            egui::Grid::new("editor-grid").num_columns(2).spacing(vec2(14.0, 10.0)).min_col_width(70.0).show(ui, |ui| {
                ui.label(RichText::new("Calendar").color(p.on_surface_variant));
                let current = cals.iter().find(|c| c.id == ed.ev.calendar_id);
                egui::ComboBox::from_id_salt("cal")
                    .width(300.0)
                    .selected_text(current.map(|c| c.name.clone()).unwrap_or_else(|| "Choose…".into()))
                    .show_ui(ui, |ui| {
                        for c in &cals {
                            let text = RichText::new(format!("●  {}", c.name)).color(hex(&c.color));
                            ui.selectable_value(&mut ed.ev.calendar_id, c.id.clone(), text);
                        }
                    });
                ui.end_row();

                ui.label(RichText::new("All day").color(p.on_surface_variant));
                let was = ed.ev.all_day;
                widgets::switch(ui, &mut ed.ev.all_day);
                if was != ed.ev.all_day && ed.ev.all_day {
                    ed.reminder = None;
                }
                ui.end_row();

                ui.label(RichText::new("Starts").color(p.on_surface_variant));
                ui.horizontal(|ui| {
                    let before = ed.start_date.clone();
                    ui.add(egui::TextEdit::singleline(&mut ed.start_date).desired_width(110.0));
                    // Moving the start moves a same-day end along.
                    if before != ed.start_date && before == ed.end_date {
                        ed.end_date = ed.start_date.clone();
                    }
                    if !ed.ev.all_day {
                        ui.add(egui::TextEdit::singleline(&mut ed.start_time).desired_width(64.0));
                    }
                    ui.label(RichText::new(weekday_of(&ed.start_date)).color(p.on_surface_variant));
                });
                ui.end_row();

                ui.label(RichText::new("Ends").color(p.on_surface_variant));
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut ed.end_date).desired_width(110.0));
                    if !ed.ev.all_day {
                        ui.add(egui::TextEdit::singleline(&mut ed.end_time).desired_width(64.0));
                    }
                    ui.label(RichText::new(weekday_of(&ed.end_date)).color(p.on_surface_variant));
                });
                ui.end_row();

                ui.label(RichText::new("Repeat").color(p.on_surface_variant));
                egui::ComboBox::from_id_salt("repeat").width(300.0).selected_text(if ed.repeat == Repeat::Custom {
                    format!("Custom ({})", ed.ev.rrule.clone().unwrap_or_default())
                } else {
                    ed.repeat.label().to_string()
                }).show_ui(ui, |ui| {
                    for r in Repeat::CHOICES {
                        ui.selectable_value(&mut ed.repeat, r, r.label());
                    }
                });
                ui.end_row();

                ui.label(RichText::new("Reminder").color(p.on_surface_variant));
                egui::ComboBox::from_id_salt("reminder").width(300.0).selected_text(reminder_label(ed.reminder)).show_ui(ui, |ui| {
                    for (v, l) in REMINDERS {
                        ui.selectable_value(&mut ed.reminder, *v, *l);
                    }
                });
                ui.end_row();

                ui.label(RichText::new("Where").color(p.on_surface_variant));
                ui.add(egui::TextEdit::singleline(&mut ed.ev.location).hint_text("Add location").desired_width(300.0));
                ui.end_row();
            });
            ui.add_space(8.0);
            ui.add(egui::TextEdit::multiline(&mut ed.ev.description).hint_text("Add description").desired_rows(3).desired_width(f32::INFINITY));
        });
        if read_only {
            ui.add_space(6.0);
            material_3_ui::common::note(ui, "This calendar is read-only.");
        } else if ed.ev.rrule.is_some() && !ed.is_new {
            ui.add_space(6.0);
            material_3_ui::common::note(ui, "Changes apply to every occurrence of this repeating event.");
        }
        if let Some(e) = &ed.error {
            ui.add_space(6.0);
            ui.label(RichText::new(e).color(p.error));
        }
        dialog_buttons(ui, |ui| {
            if read_only {
                if widgets::button(ui, ButtonKind::Filled, None, "Close").clicked() {
                    outcome = Outcome::Close;
                }
                return;
            }
            if widgets::button(ui, ButtonKind::Filled, Some(ICON_CHECK), "Save").clicked() {
                match ed.build() {
                    Ok(ev) => outcome = Outcome::Save(Box::new(ev)),
                    Err(e) => ed.error = Some(e),
                }
            }
            if widgets::button(ui, ButtonKind::Text, None, "Cancel").clicked() {
                outcome = Outcome::Close;
            }
            if !ed.is_new {
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    if ed.confirm_delete {
                        if ed.ev.rrule.is_some() {
                            if let Some(at) = ed.occurrence {
                                if widgets::button(ui, ButtonKind::Tonal, None, "Only this one").clicked() {
                                    outcome = Outcome::Skip(at);
                                }
                            }
                            if widgets::button(ui, ButtonKind::Danger, None, "All").clicked() {
                                outcome = Outcome::Delete;
                            }
                        } else if widgets::button(ui, ButtonKind::Danger, Some(ICON_DELETE), "Delete").clicked() {
                            outcome = Outcome::Delete;
                        }
                    } else if widgets::icon_button(ui, ICON_DELETE, "Delete", true).clicked() {
                        ed.confirm_delete = true;
                    }
                });
            }
        });
    });
    if resp.should_close() && matches!(outcome, Outcome::None) {
        outcome = Outcome::Close;
    }
    match outcome {
        Outcome::None => {}
        Outcome::Close => app.editor = None,
        Outcome::Save(ev) => match crate::ops::save_event(&app.store, &app.settings, *ev) {
            Ok(_) => {
                app.editor = None;
                app.changed();
            }
            Err(e) => {
                if let Some(ed) = app.editor.as_mut() {
                    ed.error = Some(format!("{e:#}"));
                }
            }
        },
        Outcome::Delete => {
            let ev = app.editor.take().map(|e| e.ev).unwrap_or_default();
            let backup = app.store.event(&ev.id).ok().flatten();
            match crate::ops::delete_event(&app.store, &ev.id) {
                Ok(()) => {
                    match backup {
                        Some(b) => app.toasts.push(toast_with(format!("Deleted “{}”", b.title), "Undo", Action::UndoDelete(Box::new(b)))),
                        None => app.toasts.info("Deleted"),
                    }
                    app.changed();
                }
                Err(e) => app.toasts.error(format!("{e:#}")),
            }
        }
        Outcome::Skip(at) => {
            let id = app.editor.take().map(|e| e.ev.id).unwrap_or_default();
            match crate::ops::delete_occurrence(&app.store, &id, at) {
                Ok(()) => {
                    app.toasts.info("Removed this occurrence");
                    app.changed();
                }
                Err(e) => app.toasts.error(format!("{e:#}")),
            }
        }
    }
}

fn weekday_of(s: &str) -> String {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map(|d| d.format("%A").to_string()).unwrap_or_default()
}
