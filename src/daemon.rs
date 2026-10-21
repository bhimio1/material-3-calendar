//! `m3-calendar daemon`: keeps the cache in sync on a timer and on demand,
//! serves the D-Bus interface, and shows reminders as desktop
//! notifications with Snooze and Open actions.

use crate::config::Settings;
use crate::dbus::{self, Service};
use crate::model::Occurrence;
use crate::secrets::{Keyring, Secrets};
use crate::store::Store;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use zbus::zvariant::Value;

/// How late a missed reminder may still be shown (e.g. after resume).
const GRACE: i64 = 15 * 60;
const SNOOZE: i64 = 10 * 60;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn run() -> Result<()> {
    let store_path = crate::store::default_path();
    let store = Store::open(&store_path)?;
    crate::sync::ensure_local_calendar(&store)?;
    let (sync_tx, sync_rx) = flume::unbounded::<()>();
    let conn = dbus::serve(
        zbus::blocking::connection::Builder::session()?,
        Service { store: Mutex::new(store), sync: sync_tx.clone() },
    )
    .context("claiming org.material3.Calendar on the session bus (is another daemon running?)")?;
    log::info!("serving {} on the session bus", dbus::NAME);

    // Sync loop.
    {
        let conn = conn.clone();
        let path = store_path.clone();
        std::thread::spawn(move || {
            let store = match Store::open(&path) {
                Ok(s) => s,
                Err(e) => return log::error!("sync: {e:#}"),
            };
            let secrets: Arc<dyn Secrets> = Arc::new(Keyring);
            let mut first = true;
            loop {
                let minutes = Settings::load().sync_minutes.max(1);
                if !first {
                    let _ = sync_rx.recv_timeout(Duration::from_secs(minutes * 60));
                    // Coalesce bursts of edits.
                    std::thread::sleep(Duration::from_millis(500));
                    while sync_rx.try_recv().is_ok() {}
                }
                first = false;
                for e in crate::sync::sync_all(&store, &Settings::load(), secrets.clone()) {
                    log::warn!("{e}");
                }
                dbus::emit_changed(&conn);
            }
        });
    }

    let notifier = Notifier::new(&conn);
    let store = Store::open(&store_path)?;
    let mut last_rev = store.revision();
    let mut last_check = Instant::now() - Duration::from_secs(60);
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let rev = store.revision();
        if rev != last_rev {
            last_rev = rev;
            dbus::emit_changed(&conn);
            // Edits made by the app or the CLI go out promptly.
            if store.has_pending() {
                let _ = sync_tx.send(());
            }
        }
        if last_check.elapsed() >= Duration::from_secs(20) {
            last_check = Instant::now();
            if let Some(n) = &notifier {
                if let Err(e) = n.check(&store) {
                    log::warn!("reminders: {e:#}");
                }
            }
        }
    }
}

/// Reminders due at `now`: (fired key, occurrence), not yet shown.
pub fn due_reminders(store: &Store, now: i64) -> Result<Vec<(String, Occurrence)>> {
    let mut out = Vec::new();
    // Reminders reach at most a week ahead.
    for occ in store.occurrences(now - GRACE, now + 8 * 86400, true)? {
        for m in &occ.reminders {
            let at = occ.start - m * 60;
            let key = format!("{}:{m}", occ.key);
            if at <= now && now - at <= GRACE && !store.was_fired(&key) {
                out.push((key, occ.clone()));
                break;
            }
        }
    }
    Ok(out)
}

pub fn describe(occ: &Occurrence) -> String {
    use chrono::{Local, TimeZone};
    let fmt = |t: i64| Local.timestamp_opt(t, 0).single().map(|d| d.format("%H:%M").to_string()).unwrap_or_default();
    let mut body = if occ.all_day { "All day".to_string() } else { format!("{} – {}", fmt(occ.start), fmt(occ.end)) };
    if !occ.location.is_empty() {
        body.push_str(&format!(" · {}", occ.location));
    }
    body
}

