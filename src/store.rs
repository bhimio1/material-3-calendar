//! The local cache: one SQLite file shared by the daemon, the app and the
//! command line (WAL mode, so they can all have it open). Every write bumps
//! a revision counter that the app polls to know when to reload.

use crate::model::{Calendar, CalendarKind, Dirty, Event, Occurrence, Task};
use crate::recur;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub fn default_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| material_3_ui::sys::paths::home().join(".local/share"))
        .join("m3-calendar/cache.db")
}

pub struct Store {
    conn: Connection,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct AccountStatus {
    pub last_sync: Option<i64>,
    pub error: Option<String>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS calendars (
    id TEXT PRIMARY KEY, account_id TEXT NOT NULL, remote_id TEXT NOT NULL,
    name TEXT NOT NULL, color TEXT NOT NULL, visible INTEGER NOT NULL DEFAULT 1,
    read_only INTEGER NOT NULL DEFAULT 0, kind TEXT NOT NULL DEFAULT 'events',
    sync_token TEXT, ctag TEXT, position INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS events (
    id TEXT PRIMARY KEY, calendar_id TEXT NOT NULL, uid TEXT NOT NULL,
    title TEXT NOT NULL, location TEXT NOT NULL DEFAULT '', description TEXT NOT NULL DEFAULT '',
    start INTEGER NOT NULL, end INTEGER NOT NULL, all_day INTEGER NOT NULL DEFAULT 0,
    tz TEXT NOT NULL DEFAULT '', rrule TEXT, exdates TEXT NOT NULL DEFAULT '[]',
    recurrence_id INTEGER, reminders TEXT NOT NULL DEFAULT '[]',
    etag TEXT, href TEXT, dirty INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS events_calendar ON events (calendar_id, uid);
CREATE INDEX IF NOT EXISTS events_time ON events (start, end);
CREATE TABLE IF NOT EXISTS tasks (
    id TEXT PRIMARY KEY, calendar_id TEXT NOT NULL, uid TEXT NOT NULL,
    title TEXT NOT NULL, notes TEXT NOT NULL DEFAULT '', due INTEGER,
    completed INTEGER NOT NULL DEFAULT 0, etag TEXT, href TEXT, dirty INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS tasks_calendar ON tasks (calendar_id, uid);
CREATE TABLE IF NOT EXISTS fired (key TEXT PRIMARY KEY, at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS account_status (account_id TEXT PRIMARY KEY, last_sync INTEGER, error TEXT);
";

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn open_default() -> Result<Self> {
        Self::open(&default_path())
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    // ---- meta ----

    pub fn meta(&self, key: &str) -> Option<String> {
        self.conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
    }

    /// Increases on every change; readers compare it to know when to reload.
    pub fn revision(&self) -> i64 {
        self.meta("revision").and_then(|v| v.parse().ok()).unwrap_or(0)
    }

    pub fn bump(&self) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('revision', '1')
             ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
            [],
        )?;
        Ok(())
    }

    // ---- calendars ----

    fn calendar_from_row(r: &Row) -> rusqlite::Result<Calendar> {
        Ok(Calendar {
            id: r.get(0)?,
            account_id: r.get(1)?,
            remote_id: r.get(2)?,
            name: r.get(3)?,
            color: r.get(4)?,
            visible: r.get(5)?,
            read_only: r.get(6)?,
            kind: if r.get::<_, String>(7)? == "tasks" {
                CalendarKind::Tasks
            } else {
                CalendarKind::Events
            },
            sync_token: r.get(8)?,
            ctag: r.get(9)?,
        })
    }

    const CAL_COLS: &'static str =
        "id, account_id, remote_id, name, color, visible, read_only, kind, sync_token, ctag";

    pub fn calendars(&self) -> Result<Vec<Calendar>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {} FROM calendars ORDER BY account_id, position, rowid",
            Self::CAL_COLS
        ))?;
        let rows = st.query_map([], Self::calendar_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn calendar(&self, id: &str) -> Result<Option<Calendar>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {} FROM calendars WHERE id = ?1", Self::CAL_COLS),
                [id],
                Self::calendar_from_row,
            )
            .optional()?)
    }

