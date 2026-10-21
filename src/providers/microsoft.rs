//! Microsoft Graph: Outlook calendars through calendarView delta queries
//! (which return each occurrence of a series as its own event, over a
//! window around today) and Microsoft To Do lists.

use super::{agent, norm_color, path_segment, send, status_of, Provider, TokenSource, USER_AGENT};
use crate::model::{Calendar, CalendarKind, Dirty, Event, Task};
use crate::store::Store;
use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, NaiveDateTime, Utc};
use serde_json::{json, Value};

pub struct Microsoft {
    account_id: String,
    base: String,
    tokens: TokenSource,
    http: ureq::Agent,
}

const TODO_PREFIX: &str = "todo:";
/// Days of history and future the delta window covers.
const PAST_DAYS: i64 = 90;
const FUTURE_DAYS: i64 = 400;

impl Microsoft {
    pub fn new(account_id: &str, base: &str, tokens: TokenSource) -> Self {
        Self { account_id: account_id.into(), base: base.trim_end_matches('/').into(), tokens, http: agent(USER_AGENT, true) }
    }

    fn get(&mut self, url: &str) -> Result<Value> {
        let auth = self.tokens.bearer()?;
        let req = self
            .http
            .get(url)
            .set("Authorization", &auth)
            .set("Prefer", "outlook.timezone=\"UTC\", odata.maxpagesize=200");
        Ok(send(req, None)?.into_json()?)
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
            Err(e) if !matches!(status_of(&e), Some(404 | 410)) => Err(e),
            _ => Ok(()),
        }
    }

    /// Follows `@odata.nextLink`; returns the items and the deltaLink.
    fn pages(&mut self, url: &str) -> Result<(Vec<Value>, Option<String>)> {
        let mut items = Vec::new();
        let mut next = url.to_string();
        loop {
            let v = self.get(&next)?;
            if let Some(arr) = v["value"].as_array() {
                items.extend(arr.iter().cloned());
            }
            match v["@odata.nextLink"].as_str() {
                Some(n) => next = n.to_string(),
                None => return Ok((items, v["@odata.deltaLink"].as_str().map(String::from))),
            }
        }
    }
}

/// Graph's `{"dateTime": "2026-10-21T09:00:00.0000000", "timeZone": "UTC"}`.
fn parse_graph_time(v: &Value) -> Option<i64> {
    let s = v["dateTime"].as_str()?;
    let s = s.split('.').next()?;
    let naive = NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok()?;
    let tz = v["timeZone"].as_str().unwrap_or("UTC");
    match tz.parse::<chrono_tz::Tz>() {
        Ok(zone) if tz != "UTC" => {
            use chrono::TimeZone;
            zone.from_local_datetime(&naive).earliest().map(|d| d.timestamp())
        }
        _ => Some(naive.and_utc().timestamp()),
    }
}

fn graph_time(ts: i64) -> Value {
    let dt = DateTime::<Utc>::from_timestamp(ts, 0).unwrap_or_default();
    json!({ "dateTime": dt.format("%Y-%m-%dT%H:%M:%S").to_string(), "timeZone": "UTC" })
}

/// The editor's repeat presets as a Graph patternedRecurrence.
fn graph_recurrence(ev: &Event) -> Option<Value> {
    let rule = ev.rrule.as_deref()?.to_ascii_uppercase();
    let get = |k: &str| rule.split(';').find_map(|p| p.strip_prefix(&format!("{k}=")).map(String::from));
    let interval: i64 = get("INTERVAL").and_then(|i| i.parse().ok()).unwrap_or(1);
    let start = DateTime::<Utc>::from_timestamp(ev.start, 0)?;
    let names = ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"];
    let day_name = |d: &str| match d.trim_start_matches(|c: char| c.is_ascii_digit() || c == '-' || c == '+') {
        "MO" => Some(names[0]),
        "TU" => Some(names[1]),
        "WE" => Some(names[2]),
        "TH" => Some(names[3]),
        "FR" => Some(names[4]),
        "SA" => Some(names[5]),
        "SU" => Some(names[6]),
        _ => None,
    };
    let pattern = match get("FREQ")?.as_str() {
        "DAILY" => json!({"type": "daily", "interval": interval}),
        "WEEKLY" => {
            let days: Vec<&str> = match get("BYDAY") {
                Some(list) => list.split(',').filter_map(day_name).collect(),
                None => vec![names[start.weekday().num_days_from_monday() as usize]],
            };
            json!({"type": "weekly", "interval": interval, "daysOfWeek": days, "firstDayOfWeek": "monday"})
        }
        "MONTHLY" => json!({"type": "absoluteMonthly", "interval": interval, "dayOfMonth": start.day()}),
        "YEARLY" => json!({"type": "absoluteYearly", "interval": interval, "dayOfMonth": start.day(), "month": start.month()}),
        _ => return None,
    };
    let start_date = start.format("%Y-%m-%d").to_string();
    let range = match (get("COUNT"), get("UNTIL")) {
        (Some(n), _) => json!({"type": "numbered", "startDate": start_date, "numberOfOccurrences": n.parse::<i64>().unwrap_or(1)}),
        (None, Some(u)) => json!({"type": "endDate", "startDate": start_date, "endDate": format!("{}-{}-{}", &u[0..4], &u[4..6], &u[6..8])}),
        _ => json!({"type": "noEnd", "startDate": start_date}),
    };
    Some(json!({"pattern": pattern, "range": range}))
}

