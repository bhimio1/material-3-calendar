//! Edits shared by the app, the D-Bus service and the command line: they
//! pick the calendar, apply defaults, and queue changes for remote accounts.

use crate::config::{Accounts, Settings};
use crate::model::{CalendarKind, Event, Occurrence, Task};
use crate::store::Store;
use anyhow::{anyhow, Result};
use chrono::{Local, TimeZone};
use serde_json::{json, Value};

/// The calendar new events go to: the preferred one if it's writable,
/// else the first writable visible calendar.
pub fn default_calendar(store: &Store, settings: &Settings) -> Result<String> {
    let cals = store.calendars()?;
    let writable = |c: &&crate::model::Calendar| c.kind == CalendarKind::Events && !c.read_only;
    cals.iter()
        .filter(writable)
        .find(|c| c.id == settings.default_calendar)
        .or_else(|| cals.iter().filter(writable).find(|c| c.visible))
        .or_else(|| cals.iter().find(writable))
        .map(|c| c.id.clone())
        .ok_or_else(|| anyhow!("there's no calendar to add events to"))
}

pub fn default_task_list(store: &Store) -> Result<String> {
    if let Some(c) = store.calendars()?.into_iter().find(|c| c.kind == CalendarKind::Tasks && !c.read_only) {
        return Ok(c.id);
    }
    Ok(crate::sync::add_local_task_list(store, "Tasks")?.id)
}

fn tracked(store: &Store, calendar_id: &str) -> Result<bool> {
    let cal = store.calendar(calendar_id)?.ok_or_else(|| anyhow!("no such calendar: {calendar_id}"))?;
    if cal.read_only {
        return Err(anyhow!("{} is read-only", cal.name));
    }
    Ok(Accounts::load().is_remote(&cal.account_id))
}

/// Creates or updates an event. Returns its id.
pub fn save_event(store: &Store, settings: &Settings, mut ev: Event) -> Result<String> {
    if ev.calendar_id.is_empty() {
        ev.calendar_id = default_calendar(store, settings)?;
    }
    if ev.tz.is_empty() && !ev.all_day {
        ev.tz = crate::recur::local_zone_name();
    }
    if ev.end < ev.start {
        ev.end = ev.start;
    }
    if ev.title.trim().is_empty() {
        ev.title = "(No title)".into();
    }
    // Moving between calendars: delete from the old one, create in the new.
    if let Some(old) = store.event(&ev.id)?.filter(|o| o.calendar_id != ev.calendar_id) {
        store.delete_event(&old.id, tracked(store, &old.calendar_id)?)?;
        ev.id = String::new();
        ev.uid = String::new();
        ev.etag = None;
        ev.href = None;
    } else if let Some(old) = store.event(&ev.id)? {
        // Keep provider bookkeeping the caller may not have.
        ev.uid = old.uid;
        ev.etag = old.etag;
        ev.href = old.href;
        ev.recurrence_id = old.recurrence_id;
    }
    let track = tracked(store, &ev.calendar_id)?;
    store.save_event(&mut ev, track)?;
    Ok(ev.id)
}

/// A new event with the user's defaults (reminder, duration).
pub fn new_event(settings: &Settings, start: i64, all_day: bool) -> Event {
    Event {
        start,
        end: start + if all_day { 86400 } else { settings.default_duration * 60 },
        all_day,
        reminders: if all_day { vec![] } else { settings.default_reminder.into_iter().collect() },
        ..Default::default()
    }
}

pub fn delete_event(store: &Store, id: &str) -> Result<()> {
    let ev = store.event(id)?.ok_or_else(|| anyhow!("no such event"))?;
    store.delete_event(id, tracked(store, &ev.calendar_id)?)
}

pub fn delete_occurrence(store: &Store, id: &str, start: i64) -> Result<()> {
    let ev = store.event(id)?.ok_or_else(|| anyhow!("no such event"))?;
    store.exclude_occurrence(id, start, tracked(store, &ev.calendar_id)?)
}

