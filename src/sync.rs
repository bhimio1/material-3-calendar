//! Syncing accounts: send queued local edits, then pull changes. Also the
//! account-level operations (add, remove, import/export .ics) that the app,
//! the daemon and the command line share.

use crate::config::{Account, Accounts, Settings};
use crate::model::{AccountKind, Calendar, CalendarKind, Dirty};
use crate::providers::{self, Endpoints, Provider};
use crate::secrets::Secrets;
use crate::store::Store;
use anyhow::{anyhow, Context, Result};
use std::path::Path;
use std::sync::Arc;

/// Syncs one account and records the outcome for the Accounts page.
pub fn sync_account(store: &Store, account: &Account, settings: &Settings, secrets: Arc<dyn Secrets>, ep: &Endpoints) -> Result<()> {
    let result = (|| -> Result<()> {
        let Some(mut provider) = providers::for_account(account, settings, secrets, ep)? else { return Ok(()) };
        sync_with(store, &account.id, provider.as_mut())
    })();
    let err = result.as_ref().err().map(|e| format!("{e:#}"));
    store.set_account_status(&account.id, err.as_deref())?;
    result
}

/// The provider-independent part: calendars, pushes, pulls.
pub fn sync_with(store: &Store, account_id: &str, provider: &mut dyn Provider) -> Result<()> {
    let cals = provider.calendars()?;
    store.sync_calendars(account_id, &cals)?;
    let mut first_error = None;
    for cal in store.calendars()?.into_iter().filter(|c| c.account_id == account_id) {
        if let Err(e) = sync_calendar(store, &cal, provider) {
            log::warn!("syncing {}: {e:#}", cal.name);
            first_error.get_or_insert(e.context(format!("syncing {}", cal.name)));
        }
    }
    first_error.map_or(Ok(()), Err)
}

fn sync_calendar(store: &Store, cal: &Calendar, provider: &mut dyn Provider) -> Result<()> {
    match cal.kind {
        CalendarKind::Events => {
            for ev in store.dirty_events(&cal.id)? {
                if cal.read_only {
                    // Can't send it anywhere; drop the local change.
                    store.delete_event_row(&ev.id)?;
                    continue;
                }
                if ev.dirty == Dirty::Deleted {
                    provider.delete_event(cal, &ev)?;
                    store.apply_remote_delete(&cal.id, &ev.uid, ev.recurrence_id)?;
                } else if store.event(&ev.id)?.is_some_and(|e| e.dirty != Dirty::Clean) {
                    // (CalDAV pushes a whole series at once, so check again.)
                    provider.push_event(cal, &ev, store)?;
                }
            }
            let fresh = store.calendar(&cal.id)?.unwrap_or_else(|| cal.clone());
            provider.sync_events(&fresh, store)
        }
        CalendarKind::Tasks => {
            for t in store.dirty_tasks(&cal.id)? {
                if cal.read_only {
                    store.delete_task_row(&t.id)?;
                } else if t.dirty == Dirty::Deleted {
                    provider.delete_task(cal, &t)?;
                    store.delete_task_row(&t.id)?;
                } else {
                    provider.push_task(cal, &t, store)?;
                }
            }
            let fresh = store.calendar(&cal.id)?.unwrap_or_else(|| cal.clone());
            provider.sync_tasks(&fresh, store)
        }
    }
}

/// Syncs every remote account; returns one message per failing account.
pub fn sync_all(store: &Store, settings: &Settings, secrets: Arc<dyn Secrets>) -> Vec<String> {
    let accounts = Accounts::load();
    let ep = Endpoints::default();
    ensure_local_calendar(store).ok();
    accounts
        .list
        .iter()
        .filter(|a| !a.is_local())
        .filter_map(|a| sync_account(store, a, settings, secrets.clone(), &ep).err().map(|e| format!("{}: {e:#}", a.name)))
        .collect()
}

/// Every install has at least one local calendar to put events in.
pub fn ensure_local_calendar(store: &Store) -> Result<()> {
    let has_local = store.calendars()?.iter().any(|c| c.account_id == "local" && c.kind == CalendarKind::Events);
    if !has_local {
        add_local_calendar(store, "Personal", crate::model::CALENDAR_COLORS[0])?;
    }
    Ok(())
}