impl Microsoft {
    fn to_event(cal: &Calendar, v: &Value) -> Option<Event> {
        let start = parse_graph_time(&v["start"])?;
        let end = parse_graph_time(&v["end"]).unwrap_or(start);
        let all_day = v["isAllDay"].as_bool().unwrap_or(false);
        let mut reminders = vec![];
        if v["isReminderOn"].as_bool().unwrap_or(false) {
            reminders.push(v["reminderMinutesBeforeStart"].as_i64().unwrap_or(15));
        }
        Some(Event {
            calendar_id: cal.id.clone(),
            uid: v["id"].as_str()?.into(),
            title: v["subject"].as_str().filter(|s| !s.is_empty()).unwrap_or("(No title)").into(),
            location: v["location"]["displayName"].as_str().unwrap_or_default().into(),
            description: v["bodyPreview"].as_str().unwrap_or_default().into(),
            start,
            end,
            all_day,
            tz: if all_day { String::new() } else { crate::recur::local_zone_name() },
            reminders,
            etag: v["@odata.etag"].as_str().map(String::from),
            ..Default::default()
        })
    }

    fn event_body(ev: &Event, include_recurrence: bool) -> Value {
        let mut body = json!({
            "subject": ev.title,
            "body": {"contentType": "text", "content": ev.description},
            "start": graph_time(ev.start),
            "end": graph_time(ev.end.max(ev.start + if ev.all_day { 86400 } else { 0 })),
            "isAllDay": ev.all_day,
            "location": {"displayName": ev.location},
            "isReminderOn": !ev.reminders.is_empty(),
            "reminderMinutesBeforeStart": ev.reminders.first().copied().unwrap_or(0),
        });
        if include_recurrence {
            if let Some(r) = graph_recurrence(ev) {
                body["recurrence"] = r;
            }
        }
        body
    }

    fn list_id(cal: &Calendar) -> String {
        cal.remote_id.trim_start_matches(TODO_PREFIX).to_string()
    }
}

/// The signed-in user's address.
pub fn account_email(base: &str, access_token: &str) -> Result<String> {
    let url = format!("{}/me", base.trim_end_matches('/'));
    let v: Value = send(agent(USER_AGENT, true).get(&url).set("Authorization", &format!("Bearer {access_token}")), None)?.into_json()?;
    Ok(v["mail"].as_str().or(v["userPrincipalName"].as_str()).unwrap_or("Microsoft account").to_string())
}

