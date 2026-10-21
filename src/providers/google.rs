//! Google Calendar API v3 (incremental sync with syncToken) and Google
//! Tasks API v1.

use super::{agent, norm_color, send, seg, status_of, Provider, TokenSource};
use crate::model::{Calendar, CalendarKind, Dirty, Event, Task};
use crate::store::Store;
use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, Utc};
use serde_json::{json, Value};
use std::collections::HashMap;

pub struct Google {
    account_id: String,
    base: String,
    tokens: TokenSource,
    http: ureq::Agent,
    /// Per calendar: its time zone and default reminders.
    defaults: HashMap<String, (String, Vec<i64>)>,
}

const TASKS_PREFIX: &str = "tasks:";

impl Google {
    pub fn new(account_id: &str, base: &str, tokens: TokenSource) -> Self {
        Self {
            account_id: account_id.into(),
            base: base.trim_end_matches('/').into(),
            tokens,
            http: agent(true),
            defaults: HashMap::new(),
        }
    }

    fn get(&mut self, url: &str) -> Result<Value> {
        let auth = self.tokens.bearer()?;
        Ok(send(self.http.get(url).set("Authorization", &auth), None)?.into_json()?)
    }

    fn write(&mut self, method: &str, url: &str, body: &Value) -> Result<Value> {
        let auth = self.tokens.bearer()?;
        let r = send(
            self.http.request(method, url).set("Authorization", &auth).set("Content-Type", "application/json"),
            Some(&body.to_string()),
        )?;
        let text = r.into_string()?;
        Ok(if text.trim().is_empty() { Value::Null } else { serde_json::from_str(&text)? })
    }