pub fn add_local_calendar(store: &Store, name: &str, color: &str) -> Result<Calendar> {
    let remote = crate::model::new_id();
    let cal = Calendar {
        id: Calendar::make_id("local", &remote),
        account_id: "local".into(),
        remote_id: remote,
        name: name.into(),
        color: color.into(),
        visible: true,
        ..Default::default()
    };
    store.upsert_calendar(&cal)?;
    Ok(cal)
}

pub fn add_local_task_list(store: &Store, name: &str) -> Result<Calendar> {
    let mut cal = add_local_calendar(store, name, "#1E88E5")?;
    cal.kind = CalendarKind::Tasks;
    store.upsert_calendar(&cal)?;
    Ok(cal)
}

/// Imports an .ics file into a new local calendar. Returns it and how many
/// events came in.
pub fn import_ics(store: &Store, path: &Path) -> Result<(Calendar, usize)> {
    ensure_local_calendar(store)?;
    let text = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = crate::ical::parse(&text, &crate::recur::local_zone_name());
    if parsed.events.is_empty() && parsed.tasks.is_empty() {
        return Err(anyhow!("{} has no events", path.display()));
    }
    let name = parsed
        .name
        .clone()
        .unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Imported".into()));
    let color = parsed.color.clone().unwrap_or_else(|| crate::model::CALENDAR_COLORS[fastrand::usize(..crate::model::CALENDAR_COLORS.len())].into());
    let cal = add_local_calendar(store, &name, &color)?;
    let n = parsed.events.len();
    for mut ev in parsed.events {
        ev.calendar_id = cal.id.clone();
        if ev.uid.is_empty() {
            ev.uid = format!("{}@m3-calendar", crate::model::new_id());
        }
        store.save_event(&mut ev, false)?;
    }
    if !parsed.tasks.is_empty() {
        let list = add_local_task_list(store, &format!("{name} tasks"))?;
        for mut t in parsed.tasks {
            t.calendar_id = list.id.clone();
            store.save_task(&mut t, false)?;
        }
    }
    Ok((cal, n))
}

pub fn export_ics(store: &Store, calendar_id: &str, path: &Path) -> Result<usize> {
    let cal = store.calendar(calendar_id)?.ok_or_else(|| anyhow!("no such calendar"))?;
    let events: Vec<_> = store.events_in_calendar(calendar_id)?.into_iter().filter(|e| e.dirty != Dirty::Deleted).collect();
    std::fs::write(path, crate::ical::calendar_file(&cal.name, &events)).with_context(|| format!("writing {}", path.display()))?;
    Ok(events.len())
}

/// Forgets an account: its cache, its keyring entry and its metadata.
pub fn remove_account(store: &Store, secrets: &dyn Secrets, id: &str) -> Result<()> {
    let mut accounts = Accounts::load();
    if let Some(a) = accounts.get(id).cloned() {
        if a.kind == AccountKind::Local {
            return Err(anyhow!("the local account can't be removed"));
        }
        secrets.delete(&a.secret_key())?;
    }
    accounts.list.retain(|a| a.id != id);
    accounts.save()?;
    store.delete_account(id)
}