impl Provider for Microsoft {
    fn calendars(&mut self) -> Result<Vec<Calendar>> {
        let (items, _) = self.pages(&format!("{}/me/calendars", self.base)).context("listing Outlook calendars")?;
        let mut out: Vec<Calendar> = items
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let remote = c["id"].as_str()?;
                Some(Calendar {
                    id: Calendar::make_id(&self.account_id, remote),
                    account_id: self.account_id.clone(),
                    remote_id: remote.into(),
                    name: c["name"].as_str().unwrap_or("Calendar").into(),
                    color: norm_color(c["hexColor"].as_str().unwrap_or_default(), i),
                    visible: true,
                    read_only: !c["canEdit"].as_bool().unwrap_or(true),
                    kind: CalendarKind::Events,
                    ..Default::default()
                })
            })
            .collect();
        match self.pages(&format!("{}/me/todo/lists", self.base)) {
            Ok((lists, _)) => {
                for l in lists {
                    let Some(id) = l["id"].as_str() else { continue };
                    let remote = format!("{TODO_PREFIX}{id}");
                    out.push(Calendar {
                        id: Calendar::make_id(&self.account_id, &remote),
                        account_id: self.account_id.clone(),
                        remote_id: remote,
                        name: l["displayName"].as_str().unwrap_or("Tasks").into(),
                        color: "#2564CF".into(),
                        visible: true,
                        kind: CalendarKind::Tasks,
                        ..Default::default()
                    });
                }
            }
            Err(e) => log::info!("Microsoft To Do unavailable: {e:#}"),
        }
        Ok(out)
    }

    fn sync_events(&mut self, cal: &Calendar, store: &Store) -> Result<()> {
        let now = Utc::now().timestamp();
        let fmt = |t: i64| DateTime::<Utc>::from_timestamp(t, 0).unwrap_or_default().format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let fresh = format!(
            "{}/me/calendars/{}/calendarView/delta?startDateTime={}&endDateTime={}",
            self.base,
            path_segment(&cal.remote_id),
            fmt(now - PAST_DAYS * 86400),
            fmt(now + FUTURE_DAYS * 86400)
        );
        let (items, delta, full) = match &cal.sync_token {
            Some(link) => match self.pages(link) {
                Ok((i, d)) => (i, d, false),
                Err(e) if matches!(status_of(&e), Some(410)) => {
                    let (i, d) = self.pages(&fresh)?;
                    (i, d, true)
                }
                Err(e) => return Err(e),
            },
            None => {
                let (i, d) = self.pages(&fresh)?;
                (i, d, true)
            }
        };
        if full {
            store.clear_calendar_events(&cal.id)?;
        }
        for v in &items {
            let Some(id) = v["id"].as_str() else { continue };
            if !v["@removed"].is_null() || v["isCancelled"].as_bool() == Some(true) {
                store.apply_remote_delete(&cal.id, id, None)?;
            } else if let Some(ev) = Self::to_event(cal, v) {
                store.apply_remote_event(ev)?;
            }
        }
        store.set_sync_state(&cal.id, delta.as_deref(), None)?;
        Ok(())
    }

    fn push_event(&mut self, cal: &Calendar, ev: &Event, store: &Store) -> Result<()> {
        let res = if ev.dirty == Dirty::Created {
            let url = format!("{}/me/calendars/{}/events", self.base, path_segment(&cal.remote_id));
            self.write("POST", &url, &Self::event_body(ev, true))?
        } else {
            let url = format!("{}/me/events/{}", self.base, path_segment(&ev.uid));
            self.write("PATCH", &url, &Self::event_body(ev, false))?
        };
        let mut done = ev.clone();
        if let Some(id) = res["id"].as_str() {
            done.uid = id.into();
        }
        done.etag = res["@odata.etag"].as_str().map(String::from);
        done.dirty = Dirty::Clean;
        store.put_event(&done)?;
        // A new series comes back as occurrences on the next delta; drop
        // the local master so it doesn't show twice.
        if ev.dirty == Dirty::Created && ev.rrule.is_some() {
            store.delete_event_row(&done.id)?;
            store.set_sync_state(&cal.id, None, None)?;
        }
        Ok(())
    }

    fn delete_event(&mut self, _cal: &Calendar, ev: &Event) -> Result<()> {
        let url = format!("{}/me/events/{}", self.base, path_segment(&ev.uid));
        self.delete(&url)
    }

    fn sync_tasks(&mut self, cal: &Calendar, store: &Store) -> Result<()> {
        let url = format!("{}/me/todo/lists/{}/tasks", self.base, path_segment(&Self::list_id(cal)));
        let (items, _) = self.pages(&url)?;
        let tasks = items
            .iter()
            .filter_map(|t| {
                Some(Task {
                    calendar_id: cal.id.clone(),
                    uid: t["id"].as_str()?.into(),
                    title: t["title"].as_str().unwrap_or_default().into(),
                    notes: t["body"]["content"].as_str().unwrap_or_default().into(),
                    due: parse_graph_time(&t["dueDateTime"]).map(|d| d - d.rem_euclid(86400)),
                    completed: t["status"].as_str() == Some("completed"),
                    etag: t["@odata.etag"].as_str().map(String::from),
                    ..Default::default()
                })
            })
            .collect();
        store.replace_remote_tasks(&cal.id, tasks)
    }

    fn push_task(&mut self, cal: &Calendar, t: &Task, store: &Store) -> Result<()> {
        let mut body = json!({
            "title": t.title,
            "body": {"contentType": "text", "content": t.notes},
            "status": if t.completed { "completed" } else { "notStarted" },
        });
        if let Some(d) = t.due {
            body["dueDateTime"] = graph_time(d);
        }
        let list = format!("{}/me/todo/lists/{}/tasks", self.base, path_segment(&Self::list_id(cal)));
        let res = if t.dirty == Dirty::Created {
            self.write("POST", &list, &body)?
        } else {
            self.write("PATCH", &format!("{list}/{}", path_segment(&t.uid)), &body)?
        };
        let mut done = t.clone();
        if let Some(id) = res["id"].as_str() {
            done.uid = id.into();
        }
        done.dirty = Dirty::Clean;
        store.put_task(&done)
    }

    fn delete_task(&mut self, cal: &Calendar, t: &Task) -> Result<()> {
        let url = format!("{}/me/todo/lists/{}/tasks/{}", self.base, path_segment(&Self::list_id(cal)), path_segment(&t.uid));
        self.delete(&url)
    }
}