struct Notifier {
    proxy: zbus::blocking::Proxy<'static>,
    shown: Arc<Mutex<HashMap<u32, Occurrence>>>,
    snoozed: Arc<Mutex<Vec<(i64, Occurrence)>>>,
}

impl Notifier {
    fn new(conn: &zbus::blocking::Connection) -> Option<Self> {
        let proxy = zbus::blocking::Proxy::new(
            conn,
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
        )
        .ok()?;
        let shown: Arc<Mutex<HashMap<u32, Occurrence>>> = Arc::default();
        let snoozed: Arc<Mutex<Vec<(i64, Occurrence)>>> = Arc::default();
        // Act on the buttons.
        {
            let proxy = proxy.clone();
            let shown = shown.clone();
            let snoozed = snoozed.clone();
            std::thread::spawn(move || {
                let Ok(signals) = proxy.receive_signal("ActionInvoked") else { return };
                for msg in signals {
                    let Ok((id, action)) = msg.body().deserialize::<(u32, String)>() else { continue };
                    let Some(occ) = shown.lock().unwrap().remove(&id) else { continue };
                    match action.as_str() {
                        "snooze" => snoozed.lock().unwrap().push((now() + SNOOZE, occ)),
                        _ => open_in_app(&occ),
                    }
                }
            });
        }
        Some(Self { proxy, shown, snoozed })
    }

    fn notify(&self, occ: &Occurrence) -> Result<()> {
        let actions = vec!["default", "Open", "open", "Open", "snooze", "Snooze 10 min"];
        let mut hints: HashMap<&str, Value> = HashMap::new();
        hints.insert("desktop-entry", Value::from("material_3_calendar"));
        hints.insert("urgency", Value::U8(1));
        let id: u32 = self.proxy.call(
            "Notify",
            &("Calendar", 0u32, "x-office-calendar", occ.title.as_str(), describe(occ), actions, hints, 0i32),
        )?;
        self.shown.lock().unwrap().insert(id, occ.clone());
        Ok(())
    }

    fn check(&self, store: &Store) -> Result<()> {
        let t = now();
        for (key, occ) in due_reminders(store, t)? {
            self.notify(&occ)?;
            store.mark_fired(&key)?;
        }
        let due: Vec<Occurrence> = {
            let mut s = self.snoozed.lock().unwrap();
            let (ready, wait): (Vec<_>, Vec<_>) = s.drain(..).partition(|(at, _)| *at <= t);
            *s = wait;
            ready.into_iter().map(|(_, o)| o).collect()
        };
        for occ in due {
            self.notify(&occ)?;
        }
        Ok(())
    }
}

fn open_in_app(occ: &Occurrence) {
    let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| "m3-calendar".into());
    let at = occ.start.to_string();
    let _ = material_3_ui::sys::cmd::spawn_detached(&exe, &["--open", &occ.event_id, "--at", &at]);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Calendar, Event};

    #[test]
    fn reminders_fire_once_within_grace() {
        let store = Store::in_memory().unwrap();
        store
            .upsert_calendar(&Calendar { id: "l/c".into(), account_id: "local".into(), name: "C".into(), visible: true, ..Default::default() })
            .unwrap();
        let start = 2_000_000_000;
        let mut ev = Event { calendar_id: "l/c".into(), title: "Call".into(), start, end: start + 600, reminders: vec![10, 60], ..Default::default() };
        store.save_event(&mut ev, false).unwrap();
        assert!(due_reminders(&store, start - 3601).unwrap().is_empty());
        let due = due_reminders(&store, start - 3600).unwrap();
        assert_eq!(due.len(), 1);
        store.mark_fired(&due[0].0).unwrap();
        assert!(due_reminders(&store, start - 3500).unwrap().is_empty());
        // The 10-minute one is separate.
        assert_eq!(due_reminders(&store, start - 600).unwrap().len(), 1);
        // Too late is skipped.
        assert!(due_reminders(&store, start - 600 + GRACE + 1).unwrap().is_empty());
        assert!(describe(&due[0].1).contains('–'));
    }
}
