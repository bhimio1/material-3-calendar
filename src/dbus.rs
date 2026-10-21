//! The session-bus interface the daemon serves for the shell and the
//! command line: `org.material3.Calendar` at `/org/material3/Calendar`.
//!
//! | Method | Returns |
//! |---|---|
//! | `ListCalendars()` | JSON array of calendars |
//! | `GetEvents(from: x, to: x)` | JSON array of occurrences overlapping [from, to) (unix seconds) |
//! | `GetTasks()` | JSON array of open tasks |
//! | `CreateEvent(json: s)` | id of the new event |
//! | `UpdateEvent(json: s)` | – |
//! | `DeleteEvent(id: s)` | – |
//! | `QuickAdd(text: s)` | JSON of the new event |
//! | `CompleteTask(id: s)` | – |
//! | `SyncNow()` | – (sync runs in the background) |
//!
//! Signal `Changed()` fires whenever the cache changes.

use crate::config::Settings;
use crate::model::Event;
use crate::store::Store;
use std::sync::Mutex;
use zbus::fdo;

pub const NAME: &str = "org.material3.Calendar";
pub const PATH: &str = "/org/material3/Calendar";
pub const IFACE: &str = "org.material3.Calendar";

pub struct Service {
    pub store: Mutex<Store>,
    pub sync: flume::Sender<()>,
}

fn err(e: anyhow::Error) -> fdo::Error {
    fdo::Error::Failed(format!("{e:#}"))
}

#[zbus::interface(name = "org.material3.Calendar")]
impl Service {
    fn list_calendars(&self) -> fdo::Result<String> {
        crate::ops::calendars_json(&self.store.lock().unwrap()).map_err(err)
    }

    fn get_events(&self, from: i64, to: i64) -> fdo::Result<String> {
        let occ = self.store.lock().unwrap().occurrences(from, to, true).map_err(err)?;
        Ok(crate::ops::occurrences_json(&occ))
    }

    fn get_tasks(&self) -> fdo::Result<String> {
        crate::ops::tasks_json(&self.store.lock().unwrap()).map_err(err)
    }

    fn create_event(&self, json: &str) -> fdo::Result<String> {
        let mut ev: Event = serde_json::from_str(json).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        ev.id = String::new();
        ev.uid = String::new();
        let id = crate::ops::save_event(&self.store.lock().unwrap(), &Settings::load(), ev).map_err(err)?;
        let _ = self.sync.send(());
        Ok(id)
    }

    fn update_event(&self, json: &str) -> fdo::Result<()> {
        let patch: serde_json::Value = serde_json::from_str(json).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        let id = patch["id"].as_str().ok_or_else(|| fdo::Error::InvalidArgs("missing id".into()))?;
        let store = self.store.lock().unwrap();
        let current = store.event(id).map_err(err)?.ok_or_else(|| fdo::Error::InvalidArgs("no such event".into()))?;
        // Only the fields given change.
        let mut merged = serde_json::to_value(&current).map_err(|e| fdo::Error::Failed(e.to_string()))?;
        if let (Some(m), Some(p)) = (merged.as_object_mut(), patch.as_object()) {
            for (k, v) in p {
                m.insert(k.clone(), v.clone());
            }
        }
        let ev: Event = serde_json::from_value(merged).map_err(|e| fdo::Error::InvalidArgs(e.to_string()))?;
        crate::ops::save_event(&store, &Settings::load(), ev).map_err(err)?;
        let _ = self.sync.send(());
        Ok(())
    }

    fn delete_event(&self, id: &str) -> fdo::Result<()> {
        crate::ops::delete_event(&self.store.lock().unwrap(), id).map_err(err)?;
        let _ = self.sync.send(());
        Ok(())
    }

    fn quick_add(&self, text: &str) -> fdo::Result<String> {
        let ev = crate::ops::quick_add(&self.store.lock().unwrap(), &Settings::load(), text, None).map_err(err)?;
        let _ = self.sync.send(());
        serde_json::to_string(&ev).map_err(|e| fdo::Error::Failed(e.to_string()))
    }