#[cfg(test)]
mod tests {
    use material_3_ui::testing::MockServer;
    use super::*;

    #[test]
    fn delta_sync_and_push() {
        let base = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let b2 = base.clone();
        let server = MockServer::start(move |req| {
            let link = |s: &str| format!("{}/{s}", b2.lock().unwrap());
            let p = req.path.as_str();
            assert!(req.headers.get("authorization").is_some_and(|a| a == "Bearer tok"));
            if p == "/me/calendars" {
                return (200, json!({"value": [{"id": "C1", "name": "Calendar", "hexColor": "", "canEdit": true}]}).to_string());
            }
            if p == "/me/todo/lists" {
                return (200, json!({"value": [{"id": "T1", "displayName": "Tasks"}]}).to_string());
            }
            if p.starts_with("/me/calendars/C1/calendarView/delta?startDateTime=") {
                assert!(req.headers.get("prefer").is_some_and(|h| h.contains("outlook.timezone")));
                return (200, json!({"value": [
                    {"id": "A", "subject": "Planning", "start": {"dateTime": "2026-10-21T09:00:00.0000000", "timeZone": "UTC"},
                     "end": {"dateTime": "2026-10-21T10:00:00.0000000", "timeZone": "UTC"}, "isAllDay": false,
                     "isReminderOn": true, "reminderMinutesBeforeStart": 15, "location": {"displayName": "Room 4"}}
                ], "@odata.nextLink": link("NEXT")}).to_string());
            }
            if p == "/NEXT" {
                return (200, json!({"value": [
                    {"id": "B", "subject": "Holiday", "start": {"dateTime": "2026-10-23T00:00:00.0000000", "timeZone": "UTC"},
                     "end": {"dateTime": "2026-10-24T00:00:00.0000000", "timeZone": "UTC"}, "isAllDay": true}
                ], "@odata.deltaLink": link("DELTA1")}).to_string());
            }
            if p == "/DELTA1" {
                return (200, json!({"value": [{"id": "A", "@removed": {"reason": "deleted"}}], "@odata.deltaLink": link("DELTA2")}).to_string());
            }
            if p == "/me/calendars/C1/events" && req.method == "POST" {
                let b: Value = serde_json::from_str(&req.body).unwrap();
                assert_eq!(b["subject"], "Lunch");
                assert_eq!(b["recurrence"]["pattern"]["type"], "weekly");
                return (201, json!({"id": "N1", "@odata.etag": "W/\"1\""}).to_string());
            }
            if p == "/me/todo/lists/T1/tasks" {
                return (200, json!({"value": [{"id": "x", "title": "Call bank", "status": "notStarted",
                    "dueDateTime": {"dateTime": "2026-10-30T00:00:00.0000000", "timeZone": "UTC"}}]}).to_string());
            }
            (404, format!("unexpected {p}"))
        });
        // nextLink/deltaLink are absolute in Graph.
        *base.lock().unwrap() = server.url.clone();
        let store = Store::in_memory().unwrap();
        let mut m = Microsoft::new("m", &server.url, TokenSource::fixed("tok"));
        let cals = m.calendars().unwrap();
        assert_eq!(cals.len(), 2);
        store.sync_calendars("m", &cals).unwrap();
        let cal = store.calendar("m/C1").unwrap().unwrap();
        m.sync_events(&cal, &store).unwrap_or_else(|e| panic!("{e:#}"));
        let evs = store.events_in_calendar(&cal.id).unwrap();
        assert_eq!(evs.len(), 2);
        let a = evs.iter().find(|e| e.uid == "A").unwrap();
        assert_eq!(a.reminders, vec![15]);
        assert_eq!(a.location, "Room 4");
        assert_eq!(a.end - a.start, 3600);
        assert!(evs.iter().find(|e| e.uid == "B").unwrap().all_day);
        let cal = store.calendar(&cal.id).unwrap().unwrap();
        assert_eq!(cal.sync_token, Some(format!("{}/DELTA1", server.url)));
        m.sync_events(&cal, &store).unwrap();
        assert_eq!(store.events_in_calendar(&cal.id).unwrap().len(), 1);

        let mut ev = Event {
            calendar_id: cal.id.clone(),
            title: "Lunch".into(),
            start: 1_800_000_000,
            end: 1_800_003_600,
            rrule: Some("FREQ=WEEKLY".into()),
            ..Default::default()
        };
        store.save_event(&mut ev, true).unwrap();
        m.push_event(&cal, &store.event(&ev.id).unwrap().unwrap(), &store).unwrap();
        // The series comes back with the next delta.
        assert!(store.event(&ev.id).unwrap().is_none());

        let list = store.calendar("m/todo:T1").unwrap().unwrap();
        m.sync_tasks(&list, &store).unwrap();
        assert_eq!(store.tasks(false).unwrap()[0].title, "Call bank");
    }
}