    fn delete(&mut self, url: &str) -> Result<()> {
        let auth = self.tokens.bearer()?;
        match send(self.http.delete(url).set("Authorization", &auth), None) {
            Ok(_) => Ok(()),
            // Already gone.
            Err(e) if matches!(status_of(&e), Some(404 | 410)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Follows `nextPageToken` and collects `items`.
    fn pages(&mut self, url: &str) -> Result<(Vec<Value>, Option<String>)> {
        let mut items = Vec::new();
        let mut page: Option<String> = None;
        loop {
            let full = match &page {
                Some(p) => format!("{url}{}pageToken={}", if url.contains('?') { '&' } else { '?' }, seg(p)),
                None => url.to_string(),
            };
            let v = self.get(&full)?;
            if let Some(arr) = v["items"].as_array() {
                items.extend(arr.iter().cloned());
            }
            match v["nextPageToken"].as_str() {
                Some(p) => page = Some(p.to_string()),
                None => return Ok((items, v["nextSyncToken"].as_str().map(String::from))),
            }
        }
    }

    fn events_url(&self, remote: &str) -> String {
        format!("{}/calendar/v3/calendars/{}/events", self.base, seg(remote))
    }

    fn tasks_url(&self, list: &str) -> String {
        format!("{}/tasks/v1/lists/{}/tasks", self.base, seg(list))
    }
}

/// `{"date": …}` or `{"dateTime": …}` → (UTC seconds, all-day).
fn parse_when(v: &Value) -> Option<(i64, bool)> {
    if let Some(d) = v["date"].as_str() {
        let d = NaiveDate::parse_from_str(d, "%Y-%m-%d").ok()?;
        return Some((d.and_hms_opt(0, 0, 0)?.and_utc().timestamp(), true));
    }
    let dt = DateTime::parse_from_rfc3339(v["dateTime"].as_str()?).ok()?;
    Some((dt.timestamp(), false))
}

fn when(ts: i64, ev: &Event) -> Value {
    let dt = DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default();
    if ev.all_day {
        json!({ "date": dt.format("%Y-%m-%d").to_string() })
    } else {
        let tz = if ev.tz.is_empty() { "UTC" } else { ev.tz.as_str() };
        json!({ "dateTime": dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true), "timeZone": tz })
    }
}

/// Reads Google's `recurrence` lines (RRULE, EXDATE) via the iCalendar parser.
fn parse_recurrence(lines: &[Value], start_line: &str, tz: &str) -> (Option<String>, Vec<i64>) {
    let mut text = format!("BEGIN:VCALENDAR\nBEGIN:VEVENT\n{start_line}\n");
    for l in lines.iter().filter_map(Value::as_str) {
        text.push_str(l);
        text.push('\n');
    }
    text.push_str("END:VEVENT\nEND:VCALENDAR\n");
    match crate::ical::parse(&text, tz).events.pop() {
        Some(e) => (e.rrule, e.exdates),
        None => (None, vec![]),
    }
}

fn ical_start(ts: i64, all_day: bool, tz: &str) -> String {
    if all_day {
        format!("DTSTART;VALUE=DATE:{}", crate::ical::fmt_date(ts))
    } else {
        let _ = tz;
        format!("DTSTART:{}", crate::ical::fmt_utc(ts))
    }
}

impl Google {
    /// Converts an API event; `Err(())` items are deletions (uid, instance).
    fn to_event(&self, cal: &Calendar, v: &Value) -> Option<std::result::Result<Event, (String, Option<i64>)>> {
        let id = v["id"].as_str()?.to_string();
        let master = v["recurringEventId"].as_str().map(String::from);
        let original = parse_when(&v["originalStartTime"]).map(|t| t.0);
        if v["status"].as_str() == Some("cancelled") {
            return Some(Err(match master {
                Some(m) => (m, original),
                None => (id, None),
            }));
        }
        let (default_tz, default_reminders) = self.defaults.get(&cal.remote_id).cloned().unwrap_or_default();
        let (start, all_day) = parse_when(&v["start"])?;
        let end = parse_when(&v["end"]).map(|e| e.0).unwrap_or(start);
        let tz = v["start"]["timeZone"].as_str().map(String::from).unwrap_or(if default_tz.is_empty() {
            crate::recur::local_zone_name()
        } else {
            default_tz
        });
        let (rrule, exdates) = match v["recurrence"].as_array() {
            Some(lines) => parse_recurrence(lines, &ical_start(start, all_day, &tz), &tz),
            None => (None, vec![]),
        };
        let reminders = if v["reminders"]["useDefault"].as_bool().unwrap_or(false) {
            default_reminders
        } else {
            v["reminders"]["overrides"]
                .as_array()
                .map(|a| a.iter().filter_map(|r| r["minutes"].as_i64()).collect())
                .unwrap_or_default()
        };
        Some(Ok(Event {
            id: String::new(),
            calendar_id: cal.id.clone(),
            uid: master.clone().unwrap_or(id),
            title: v["summary"].as_str().unwrap_or("(No title)").to_string(),
            location: v["location"].as_str().unwrap_or_default().to_string(),
            description: v["description"].as_str().unwrap_or_default().to_string(),
            start,
            end,
            all_day,
            tz: if all_day { String::new() } else { tz },
            rrule,
            exdates,
            recurrence_id: master.and(original),
            reminders,
            etag: v["etag"].as_str().map(String::from),
            href: None,
            dirty: Dirty::Clean,
        }))
    }

    fn event_body(ev: &Event) -> Value {
        let mut recurrence = Vec::new();
        if let Some(r) = &ev.rrule {
            recurrence.push(format!("RRULE:{}", r.trim_start_matches("RRULE:")));
            for ex in &ev.exdates {
                recurrence.push(if ev.all_day {
                    format!("EXDATE;VALUE=DATE:{}", crate::ical::fmt_date(*ex))
                } else {
                    format!("EXDATE:{}", crate::ical::fmt_utc(*ex))
                });
            }
        }
        json!({
            "summary": ev.title,
            "location": ev.location,
            "description": ev.description,
            "start": when(ev.start, ev),
            "end": when(ev.end.max(ev.start + if ev.all_day { 86400 } else { 0 }), ev),
            "recurrence": recurrence,
            "reminders": {
                "useDefault": false,
                "overrides": ev.reminders.iter().map(|m| json!({"method": "popup", "minutes": m})).collect::<Vec<_>>(),
            },
        })
    }
}

impl Provider for Google {
    fn calendars(&mut self) -> Result<Vec<Calendar>> {
        let url = format!("{}/calendar/v3/users/me/calendarList?maxResults=250", self.base);
        let (items, _) = self.pages(&url).context("listing Google calendars")?;
        let mut out = Vec::new();
        for (i, c) in items.iter().enumerate() {
            let Some(remote) = c["id"].as_str() else { continue };
            if c["deleted"].as_bool() == Some(true) {
                continue;
            }
            let reminders = c["defaultReminders"]
                .as_array()
                .map(|a| a.iter().filter_map(|r| r["minutes"].as_i64()).collect())
                .unwrap_or_default();
            self.defaults
                .insert(remote.to_string(), (c["timeZone"].as_str().unwrap_or_default().to_string(), reminders));
            let role = c["accessRole"].as_str().unwrap_or("reader");
            out.push(Calendar {
                id: Calendar::make_id(&self.account_id, remote),
                account_id: self.account_id.clone(),
                remote_id: remote.into(),
                name: c["summaryOverride"].as_str().or(c["summary"].as_str()).unwrap_or(remote).to_string(),
                color: norm_color(c["backgroundColor"].as_str().unwrap_or_default(), i),
                visible: c["selected"].as_bool().unwrap_or(true),
                read_only: !matches!(role, "owner" | "writer"),
                kind: CalendarKind::Events,
                sync_token: None,
                ctag: None,
            });
        }
        // Task lists need the Tasks API enabled for the client; skip quietly
        // when it isn't.
        match self.pages(&format!("{}/tasks/v1/users/@me/lists?maxResults=100", self.base)) {
            Ok((lists, _)) => {
                for l in lists {
                    let Some(id) = l["id"].as_str() else { continue };
                    let remote = format!("{TASKS_PREFIX}{id}");
                    out.push(Calendar {
                        id: Calendar::make_id(&self.account_id, &remote),
                        account_id: self.account_id.clone(),
                        remote_id: remote,
                        name: l["title"].as_str().unwrap_or("Tasks").to_string(),
                        color: "#1E88E5".into(),
                        visible: true,
                        kind: CalendarKind::Tasks,
                        ..Default::default()
                    });
                }
            }
            Err(e) => log::info!("Google Tasks unavailable: {e:#}"),
        }
        Ok(out)
    }

    fn sync_events(&mut self, cal: &Calendar, store: &Store) -> Result<()> {
        if self.defaults.is_empty() {
            let _ = self.calendars();
        }
        let base = format!("{}?maxResults=250&showDeleted=true", self.events_url(&cal.remote_id));
        let (items, token, full) = match &cal.sync_token {
            Some(t) => match self.pages(&format!("{base}&syncToken={}", seg(t))) {
                Ok((items, token)) => (items, token, false),
                // 410: the token expired, start over.
                Err(e) if status_of(&e) == Some(410) => {
                    let (items, token) = self.pages(&base)?;
                    (items, token, true)
                }
                Err(e) => return Err(e),
            },
            None => {
                let (items, token) = self.pages(&base)?;
                (items, token, true)
            }
        };
        if full {
            store.clear_calendar_events(&cal.id)?;
        }
        // Masters first, so instance cancellations find them.
        let mut deferred = Vec::new();
        for v in &items {
            match self.to_event(cal, v) {
                Some(Ok(ev)) => store.apply_remote_event(ev)?,
                Some(Err(del)) => deferred.push(del),
                None => {}
            }
        }
        for (uid, instance) in deferred {
            match instance {
                None => store.apply_remote_delete(&cal.id, &uid, None)?,
                Some(start) => {
                    // A deleted instance: drop its override and skip it.
                    store.apply_remote_delete(&cal.id, &uid, Some(start))?;
                    if let Some(mut master) = store
                        .events_by_uid(&cal.id, &uid)?
                        .into_iter()
                        .find(|e| e.recurrence_id.is_none() && e.dirty == Dirty::Clean)
                    {
                        if !master.exdates.contains(&start) {
                            master.exdates.push(start);
                            store.put_event(&master)?;
                        }
                    }
                }
            }
        }
        store.set_sync_state(&cal.id, token.as_deref(), None)?;
        Ok(())
    }

    fn push_event(&mut self, cal: &Calendar, ev: &Event, store: &Store) -> Result<()> {
        let body = Self::event_body(ev);
        let url = self.events_url(&cal.remote_id);
        let created = ev.dirty == Dirty::Created;
        let res = if created {
            self.write("POST", &url, &body)?
        } else {
            match self.write("PUT", &format!("{url}/{}", seg(&ev.uid)), &body) {
                Err(e) if status_of(&e) == Some(404) => self.write("POST", &url, &body)?,
                other => other?,
            }
        };
        let mut done = ev.clone();
        if let Some(id) = res["id"].as_str() {
            done.uid = id.to_string();
        }
        done.etag = res["etag"].as_str().map(String::from);
        done.dirty = Dirty::Clean;
        store.put_event(&done)
    }

    fn delete_event(&mut self, cal: &Calendar, ev: &Event) -> Result<()> {
        let url = format!("{}/{}", self.events_url(&cal.remote_id), seg(&ev.uid));
        self.delete(&url)
    }

    fn sync_tasks(&mut self, cal: &Calendar, store: &Store) -> Result<()> {
        let list = cal.remote_id.trim_start_matches(TASKS_PREFIX).to_string();
        let url = format!("{}?showCompleted=true&showHidden=true&maxResults=100", self.tasks_url(&list));
        let (items, _) = self.pages(&url)?;
        let tasks = items
            .iter()
            .filter(|t| t["deleted"].as_bool() != Some(true) && !t["title"].as_str().unwrap_or_default().is_empty())
            .filter_map(|t| {
                Some(Task {
                    calendar_id: cal.id.clone(),
                    uid: t["id"].as_str()?.to_string(),
                    title: t["title"].as_str().unwrap_or_default().to_string(),
                    notes: t["notes"].as_str().unwrap_or_default().to_string(),
                    due: t["due"].as_str().and_then(|d| DateTime::parse_from_rfc3339(d).ok()).map(|d| {
                        let s = d.timestamp();
                        s - s.rem_euclid(86400)
                    }),
                    completed: t["status"].as_str() == Some("completed"),
                    etag: t["etag"].as_str().map(String::from),
                    ..Default::default()
                })
            })
            .collect();
        store.replace_remote_tasks(&cal.id, tasks)
    }

    fn push_task(&mut self, cal: &Calendar, t: &Task, store: &Store) -> Result<()> {
        let list = cal.remote_id.trim_start_matches(TASKS_PREFIX).to_string();
        let mut body = json!({
            "title": t.title,
            "notes": t.notes,
            "status": if t.completed { "completed" } else { "needsAction" },
        });
        if let Some(d) = t.due {
            body["due"] = json!(DateTime::<Utc>::from_timestamp(d, 0).unwrap_or_default().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
        }
        let url = self.tasks_url(&list);
        let res = if t.dirty == Dirty::Created {
            self.write("POST", &url, &body)?
        } else {
            self.write("PATCH", &format!("{url}/{}", seg(&t.uid)), &body)?
        };
        let mut done = t.clone();
        if let Some(id) = res["id"].as_str() {
            done.uid = id.into();
        }
        done.dirty = Dirty::Clean;
        store.put_task(&done)
    }

    fn delete_task(&mut self, cal: &Calendar, t: &Task) -> Result<()> {
        let list = cal.remote_id.trim_start_matches(TASKS_PREFIX).to_string();
        let url = format!("{}/{}", self.tasks_url(&list), seg(&t.uid));
        self.delete(&url)
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::MockServer;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn full_then_incremental_sync_and_push() {
        let round = Arc::new(AtomicUsize::new(0));
        let r = round.clone();
        let server = MockServer::start(move |req| {
            assert_eq!(req.headers.get("authorization").map(String::as_str), Some("Bearer tok"));
            let p = req.path.as_str();
            if p.starts_with("/calendar/v3/users/me/calendarList") {
                return (200, json!({"items": [
                    {"id": "me@example.com", "summary": "me@example.com", "backgroundColor": "#9fe1e7", "accessRole": "owner", "timeZone": "Europe/Berlin", "defaultReminders": [{"method": "popup", "minutes": 30}]},
                    {"id": "en.usa#holiday@group.v.calendar.google.com", "summary": "Holidays", "accessRole": "reader"}
                ]}).to_string());
            }
            if p.starts_with("/tasks/v1/users/@me/lists") {
                return (200, json!({"items": [{"id": "L1", "title": "My Tasks"}]}).to_string());
            }
            if p.starts_with("/tasks/v1/lists/L1/tasks") {
                return (200, json!({"items": [
                    {"id": "t1", "title": "Pay rent", "status": "needsAction", "due": "2026-11-01T00:00:00.000Z"},
                    {"id": "t2", "title": "Done thing", "status": "completed"}
                ]}).to_string());
            }
            if req.method == "POST" && p.starts_with("/calendar/v3/calendars/me%40example.com/events") {
                let body: Value = serde_json::from_str(&req.body).unwrap();
                assert_eq!(body["summary"], "New one");
                assert_eq!(body["start"]["timeZone"], "Europe/Berlin");
                assert_eq!(body["recurrence"][0], "RRULE:FREQ=WEEKLY");
                return (200, json!({"id": "srv1", "etag": "\"e1\""}).to_string());
            }
            if req.method == "DELETE" {
                return (204, String::new());
            }
            if p.starts_with("/calendar/v3/calendars/me%40example.com/events") {
                if p.contains("syncToken=S1") {
                    r.fetch_add(1, Ordering::SeqCst);
                    return (200, json!({"items": [
                        {"id": "e1", "status": "cancelled"},
                        {"id": "r1_20261102", "status": "cancelled", "recurringEventId": "r1", "originalStartTime": {"dateTime": "2026-11-02T09:00:00+01:00"}}
                    ], "nextSyncToken": "S2"}).to_string());
                }
                if !p.contains("pageToken") {
                    return (200, json!({"items": [
                        {"id": "e1", "status": "confirmed", "summary": "Dentist", "etag": "\"a\"",
                         "start": {"dateTime": "2026-10-21T15:00:00+02:00"}, "end": {"dateTime": "2026-10-21T16:00:00+02:00"},
                         "reminders": {"useDefault": true}}
                    ], "nextPageToken": "P2"}).to_string());
                }
                return (200, json!({"items": [
                    {"id": "r1", "summary": "Standup", "start": {"dateTime": "2026-10-19T09:00:00+02:00", "timeZone": "Europe/Berlin"},
                     "end": {"dateTime": "2026-10-19T09:15:00+02:00", "timeZone": "Europe/Berlin"},
                     "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO"], "reminders": {"useDefault": false, "overrides": [{"method": "popup", "minutes": 5}]}},
                    {"id": "r1_20261026", "summary": "Standup (later)", "recurringEventId": "r1",
                     "originalStartTime": {"dateTime": "2026-10-26T09:00:00+01:00"},
                     "start": {"dateTime": "2026-10-26T11:00:00+01:00"}, "end": {"dateTime": "2026-10-26T11:15:00+01:00"}},
                    {"id": "h1", "summary": "Party", "start": {"date": "2026-10-31"}, "end": {"date": "2026-11-01"}}
                ], "nextSyncToken": "S1"}).to_string());
            }
            (404, format!("unexpected {p}"))
        });
        let store = Store::in_memory().unwrap();
        let mut g = Google::new("g", &server.url, TokenSource::fixed("tok"));
        let cals = g.calendars().unwrap();
        assert_eq!(cals.len(), 3);
        assert!(cals[1].read_only);
        assert_eq!(cals[0].color, "#9FE1E7");
        assert_eq!(cals[2].kind, CalendarKind::Tasks);
        store.sync_calendars("g", &cals).unwrap();
        let cal = store.calendar("g/me@example.com").unwrap().unwrap();
        g.sync_events(&cal, &store).unwrap();
        let evs = store.events_in_calendar(&cal.id).unwrap();
        assert_eq!(evs.len(), 4);
        let dentist = evs.iter().find(|e| e.title == "Dentist").unwrap();
        assert_eq!(dentist.reminders, vec![30]);
        assert_eq!(dentist.tz, "Europe/Berlin");
        let party = evs.iter().find(|e| e.title == "Party").unwrap();
        assert!(party.all_day);
        let master = evs.iter().find(|e| e.uid == "r1" && e.recurrence_id.is_none()).unwrap();
        assert_eq!(master.rrule.as_deref(), Some("FREQ=WEEKLY;BYDAY=MO"));
        // Oct 19 .. Nov 9: three Mondays, the 26th moved to 11:00.
        let from = chrono::DateTime::parse_from_rfc3339("2026-10-19T00:00:00Z").unwrap().timestamp();
        let occ: Vec<_> = store.occurrences(from, from + 21 * 86400, true).unwrap().into_iter().filter(|o| o.title.starts_with("Standup")).collect();
        assert_eq!(occ.len(), 3);
        assert_eq!(occ[1].title, "Standup (later)");

        // Incremental: e1 deleted, Nov 2 instance cancelled.
        let cal = store.calendar(&cal.id).unwrap().unwrap();
        assert_eq!(cal.sync_token.as_deref(), Some("S1"));
        g.sync_events(&cal, &store).unwrap();
        assert_eq!(round.load(Ordering::SeqCst), 1);
        assert!(store.events_in_calendar(&cal.id).unwrap().iter().all(|e| e.title != "Dentist"));
        let occ: Vec<_> = store.occurrences(from, from + 21 * 86400, true).unwrap().into_iter().filter(|o| o.title.starts_with("Standup")).collect();
        assert_eq!(occ.len(), 2);
        assert_eq!(store.calendar(&cal.id).unwrap().unwrap().sync_token.as_deref(), Some("S2"));

        // Push a new recurring event.
        let mut ev = Event {
            calendar_id: cal.id.clone(),
            title: "New one".into(),
            start: from,
            end: from + 3600,
            tz: "Europe/Berlin".into(),
            rrule: Some("FREQ=WEEKLY".into()),
            ..Default::default()
        };
        store.save_event(&mut ev, true).unwrap();
        g.push_event(&cal, &store.event(&ev.id).unwrap().unwrap(), &store).unwrap();
        let pushed = store.event(&ev.id).unwrap().unwrap();
        assert_eq!(pushed.uid, "srv1");
        assert_eq!(pushed.dirty, Dirty::Clean);
        g.delete_event(&cal, &pushed).unwrap();

        // Tasks.
        let list = store.calendar("g/tasks:L1").unwrap().unwrap();
        g.sync_tasks(&list, &store).unwrap();
        let tasks = store.tasks(true).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].title, "Pay rent");
        assert!(tasks[1].completed);
    }
}