    /// Inserts or updates one calendar. On update the user's visibility and
    /// the sync state are kept.
    pub fn upsert_calendar(&self, cal: &Calendar) -> Result<()> {
        let kind = match cal.kind {
            CalendarKind::Events => "events",
            CalendarKind::Tasks => "tasks",
        };
        self.conn.execute(
            "INSERT INTO calendars (id, account_id, remote_id, name, color, visible, read_only, kind, sync_token, ctag)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET name = ?4, color = ?5, read_only = ?7, kind = ?8",
            params![cal.id, cal.account_id, cal.remote_id, cal.name, cal.color, cal.visible, cal.read_only, kind, cal.sync_token, cal.ctag],
        )?;
        self.bump()
    }

    /// Makes an account's calendars match `cals`: new ones are added,
    /// existing ones renamed/recolored, missing ones removed with their data.
    pub fn sync_calendars(&self, account_id: &str, cals: &[Calendar]) -> Result<()> {
        let keep: HashSet<&str> = cals.iter().map(|c| c.id.as_str()).collect();
        for (i, cal) in cals.iter().enumerate() {
            self.upsert_calendar(cal)?;
            self.conn
                .execute("UPDATE calendars SET position = ?2 WHERE id = ?1", params![cal.id, i as i64])?;
        }
        for old in self.calendars()?.into_iter().filter(|c| c.account_id == account_id) {
            if !keep.contains(old.id.as_str()) {
                self.delete_calendar(&old.id)?;
            }
        }
        Ok(())
    }

    pub fn delete_calendar(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM events WHERE calendar_id = ?1", [id])?;
        self.conn.execute("DELETE FROM tasks WHERE calendar_id = ?1", [id])?;
        self.conn.execute("DELETE FROM calendars WHERE id = ?1", [id])?;
        self.bump()
    }

    pub fn delete_account(&self, account_id: &str) -> Result<()> {
        for cal in self.calendars()?.into_iter().filter(|c| c.account_id == account_id) {
            self.delete_calendar(&cal.id)?;
        }
        self.conn.execute("DELETE FROM account_status WHERE account_id = ?1", [account_id])?;
        self.bump()
    }

    pub fn set_calendar_visible(&self, id: &str, visible: bool) -> Result<()> {
        self.conn
            .execute("UPDATE calendars SET visible = ?2 WHERE id = ?1", params![id, visible])?;
        self.bump()
    }

    pub fn set_calendar_color(&self, id: &str, color: &str) -> Result<()> {
        self.conn
            .execute("UPDATE calendars SET color = ?2 WHERE id = ?1", params![id, color])?;
        self.bump()
    }