/// Saves a newly signed-in account and its secret.
pub fn add_account(secrets: &dyn Secrets, mut account: Account, secret: &str) -> Result<Account> {
    let mut accounts = Accounts::load();
    // Signing in again to the same account replaces it.
    if let Some(existing) = accounts.list.iter().find(|a| a.kind == account.kind && a.name == account.name && a.url == account.url) {
        account.id = existing.id.clone();
    }
    if account.id.is_empty() {
        account.id = format!("{}-{}", account.kind.as_str(), &crate::model::new_id()[..8]);
    }
    secrets.set(&account.secret_key(), &format!("Material 3 Calendar: {}", account.name), secret)?;
    accounts.list.retain(|a| a.id != account.id);
    accounts.list.push(account.clone());
    accounts.save()?;
    Ok(account)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Event, Task};

    /// A provider that records what it was asked to do.
    #[derive(Default)]
    struct Fake {
        pushed: Vec<String>,
        deleted: Vec<String>,
    }

    impl Provider for Fake {
        fn calendars(&mut self) -> Result<Vec<Calendar>> {
            Ok(vec![
                Calendar { id: "f/c".into(), account_id: "f".into(), remote_id: "c".into(), name: "C".into(), visible: true, ..Default::default() },
                Calendar { id: "f/ro".into(), account_id: "f".into(), remote_id: "ro".into(), name: "RO".into(), visible: true, read_only: true, ..Default::default() },
                Calendar { id: "f/t".into(), account_id: "f".into(), remote_id: "t".into(), name: "T".into(), visible: true, kind: CalendarKind::Tasks, ..Default::default() },
            ])
        }
        fn sync_events(&mut self, cal: &Calendar, store: &Store) -> Result<()> {
            if cal.id == "f/c" {
                store.apply_remote_event(Event { calendar_id: cal.id.clone(), uid: "srv".into(), title: "From server".into(), start: 10, end: 20, ..Default::default() })?;
            }
            Ok(())
        }
        fn push_event(&mut self, _cal: &Calendar, ev: &Event, store: &Store) -> Result<()> {
            self.pushed.push(ev.title.clone());
            let mut e = ev.clone();
            e.dirty = Dirty::Clean;
            store.put_event(&e)
        }
        fn delete_event(&mut self, _cal: &Calendar, ev: &Event) -> Result<()> {
            self.deleted.push(ev.title.clone());
            Ok(())
        }
        fn push_task(&mut self, _cal: &Calendar, t: &Task, store: &Store) -> Result<()> {
            self.pushed.push(t.title.clone());
            let mut t = t.clone();
            t.dirty = Dirty::Clean;
            store.put_task(&t)
        }
    }

    #[test]
    fn pushes_then_pulls() {
        let store = Store::in_memory().unwrap();
        let mut fake = Fake::default();
        sync_with(&store, "f", &mut fake).unwrap();
        let mut new = Event { calendar_id: "f/c".into(), title: "Mine".into(), start: 1, end: 2, ..Default::default() };
        store.save_event(&mut new, true).unwrap();
        let mut gone = store.events_by_uid("f/c", "srv").unwrap().remove(0);
        gone.title = "Gone".into();
        store.put_event(&gone).unwrap();
        store.delete_event(&gone.id, true).unwrap();
        let mut ro = Event { calendar_id: "f/ro".into(), title: "Nope".into(), start: 1, end: 2, ..Default::default() };
        store.save_event(&mut ro, true).unwrap();
        let mut t = Task { calendar_id: "f/t".into(), title: "Task".into(), ..Default::default() };
        store.save_task(&mut t, true).unwrap();
        sync_with(&store, "f", &mut fake).unwrap();
        assert_eq!(fake.pushed, vec!["Mine", "Task"]);
        assert_eq!(fake.deleted, vec!["Gone"]);
        assert!(store.event(&ro.id).unwrap().is_none());
        assert_eq!(store.event(&new.id).unwrap().unwrap().dirty, Dirty::Clean);
        // The server copy came back on the pull.
        assert_eq!(store.events_by_uid("f/c", "srv").unwrap().len(), 1);
    }

    #[test]
    fn import_export_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::in_memory().unwrap();
        let src = dir.path().join("team.ics");
        std::fs::write(&src, "BEGIN:VCALENDAR\nX-WR-CALNAME:Team\nBEGIN:VEVENT\nUID:1\nDTSTART:20261021T100000Z\nDTEND:20261021T110000Z\nSUMMARY:Sync\nEND:VEVENT\nBEGIN:VTODO\nUID:2\nSUMMARY:Prep\nEND:VTODO\nEND:VCALENDAR\n").unwrap();
        let (cal, n) = import_ics(&store, &src).unwrap();
        assert_eq!((cal.name.as_str(), n), ("Team", 1));
        assert_eq!(store.tasks(false).unwrap().len(), 1);
        let out = dir.path().join("out.ics");
        assert_eq!(export_ics(&store, &cal.id, &out).unwrap(), 1);
        let back = crate::ical::parse(&std::fs::read_to_string(out).unwrap(), "UTC");
        assert_eq!(back.events[0].title, "Sync");
        // Importing makes sure the default "Personal" calendar exists too.
        let names: Vec<String> = store.calendars().unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["Personal", "Team", "Team tasks"]);
    }
}