    fn complete_task(&self, id: &str) -> fdo::Result<()> {
        crate::ops::set_task_completed(&self.store.lock().unwrap(), id, true).map_err(err)?;
        let _ = self.sync.send(());
        Ok(())
    }

    fn sync_now(&self) {
        let _ = self.sync.send(());
    }

    #[zbus(signal)]
    async fn changed(ctxt: &zbus::SignalContext<'_>) -> zbus::Result<()>;
}

/// Starts serving on `conn` (the session bus, or a test bus).
pub fn serve(builder: zbus::blocking::connection::Builder<'static>, service: Service) -> zbus::Result<zbus::blocking::Connection> {
    builder.name(NAME)?.serve_at(PATH, service)?.build()
}

pub fn emit_changed(conn: &zbus::blocking::Connection) {
    let _ = conn.emit_signal(None::<zbus::names::BusName>, PATH, IFACE, "Changed", &());
}

/// A client for the command line and the app. `None` when no daemon runs.
pub struct Client {
    proxy: zbus::blocking::Proxy<'static>,
}

impl Client {
    pub fn connect() -> Option<Self> {
        let conn = zbus::blocking::Connection::session().ok()?;
        Self::on(&conn)
    }

    pub fn on(conn: &zbus::blocking::Connection) -> Option<Self> {
        let dbus = zbus::blocking::fdo::DBusProxy::new(conn).ok()?;
        let name = zbus::names::BusName::try_from(NAME).ok()?;
        if !dbus.name_has_owner(name).ok()? {
            return None;
        }
        let proxy = zbus::blocking::Proxy::new(conn, NAME, PATH, IFACE).ok()?;
        Some(Self { proxy })
    }

    pub fn call<B, R>(&self, method: &str, body: &B) -> anyhow::Result<R>
    where
        B: serde::Serialize + zbus::zvariant::DynamicType,
        R: for<'d> zbus::zvariant::DynamicDeserialize<'d>,
    {
        Ok(self.proxy.call(method, body)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Needs a session bus: run under `dbus-run-session -- cargo test`.
    #[test]
    fn round_trip_over_the_bus() {
        if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
            eprintln!("skipping: no session bus (use dbus-run-session)");
            return;
        }
        let home = tempfile::tempdir().unwrap();
        // Keep config/accounts reads away from the real home.
        std::env::set_var("XDG_CONFIG_HOME", home.path().join("config"));
        let store = Store::in_memory().unwrap();
        crate::sync::ensure_local_calendar(&store).unwrap();
        let (tx, rx) = flume::unbounded();
        let conn = serve(zbus::blocking::connection::Builder::session().unwrap(), Service { store: Mutex::new(store), sync: tx }).unwrap();
        let client_conn = zbus::blocking::Connection::session().unwrap();
        let client = Client::on(&client_conn).expect("service visible");
        let start = 1_900_000_000i64;
        let id: String = client
            .call("CreateEvent", &(serde_json::json!({"title": "Bus test", "start": start, "end": start + 3600, "location": "Here"}).to_string(),))
            .unwrap();
        assert!(!id.is_empty());
        assert!(rx.try_recv().is_ok());
        let json: String = client.call("GetEvents", &(start - 10, start + 10)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v[0]["title"], "Bus test");
        assert_eq!(v[0]["location"], "Here");
        assert!(v[0]["start_iso"].as_str().unwrap().contains('T'));
        let () = client.call("UpdateEvent", &(serde_json::json!({"id": id, "title": "Renamed"}).to_string(),)).unwrap();
        let json: String = client.call("GetEvents", &(start - 10, start + 10)).unwrap();
        assert!(json.contains("Renamed") && json.contains("Here"));
        let cals: String = client.call("ListCalendars", &()).unwrap();
        assert!(cals.contains("Personal"));
        let () = client.call("DeleteEvent", &(id.clone(),)).unwrap();
        let json: String = client.call("GetEvents", &(start - 10, start + 10)).unwrap();
        assert_eq!(json, "[]");
        let () = client.call("SyncNow", &()).unwrap();
        emit_changed(&conn);
    }
}