    pub fn set_sync_state(&self, id: &str, token: Option<&str>, ctag: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE calendars SET sync_token = ?2, ctag = ?3 WHERE id = ?1",
            params![id, token, ctag],
        )?;
        Ok(())
    }

    // ---- account status ----

    pub fn account_status(&self, account_id: &str) -> AccountStatus {
        self.conn
            .query_row(
                "SELECT last_sync, error FROM account_status WHERE account_id = ?1",
                [account_id],
                |r| Ok(AccountStatus { last_sync: r.get(0)?, error: r.get(1)? }),
            )
            .optional()
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    pub fn set_account_status(&self, account_id: &str, error: Option<&str>) -> Result<()> {
        let last_sync = if error.is_none() { Some(now()) } else { self.account_status(account_id).last_sync };
        self.conn.execute(
            "INSERT INTO account_status (account_id, last_sync, error) VALUES (?1, ?2, ?3)
             ON CONFLICT(account_id) DO UPDATE SET last_sync = ?2, error = ?3",
            params![account_id, last_sync, error],
        )?;
        self.bump()
    }

    // ---- events ----

    const EV_COLS: &'static str = "id, calendar_id, uid, title, location, description, start, end, all_day, tz, rrule, exdates, recurrence_id, reminders, etag, href, dirty";

    fn event_from_row(r: &Row) -> rusqlite::Result<Event> {
        let exdates: String = r.get(11)?;
        let reminders: String = r.get(13)?;
        Ok(Event {
            id: r.get(0)?,
            calendar_id: r.get(1)?,
            uid: r.get(2)?,
            title: r.get(3)?,
            location: r.get(4)?,
            description: r.get(5)?,
            start: r.get(6)?,
            end: r.get(7)?,
            all_day: r.get(8)?,
            tz: r.get(9)?,
            rrule: r.get(10)?,
            exdates: serde_json::from_str(&exdates).unwrap_or_default(),
            recurrence_id: r.get(12)?,
            reminders: serde_json::from_str(&reminders).unwrap_or_default(),
            etag: r.get(14)?,
            href: r.get(15)?,
            dirty: Dirty::from_i64(r.get(16)?),
        })
    }

    fn query_events(&self, filter: &str, args: impl rusqlite::Params) -> Result<Vec<Event>> {
        let mut st = self
            .conn
            .prepare(&format!("SELECT {} FROM events {filter}", Self::EV_COLS))?;
        let rows = st.query_map(args, Self::event_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn event(&self, id: &str) -> Result<Option<Event>> {
        Ok(self.query_events("WHERE id = ?1", [id])?.into_iter().next())
    }

    pub fn events_in_calendar(&self, calendar_id: &str) -> Result<Vec<Event>> {
        self.query_events("WHERE calendar_id = ?1 ORDER BY start", [calendar_id])
    }

    /// The master and the edited instances of a series.
    pub fn events_by_uid(&self, calendar_id: &str, uid: &str) -> Result<Vec<Event>> {
        self.query_events("WHERE calendar_id = ?1 AND uid = ?2", params![calendar_id, uid])
    }

    pub fn events_with_href(&self, calendar_id: &str, href: &str) -> Result<Vec<Event>> {
        self.query_events("WHERE calendar_id = ?1 AND href = ?2", params![calendar_id, href])
    }

    /// href → etag of a calendar's stored resources (events and tasks).
    pub fn hrefs(&self, calendar_id: &str) -> Result<HashMap<String, Option<String>>> {
        let mut out = HashMap::new();
        for table in ["events", "tasks"] {
            let mut st = self
                .conn
                .prepare(&format!("SELECT href, etag FROM {table} WHERE calendar_id = ?1 AND href IS NOT NULL"))?;
            let rows = st.query_map([calendar_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)))?;
            for row in rows {
                let (h, e) = row?;
                out.insert(h, e);
            }
        }
        Ok(out)
    }

    /// Whether any local edit is waiting to be sent.
    pub fn has_pending(&self) -> bool {
        let q = |t: &str| {
            self.conn
                .query_row(&format!("SELECT EXISTS(SELECT 1 FROM {t} WHERE dirty != 0)"), [], |r| r.get::<_, bool>(0))
                .unwrap_or(false)
        };
        q("events") || q("tasks")
    }

    pub fn dirty_events(&self, calendar_id: &str) -> Result<Vec<Event>> {
        self.query_events("WHERE calendar_id = ?1 AND dirty != 0", [calendar_id])
    }

    /// Writes an event row as is.
    pub fn put_event(&self, ev: &Event) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT OR REPLACE INTO events ({}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
                Self::EV_COLS
            ),
            params![
                ev.id,
                ev.calendar_id,
                ev.uid,
                ev.title,
                ev.location,
                ev.description,
                ev.start,
                ev.end,
                ev.all_day,
                ev.tz,
                ev.rrule,
                serde_json::to_string(&ev.exdates)?,
                ev.recurrence_id,
                serde_json::to_string(&ev.reminders)?,
                ev.etag,
                ev.href,
                ev.dirty.as_i64(),
            ],
        )?;
        self.bump()
    }

    /// Stores an event as the provider has it. It replaces the cached copy
    /// with the same uid and recurrence id (keeping its local id), unless
    /// that copy has local edits waiting, which win until they're sent.
    pub fn apply_remote_event(&self, mut ev: Event) -> Result<()> {
        let existing = self
            .events_by_uid(&ev.calendar_id, &ev.uid)?
            .into_iter()
            .find(|e| e.recurrence_id == ev.recurrence_id);
        if let Some(old) = existing {
            if old.dirty != Dirty::Clean {
                return Ok(());
            }
            ev.id = old.id;
        } else if ev.id.is_empty() {
            ev.id = crate::model::new_id();
        }
        ev.dirty = Dirty::Clean;
        self.put_event(&ev)
    }

    /// Removes a series (or a single instance with `recurrence_id`) that the
    /// provider deleted.
    pub fn apply_remote_delete(&self, calendar_id: &str, uid: &str, recurrence_id: Option<i64>) -> Result<()> {
        match recurrence_id {
            None => self.conn.execute(
                "DELETE FROM events WHERE calendar_id = ?1 AND uid = ?2",
                params![calendar_id, uid],
            )?,
            Some(rid) => self.conn.execute(
                "DELETE FROM events WHERE calendar_id = ?1 AND uid = ?2 AND recurrence_id = ?3",
                params![calendar_id, uid, rid],
            )?,
        };
        self.bump()
    }

    /// Removes a CalDAV resource by href (sync-collection reports deletions
    /// by href only).
    pub fn delete_by_href(&self, calendar_id: &str, href: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM events WHERE calendar_id = ?1 AND href = ?2", params![calendar_id, href])?;
        self.conn
            .execute("DELETE FROM tasks WHERE calendar_id = ?1 AND href = ?2", params![calendar_id, href])?;
        self.bump()
    }

    pub fn delete_event_row(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM events WHERE id = ?1", [id])?;
        self.bump()
    }

    pub fn clear_calendar_events(&self, calendar_id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM events WHERE calendar_id = ?1 AND dirty = 0",
            [calendar_id],
        )?;
        self.bump()
    }

    /// A local edit. With `track`, the change is queued for the provider.
    pub fn save_event(&self, ev: &mut Event, track: bool) -> Result<()> {
        if ev.id.is_empty() {
            ev.id = crate::model::new_id();
        }
        if ev.uid.is_empty() {
            ev.uid = format!("{}@m3-calendar", crate::model::new_id());
        }
        ev.dirty = if !track {
            Dirty::Clean
        } else {
            match self.event(&ev.id)? {
                Some(old) => old.dirty.after_edit(),
                None => Dirty::Created,
            }
        };
        self.put_event(ev)
    }

    /// A local delete. Never-sent events just disappear; others wait for
    /// the provider to confirm.
    pub fn delete_event(&self, id: &str, track: bool) -> Result<()> {
        let Some(mut ev) = self.event(id)? else { return Ok(()) };
        if !track || ev.dirty == Dirty::Created {
            // A local series takes its edited instances with it.
            if ev.recurrence_id.is_none() && !track {
                return self.apply_remote_delete(&ev.calendar_id, &ev.uid, None);
            }
            return self.delete_event_row(id);
        }
        ev.dirty = Dirty::Deleted;
        self.put_event(&ev)
    }

    /// Skips one occurrence of a recurring event.
    pub fn exclude_occurrence(&self, id: &str, start: i64, track: bool) -> Result<()> {
        let Some(mut ev) = self.event(id)? else { return Ok(()) };
        if !ev.exdates.contains(&start) {
            ev.exdates.push(start);
        }
        self.save_event(&mut ev, track)
    }

    /// Expands every visible calendar's events into occurrences that overlap
    /// `[from, to)`, sorted by start.
    pub fn occurrences(&self, from: i64, to: i64, visible_only: bool) -> Result<Vec<Occurrence>> {
        let cals: HashMap<String, Calendar> = self
            .calendars()?
            .into_iter()
            .filter(|c| c.kind == CalendarKind::Events && (!visible_only || c.visible))
            .map(|c| (c.id.clone(), c))
            .collect();
        // Recurring masters may start long before the range; everything else
        // must overlap it (overrides are matched by their recurrence id too).
        let candidates = self.query_events(
            "WHERE dirty != 3 AND (rrule IS NOT NULL OR (start < ?2 AND end > ?1) OR (start = end AND start >= ?1 AND start < ?2) OR (recurrence_id IS NOT NULL))",
            params![from, to],
        )?;
        let mut out = Vec::new();
        let mut overrides: HashMap<(String, String), Vec<i64>> = HashMap::new();
        for ev in candidates.iter().filter(|e| e.recurrence_id.is_some()) {
            overrides
                .entry((ev.calendar_id.clone(), ev.uid.clone()))
                .or_default()
                .push(ev.recurrence_id.unwrap_or_default());
        }
        for ev in &candidates {
            let Some(cal) = cals.get(&ev.calendar_id) else { continue };
            let skip = overrides
                .get(&(ev.calendar_id.clone(), ev.uid.clone()))
                .cloned()
                .unwrap_or_default();
            let spans = if ev.recurrence_id.is_some() {
                if ev.start < to && (ev.end > from || (ev.start == ev.end && ev.start >= from)) {
                    vec![(ev.start, ev.end)]
                } else {
                    vec![]
                }
            } else if ev.rrule.is_some() {
                recur::expand(ev, from, to)
                    .into_iter()
                    .filter(|(s, _)| !skip.contains(s))
                    .collect()
            } else {
                vec![(ev.start, ev.end)]
            };
            for (start, end) in spans {
                out.push(Occurrence {
                    key: format!("{}@{}", ev.id, start),
                    event_id: ev.id.clone(),
                    calendar_id: ev.calendar_id.clone(),
                    calendar: cal.name.clone(),
                    color: cal.color.clone(),
                    title: ev.title.clone(),
                    location: ev.location.clone(),
                    description: ev.description.clone(),
                    start,
                    end,
                    all_day: ev.all_day,
                    recurring: ev.rrule.is_some() || ev.recurrence_id.is_some(),
                    reminders: ev.reminders.clone(),
                });
            }
        }
        out.sort_by(|a, b| a.start.cmp(&b.start).then(b.all_day.cmp(&a.all_day)).then(a.title.cmp(&b.title)));
        Ok(out)
    }

    /// Case-insensitive search in titles, locations and descriptions.
    pub fn search(&self, query: &str, from: i64, to: i64) -> Result<Vec<Occurrence>> {
        let q = query.to_lowercase();
        Ok(self
            .occurrences(from, to, false)?
            .into_iter()
            .filter(|o| {
                o.title.to_lowercase().contains(&q)
                    || o.location.to_lowercase().contains(&q)
                    || o.description.to_lowercase().contains(&q)
            })
            .collect())
    }

    // ---- tasks ----

    const TASK_COLS: &'static str = "id, calendar_id, uid, title, notes, due, completed, etag, href, dirty";

    fn task_from_row(r: &Row) -> rusqlite::Result<Task> {
        Ok(Task {
            id: r.get(0)?,
            calendar_id: r.get(1)?,
            uid: r.get(2)?,
            title: r.get(3)?,
            notes: r.get(4)?,
            due: r.get(5)?,
            completed: r.get(6)?,
            etag: r.get(7)?,
            href: r.get(8)?,
            dirty: Dirty::from_i64(r.get(9)?),
        })
    }

    fn query_tasks(&self, filter: &str, args: impl rusqlite::Params) -> Result<Vec<Task>> {
        let mut st = self
            .conn
            .prepare(&format!("SELECT {} FROM tasks {filter}", Self::TASK_COLS))?;
        let rows = st.query_map(args, Self::task_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Tasks of visible lists: open ones first by due date, then completed.
    pub fn tasks(&self, include_completed: bool) -> Result<Vec<Task>> {
        let visible: HashSet<String> = self
            .calendars()?
            .into_iter()
            .filter(|c| c.visible)
            .map(|c| c.id)
            .collect();
        let mut tasks: Vec<Task> = self
            .query_tasks("WHERE dirty != 3", [])?
            .into_iter()
            .filter(|t| visible.contains(&t.calendar_id) && (include_completed || !t.completed))
            .collect();
        tasks.sort_by(|a, b| {
            a.completed
                .cmp(&b.completed)
                .then(a.due.unwrap_or(i64::MAX).cmp(&b.due.unwrap_or(i64::MAX)))
                .then(a.title.cmp(&b.title))
        });
        Ok(tasks)
    }

    pub fn task(&self, id: &str) -> Result<Option<Task>> {
        Ok(self.query_tasks("WHERE id = ?1", [id])?.into_iter().next())
    }

    pub fn tasks_in_calendar(&self, calendar_id: &str) -> Result<Vec<Task>> {
        self.query_tasks("WHERE calendar_id = ?1", [calendar_id])
    }

    pub fn tasks_with_href(&self, calendar_id: &str, href: &str) -> Result<Vec<Task>> {
        self.query_tasks("WHERE calendar_id = ?1 AND href = ?2", params![calendar_id, href])
    }

    pub fn dirty_tasks(&self, calendar_id: &str) -> Result<Vec<Task>> {
        self.query_tasks("WHERE calendar_id = ?1 AND dirty != 0", [calendar_id])
    }

    pub fn put_task(&self, t: &Task) -> Result<()> {
        self.conn.execute(
            &format!(
                "INSERT OR REPLACE INTO tasks ({}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                Self::TASK_COLS
            ),
            params![t.id, t.calendar_id, t.uid, t.title, t.notes, t.due, t.completed, t.etag, t.href, t.dirty.as_i64()],
        )?;
        self.bump()
    }

    /// Like [`Store::apply_remote_event`], for tasks.
    pub fn apply_remote_task(&self, mut t: Task) -> Result<()> {
        if let Some(old) = self
            .query_tasks("WHERE calendar_id = ?1 AND uid = ?2", params![t.calendar_id, t.uid])?
            .into_iter()
            .next()
        {
            if old.dirty != Dirty::Clean {
                return Ok(());
            }
            t.id = old.id;
        } else if t.id.is_empty() {
            t.id = crate::model::new_id();
        }
        t.dirty = Dirty::Clean;
        self.put_task(&t)
    }

    /// Replaces a list's clean tasks with the provider's full list.
    pub fn replace_remote_tasks(&self, calendar_id: &str, tasks: Vec<Task>) -> Result<()> {
        let keep: HashSet<String> = tasks.iter().map(|t| t.uid.clone()).collect();
        for old in self.tasks_in_calendar(calendar_id)? {
            if old.dirty == Dirty::Clean && !keep.contains(&old.uid) {
                self.conn.execute("DELETE FROM tasks WHERE id = ?1", [&old.id])?;
            }
        }
        for t in tasks {
            self.apply_remote_task(t)?;
        }
        self.bump()
    }

    pub fn delete_task_row(&self, id: &str) -> Result<()> {
        self.conn.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
        self.bump()
    }

    pub fn save_task(&self, t: &mut Task, track: bool) -> Result<()> {
        if t.id.is_empty() {
            t.id = crate::model::new_id();
        }
        if t.uid.is_empty() {
            t.uid = format!("{}@m3-calendar", crate::model::new_id());
        }
        t.dirty = if !track {
            Dirty::Clean
        } else {
            match self.task(&t.id)? {
                Some(old) => old.dirty.after_edit(),
                None => Dirty::Created,
            }
        };
        self.put_task(t)
    }

    pub fn set_task_completed(&self, id: &str, completed: bool, track: bool) -> Result<bool> {
        let Some(mut t) = self.task(id)? else { return Ok(false) };
        t.completed = completed;
        self.save_task(&mut t, track)?;
        Ok(true)
    }

    // ---- reminders ----

    pub fn was_fired(&self, key: &str) -> bool {
        self.conn
            .query_row("SELECT 1 FROM fired WHERE key = ?1", [key], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    pub fn mark_fired(&self, key: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO fired (key, at) VALUES (?1, ?2)",
            params![key, now()],
        )?;
        // Forget reminders older than a month.
        self.conn
            .execute("DELETE FROM fired WHERE at < ?1", [now() - 31 * 86400])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cal(store: &Store, id: &str) -> Calendar {
        let c = Calendar {
            id: id.into(),
            account_id: "local".into(),
            remote_id: id.into(),
            name: "Personal".into(),
            color: "#4285F4".into(),
            visible: true,
            ..Default::default()
        };
        store.upsert_calendar(&c).unwrap();
        c
    }

    fn event(cal: &str, title: &str, start: i64, end: i64) -> Event {
        Event { calendar_id: cal.into(), title: title.into(), start, end, ..Default::default() }
    }

    #[test]
    fn events_round_trip() {
        let store = Store::in_memory().unwrap();
        cal(&store, "local/personal");
        let rev = store.revision();
        let mut ev = event("local/personal", "Dentist", 1_000_000, 1_003_600);
        ev.reminders = vec![10, 60];
        ev.exdates = vec![5];
        store.save_event(&mut ev, true).unwrap();
        assert!(store.revision() > rev);
        let back = store.event(&ev.id).unwrap().unwrap();
        assert_eq!(back, ev);
        assert_eq!(back.dirty, Dirty::Created);
        let occ = store.occurrences(999_000, 1_100_000, true).unwrap();
        assert_eq!(occ.len(), 1);
        assert_eq!(occ[0].title, "Dentist");
        assert_eq!(occ[0].calendar, "Personal");
        // Outside the range.
        assert!(store.occurrences(0, 1000, true).unwrap().is_empty());
        // Hidden calendars drop out.
        store.set_calendar_visible("local/personal", false).unwrap();
        assert!(store.occurrences(999_000, 1_100_000, true).unwrap().is_empty());
        assert_eq!(store.occurrences(999_000, 1_100_000, false).unwrap().len(), 1);
    }

    #[test]
    fn local_edits_win_until_sent() {
        let store = Store::in_memory().unwrap();
        cal(&store, "a/c");
        let mut remote = event("a/c", "From server", 100, 200);
        remote.uid = "u1".into();
        store.apply_remote_event(remote.clone()).unwrap();
        let id = store.events_by_uid("a/c", "u1").unwrap()[0].id.clone();
        let mut mine = store.event(&id).unwrap().unwrap();
        mine.title = "Edited here".into();
        store.save_event(&mut mine, true).unwrap();
        assert_eq!(mine.dirty, Dirty::Updated);
        remote.title = "Changed on server".into();
        store.apply_remote_event(remote.clone()).unwrap();
        assert_eq!(store.event(&id).unwrap().unwrap().title, "Edited here");
        // Once sent (clean), the server copy applies again and keeps the id.
        mine.dirty = Dirty::Clean;
        store.put_event(&mine).unwrap();
        store.apply_remote_event(remote).unwrap();
        let now = store.event(&id).unwrap().unwrap();
        assert_eq!(now.title, "Changed on server");
        // Deleting marks it until the provider confirms.
        store.delete_event(&id, true).unwrap();
        assert_eq!(store.event(&id).unwrap().unwrap().dirty, Dirty::Deleted);
        assert!(store.occurrences(0, 1000, true).unwrap().is_empty());
        store.apply_remote_delete("a/c", "u1", None).unwrap();
        assert!(store.event(&id).unwrap().is_none());
    }

    #[test]
    fn calendars_sync_keeps_visibility() {
        let store = Store::in_memory().unwrap();
        let a = Calendar { id: "acc/a".into(), account_id: "acc".into(), name: "A".into(), visible: true, ..Default::default() };
        store.upsert_calendar(&a).unwrap();
        store.set_calendar_visible("acc/a", false).unwrap();
        store.set_sync_state("acc/a", Some("tok"), None).unwrap();
        let mut renamed = a.clone();
        renamed.name = "Work".into();
        renamed.visible = true;
        renamed.sync_token = None;
        let b = Calendar { id: "acc/b".into(), account_id: "acc".into(), name: "B".into(), visible: true, ..Default::default() };
        store.sync_calendars("acc", &[renamed, b]).unwrap();
        let got = store.calendar("acc/a").unwrap().unwrap();
        assert_eq!(got.name, "Work");
        assert!(!got.visible);
        assert_eq!(got.sync_token.as_deref(), Some("tok"));
        store.sync_calendars("acc", &[Calendar { id: "acc/b".into(), account_id: "acc".into(), name: "B".into(), ..Default::default() }]).unwrap();
        assert!(store.calendar("acc/a").unwrap().is_none());
    }

    #[test]
    fn tasks_and_reminders() {
        let store = Store::in_memory().unwrap();
        cal(&store, "l/t");
        let mut t = Task { calendar_id: "l/t".into(), title: "Buy milk".into(), due: Some(86400), ..Default::default() };
        store.save_task(&mut t, false).unwrap();
        let mut t2 = Task { calendar_id: "l/t".into(), title: "Later".into(), ..Default::default() };
        store.save_task(&mut t2, false).unwrap();
        assert_eq!(store.tasks(false).unwrap()[0].title, "Buy milk");
        store.set_task_completed(&t.id, true, false).unwrap();
        assert_eq!(store.tasks(false).unwrap().len(), 1);
        assert_eq!(store.tasks(true).unwrap().len(), 2);
        assert!(!store.was_fired("k"));
        store.mark_fired("k").unwrap();
        assert!(store.was_fired("k"));
    }

    #[test]
    fn file_store_is_shared() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/cache.db");
        let a = Store::open(&path).unwrap();
        let b = Store::open(&path).unwrap();
        cal(&a, "x/y");
        assert_eq!(b.calendars().unwrap().len(), 1);
        assert_eq!(a.revision(), b.revision());
    }
}