pub fn set_task_completed(store: &Store, id: &str, completed: bool) -> Result<()> {
    let t = store.task(id)?.ok_or_else(|| anyhow!("no such task"))?;
    store.set_task_completed(id, completed, tracked(store, &t.calendar_id)?)?;
    Ok(())
}

pub fn save_task(store: &Store, mut t: Task) -> Result<String> {
    if t.calendar_id.is_empty() {
        t.calendar_id = default_task_list(store)?;
    }
    let track = tracked(store, &t.calendar_id)?;
    store.save_task(&mut t, track)?;
    Ok(t.id)
}

pub fn delete_task(store: &Store, id: &str) -> Result<()> {
    let Some(mut t) = store.task(id)? else { return Ok(()) };
    if tracked(store, &t.calendar_id)? && t.dirty != crate::model::Dirty::Created {
        t.dirty = crate::model::Dirty::Deleted;
        store.put_task(&t)
    } else {
        store.delete_task_row(id)
    }
}

/// Quick add from a sentence.
pub fn quick_add(store: &Store, settings: &Settings, text: &str, calendar_id: Option<&str>) -> Result<Event> {
    let now = Local::now().naive_local();
    let q = crate::nlp::parse(text, now);
    let to_ts = |n: chrono::NaiveDateTime, all_day: bool| -> i64 {
        if all_day {
            n.and_utc().timestamp()
        } else {
            Local.from_local_datetime(&n).earliest().map(|d| d.timestamp()).unwrap_or_else(|| n.and_utc().timestamp())
        }
    };
    let mut ev = new_event(settings, to_ts(q.start, q.all_day), q.all_day);
    ev.end = to_ts(q.end, q.all_day);
    ev.title = q.title;
    ev.location = q.location;
    ev.calendar_id = calendar_id.unwrap_or_default().to_string();
    let id = save_event(store, settings, ev)?;
    store.event(&id)?.ok_or_else(|| anyhow!("the event wasn't saved"))
}

fn iso(ts: i64, all_day: bool) -> String {
    if all_day {
        chrono::DateTime::from_timestamp(ts, 0).unwrap_or_default().format("%Y-%m-%d").to_string()
    } else {
        Local.timestamp_opt(ts, 0).single().map(|d| d.to_rfc3339()).unwrap_or_default()
    }
}

/// The JSON the shell gets for an occurrence: the stored fields plus
/// readable local times.
pub fn occurrence_json(o: &Occurrence) -> Value {
    let mut v = serde_json::to_value(o).unwrap_or_default();
    v["start_iso"] = json!(iso(o.start, o.all_day));
    // All-day ends are exclusive; show the last day.
    v["end_iso"] = json!(iso(if o.all_day { o.end - 86400 } else { o.end }, o.all_day));
    v
}

pub fn occurrences_json(list: &[Occurrence]) -> String {
    Value::Array(list.iter().map(occurrence_json).collect()).to_string()
}

pub fn tasks_json(store: &Store) -> Result<String> {
    let names: std::collections::HashMap<String, String> = store.calendars()?.into_iter().map(|c| (c.id, c.name)).collect();
    let list: Vec<Value> = store
        .tasks(false)?
        .into_iter()
        .map(|t| {
            let mut v = serde_json::to_value(&t).unwrap_or_default();
            v["list"] = json!(names.get(&t.calendar_id).cloned().unwrap_or_default());
            v["due_iso"] = json!(t.due.map(|d| iso(d, true)));
            v
        })
        .collect();
    Ok(Value::Array(list).to_string())
}

pub fn calendars_json(store: &Store) -> Result<String> {
    let accounts = Accounts::load();
    let list: Vec<Value> = store
        .calendars()?
        .into_iter()
        .map(|c| {
            let mut v = serde_json::to_value(&c).unwrap_or_default();
            if let Some(o) = v.as_object_mut() {
                o.remove("sync_token");
                o.remove("ctag");
            }
            v["account"] = json!(accounts.get(&c.account_id).map(|a| a.name.clone()).unwrap_or_default());
            v
        })
        .collect();
    Ok(Value::Array(list).to_string())
}
