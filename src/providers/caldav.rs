//! CalDAV (RFC 4791) for iCloud, Nextcloud, Fastmail, Radicale, …:
//! discovery from any server URL (current-user-principal, .well-known,
//! calendar-home-set), incremental sync with sync-collection (RFC 6578)
//! and an etag comparison fallback, and writes with If-Match.

use super::{agent, parse_xml, status_of, xml_escape, HttpError, Node, Provider};
use crate::model::{Calendar, CalendarKind, Dirty, Event, Task};
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use std::collections::{HashMap, HashSet};

pub struct CalDav {
    account_id: String,
    url: String,
    auth: String,
    http: ureq::Agent,
}

struct Reply {
    etag: Option<String>,
    body: String,
}

const PROPFIND_PRINCIPAL: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:current-user-principal/></d:prop></d:propfind>"#;
const PROPFIND_HOME: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-home-set/></d:prop></d:propfind>"#;
const PROPFIND_CALENDARS: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:a="http://apple.com/ns/ical/"><d:prop><d:resourcetype/><d:displayname/><a:calendar-color/><cs:getctag/><d:sync-token/><c:supported-calendar-component-set/><d:current-user-privilege-set/></d:prop></d:propfind>"#;
const PROPFIND_CTAG: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><cs:getctag/><d:sync-token/></d:prop></d:propfind>"#;

impl CalDav {
    pub fn new(account_id: &str, url: &str, user: &str, password: &str) -> Self {
        let auth = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
        );
        Self { account_id: account_id.into(), url: url.trim_end_matches('/').to_string() + "/", auth, http: agent(false) }
    }

    fn abs(&self, href: &str) -> String {
        url::Url::parse(&self.url)
            .and_then(|u| u.join(href))
            .map(String::from)
            .unwrap_or_else(|_| href.to_string())
    }

    /// One WebDAV request, following redirects by hand (ureq only follows
    /// them for GET).
    fn dav(&self, method: &str, url: &str, depth: Option<&str>, body: Option<&str>, headers: &[(&str, &str)]) -> Result<Reply> {
        let mut url = self.abs(url);
        for _ in 0..5 {
            let mut req = self.http.request(method, &url).set("Authorization", &self.auth);
            if let Some(d) = depth {
                req = req.set("Depth", d);
            }
            if body.is_some() && method != "PUT" {
                req = req.set("Content-Type", "application/xml; charset=utf-8");
            }
            for (k, v) in headers {
                req = req.set(k, v);
            }
            let res = match body {
                Some(b) => req.send_string(b),
                None => req.call(),
            };
            let r = match res {
                Ok(r) => r,
                Err(ureq::Error::Status(401, _)) => bail!("the server didn't accept the user name or password"),
                Err(ureq::Error::Status(status, r)) => {
                    return Err(HttpError { status, body: r.into_string().unwrap_or_default() }.into())
                }
                Err(e) => return Err(anyhow!("{e}")),
            };
            if (300..400).contains(&r.status()) {
                let loc = r.header("Location").ok_or_else(|| anyhow!("redirect without a location"))?;
                url = url::Url::parse(&url)?.join(loc)?.to_string();
                continue;
            }
            let etag = r.header("ETag").map(String::from);
            return Ok(Reply { etag, body: r.into_string().unwrap_or_default() });
        }
        bail!("too many redirects")
    }

    fn propfind(&self, url: &str, depth: &str, body: &str) -> Result<Node> {
        let r = self.dav("PROPFIND", url, Some(depth), Some(body), &[])?;
        parse_xml(&r.body).context("reading the server's answer")
    }

    fn principal(&self) -> Result<String> {
        let at = |url: &str| -> Option<String> {
            let tree = self.propfind(url, "0", PROPFIND_PRINCIPAL).ok()?;
            tree.find("current-user-principal")?.text_of("href")
        };
        if let Some(p) = at(&self.url) {
            return Ok(p);
        }
        let origin = url::Url::parse(&self.url)?;
        let well_known = origin.join("/.well-known/caldav")?.to_string();
        at(&well_known).ok_or_else(|| anyhow!("no CalDAV account was found at {}", self.url))
    }

    /// The URL of the collection holding the user's calendars.
    pub fn home(&self) -> Result<String> {
        let principal = self.principal()?;
        let tree = self.propfind(&principal, "0", PROPFIND_HOME)?;
        let href = tree
            .find("calendar-home-set")
            .and_then(|n| n.text_of("href"))
            .ok_or_else(|| anyhow!("the server didn't say where the calendars are"))?;
        Ok(self.abs(&href))
    }

    fn report(&self, url: &str, depth: &str, body: &str) -> Result<Node> {
        let r = self.dav("REPORT", url, Some(depth), Some(body), &[])?;
        parse_xml(&r.body).context("reading the server's answer")
    }

    /// calendar-multiget for `hrefs`: (href, etag, data).
    fn multiget(&self, cal_url: &str, hrefs: &[String]) -> Result<Vec<(String, Option<String>, String)>> {
        let mut out = Vec::new();
        for chunk in hrefs.chunks(50) {
            let list: String = chunk.iter().map(|h| format!("<d:href>{}</d:href>", xml_escape(h))).collect();
            let body = format!(
                r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-multiget xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop>{list}</c:calendar-multiget>"#
            );
            let tree = self.report(cal_url, "1", &body)?;
            let mut responses = Vec::new();
            tree.find_all("response", &mut responses);
            for r in responses {
                let (Some(href), Some(data)) = (r.text_of("href"), r.find("calendar-data")) else { continue };
                if data.text.trim().is_empty() {
                    continue;
                }
                out.push((href, r.text_of("getetag"), data.text.clone()));
            }
        }
        Ok(out)
    }

    /// Stores one resource's events or task, keeping local ids stable.
    fn apply_resource(&self, cal: &Calendar, store: &Store, href: &str, etag: Option<String>, data: &str) -> Result<()> {
        let parsed = crate::ical::parse(data, &crate::recur::local_zone_name());
        if cal.kind == CalendarKind::Tasks {
            let old = store.tasks_with_href(&cal.id, href)?;
            if old.iter().any(|t| t.dirty != Dirty::Clean) {
                return Ok(());
            }
            for mut t in parsed.tasks.into_iter().take(1) {
                t.id = old.first().map(|o| o.id.clone()).unwrap_or_default();
                t.calendar_id = cal.id.clone();
                t.href = Some(href.into());
                t.etag = etag.clone();
                store.apply_remote_task(t)?;
            }
            return Ok(());
        }
        let old = store.events_with_href(&cal.id, href)?;
        if old.iter().any(|e| e.dirty != Dirty::Clean) {
            return Ok(());
        }
        let mut kept = HashSet::new();
        for mut ev in parsed.events {
            if let Some(o) = old.iter().find(|o| o.uid == ev.uid && o.recurrence_id == ev.recurrence_id) {
                ev.id = o.id.clone();
                kept.insert(o.id.clone());
            }
            ev.calendar_id = cal.id.clone();
            ev.href = Some(href.into());
            ev.etag = etag.clone();
            store.apply_remote_event(ev)?;
        }
        for o in old.iter().filter(|o| !kept.contains(&o.id)) {
            store.delete_event_row(&o.id)?;
        }
        Ok(())
    }

    /// RFC 6578 sync. `Ok(false)` when the server can't do it.
    fn sync_collection(&self, cal: &Calendar, store: &Store) -> Result<bool> {
        let token = cal.sync_token.clone().unwrap_or_default();
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><d:sync-collection xmlns:d="DAV:"><d:sync-token>{}</d:sync-token><d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>"#,
            xml_escape(&token)
        );
        let tree = match self.report(&cal.remote_id, "0", &body) {
            Ok(t) => t,
            // Expired token (403/409 valid-sync-token) or no support.
            Err(e) if !token.is_empty() && matches!(status_of(&e), Some(403 | 409 | 400)) => {
                store.set_sync_state(&cal.id, None, cal.ctag.as_deref())?;
                let fresh = Calendar { sync_token: None, ..cal.clone() };
                return self.sync_collection(&fresh, store);
            }
            Err(e) if matches!(status_of(&e), Some(400 | 403 | 404 | 405 | 409 | 415 | 501)) => return Ok(false),
            Err(e) => return Err(e),
        };
        let Some(new_token) = tree.child("multistatus").and_then(|m| m.child("sync-token")).map(|n| n.text.trim().to_string()) else {
            return Ok(false);
        };
        if token.is_empty() {
            store.clear_calendar_events(&cal.id)?;
        }
        let mut responses = Vec::new();
        tree.find_all("response", &mut responses);
        let known = store.hrefs(&cal.id)?;
        let mut fetch = Vec::new();
        for r in responses {
            let Some(href) = r.text_of("href") else { continue };
            if href.trim_end_matches('/') == url_path(&cal.remote_id).trim_end_matches('/') {
                continue;
            }
            let gone = r.child("status").is_some_and(|s| s.text.contains(" 404"));
            if gone {
                store.delete_by_href(&cal.id, &href)?;
            } else {
                let etag = r.text_of("getetag");
                if etag.is_none() || known.get(&href) != Some(&etag) {
                    fetch.push(href);
                }
            }
        }
        for (href, etag, data) in self.multiget(&cal.remote_id, &fetch)? {
            self.apply_resource(cal, store, &href, etag, &data)?;
        }
        store.set_sync_state(&cal.id, Some(&new_token), cal.ctag.as_deref())?;
        Ok(true)
    }

    /// Fallback: compare every resource's etag with the cache.
    fn sync_by_etag(&self, cal: &Calendar, store: &Store) -> Result<()> {
        let ctag = self
            .propfind(&cal.remote_id, "0", PROPFIND_CTAG)
            .ok()
            .and_then(|t| t.text_of("getctag"));
        if ctag.is_some() && ctag == cal.ctag {
            return Ok(());
        }
        let comp = if cal.kind == CalendarKind::Tasks { "VTODO" } else { "VEVENT" };
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?><c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/></d:prop><c:filter><c:comp-filter name="VCALENDAR"><c:comp-filter name="{comp}"/></c:comp-filter></c:filter></c:calendar-query>"#
        );
        let tree = self.report(&cal.remote_id, "1", &body)?;
        let mut responses = Vec::new();
        tree.find_all("response", &mut responses);
        let remote: HashMap<String, Option<String>> = responses
            .iter()
            .filter_map(|r| Some((r.text_of("href")?, r.text_of("getetag"))))
            .filter(|(h, _)| h.ends_with(".ics") || !h.ends_with('/'))
            .collect();
        let known = store.hrefs(&cal.id)?;
        for href in known.keys().filter(|h| !remote.contains_key(*h)) {
            store.delete_by_href(&cal.id, href)?;
        }
        let fetch: Vec<String> = remote
            .iter()
            .filter(|(h, e)| e.is_none() || known.get(*h) != Some(*e))
            .map(|(h, _)| h.clone())
            .collect();
        for (href, etag, data) in self.multiget(&cal.remote_id, &fetch)? {
            self.apply_resource(cal, store, &href, etag, &data)?;
        }
        store.set_sync_state(&cal.id, None, ctag.as_deref())?;
        Ok(())
    }

    fn sync(&self, cal: &Calendar, store: &Store) -> Result<()> {
        if !self.sync_collection(cal, store)? {
            self.sync_by_etag(cal, store)?;
        }
        Ok(())
    }

    /// PUT a resource. Returns the new etag (None when the server didn't
    /// send one; the next sync fetches it).
    fn put(&self, href: &str, etag: Option<&str>, ics: &str) -> Result<Option<String>> {
        let mut headers = vec![("Content-Type", "text/calendar; charset=utf-8")];
        let quoted;
        match etag {
            Some(e) => {
                quoted = e.to_string();
                headers.push(("If-Match", quoted.as_str()));
            }
            None => headers.push(("If-None-Match", "*")),
        }
        let r = self.dav("PUT", href, None, Some(ics), &headers)?;
        Ok(r.etag)
    }

    fn new_href(cal: &Calendar, uid: &str) -> String {
        let safe: String = uid.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
        format!("{}{safe}.ics", url_path(&cal.remote_id))
    }
}

/// The path part of a URL (hrefs in answers are usually paths).
fn url_path(u: &str) -> String {
    url::Url::parse(u).map(|x| x.path().to_string()).unwrap_or_else(|_| u.to_string())
}

impl Provider for CalDav {
    fn calendars(&mut self) -> Result<Vec<Calendar>> {
        let home = self.home()?;
        let tree = self.propfind(&home, "1", PROPFIND_CALENDARS)?;
        let mut responses = Vec::new();
        tree.find_all("response", &mut responses);
        let mut out = Vec::new();
        for (i, r) in responses.into_iter().enumerate() {
            let Some(href) = r.text_of("href") else { continue };
            let is_calendar = r.find("resourcetype").is_some_and(|t| t.child("calendar").is_some());
            if !is_calendar {
                continue;
            }
            let mut comps = Vec::new();
            if let Some(set) = r.find("supported-calendar-component-set") {
                set.find_all("comp", &mut comps);
            }
            let names: Vec<&str> = comps.iter().filter_map(|c| c.attr("name")).collect();
            let kind = if !names.is_empty() && !names.contains(&"VEVENT") {
                if names.contains(&"VTODO") {
                    CalendarKind::Tasks
                } else {
                    continue;
                }
            } else {
                CalendarKind::Events
            };
            let read_only = r.find("current-user-privilege-set").is_some_and(|p| {
                ["write", "write-content", "all", "bind"].iter().all(|w| p.find(w).is_none())
            });
            let url = self.abs(&href);
            out.push(Calendar {
                id: Calendar::make_id(&self.account_id, &url),
                account_id: self.account_id.clone(),
                remote_id: url,
                name: r
                    .text_of("displayname")
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| href.trim_end_matches('/').rsplit('/').next().unwrap_or("Calendar").to_string()),
                color: super::norm_color(&r.text_of("calendar-color").unwrap_or_default(), i),
                visible: true,
                read_only,
                kind,
                ..Default::default()
            });
        }
        if out.is_empty() {
            bail!("no calendars were found on the server");
        }
        Ok(out)
    }

    fn sync_events(&mut self, cal: &Calendar, store: &Store) -> Result<()> {
        self.sync(cal, store)
    }

    fn sync_tasks(&mut self, cal: &Calendar, store: &Store) -> Result<()> {
        self.sync(cal, store)
    }

    fn push_event(&mut self, cal: &Calendar, ev: &Event, store: &Store) -> Result<()> {
        // A resource holds the series and its edited instances.
        let series = store.events_by_uid(&cal.id, &ev.uid)?;
        let mut parts: Vec<&Event> = series.iter().filter(|e| e.dirty != Dirty::Deleted).collect();
        if parts.is_empty() {
            parts.push(ev);
        }
        let href = ev.href.clone().unwrap_or_else(|| Self::new_href(cal, &ev.uid));
        let etag = if ev.dirty == Dirty::Created { None } else { ev.etag.as_deref() };
        match self.put(&href, etag, &crate::ical::event_resource(&parts)) {
            Ok(new_etag) => {
                for mut e in series {
                    e.href = Some(href.clone());
                    e.etag = new_etag.clone();
                    e.dirty = Dirty::Clean;
                    store.put_event(&e)?;
                }
                Ok(())
            }
            // Changed on the server meanwhile: the server's copy wins.
            Err(e) if status_of(&e) == Some(412) => {
                log::warn!("{} changed on the server; keeping the server's version", ev.title);
                for mut e in series {
                    e.dirty = Dirty::Clean;
                    e.etag = None;
                    store.put_event(&e)?;
                }
                store.set_sync_state(&cal.id, None, None)
            }
            Err(e) => Err(e),
        }
    }

    fn delete_event(&mut self, _cal: &Calendar, ev: &Event) -> Result<()> {
        let Some(href) = &ev.href else { return Ok(()) };
        match self.dav("DELETE", href, None, None, &[]) {
            Err(e) if !matches!(status_of(&e), Some(404 | 410)) => Err(e),
            _ => Ok(()),
        }
    }

    fn push_task(&mut self, cal: &Calendar, t: &Task, store: &Store) -> Result<()> {
        let href = t.href.clone().unwrap_or_else(|| Self::new_href(cal, &t.uid));
        let etag = if t.dirty == Dirty::Created { None } else { t.etag.as_deref() };
        let new_etag = self.put(&href, etag, &crate::ical::task_resource(t))?;
        let mut done = t.clone();
        done.href = Some(href);
        done.etag = new_etag;
        done.dirty = Dirty::Clean;
        store.put_task(&done)
    }

    fn delete_task(&mut self, _cal: &Calendar, t: &Task) -> Result<()> {
        let Some(href) = &t.href else { return Ok(()) };
        match self.dav("DELETE", href, None, None, &[]) {
            Err(e) if !matches!(status_of(&e), Some(404 | 410)) => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testing::{MockServer, Reply};
    use super::*;
    use std::sync::{Arc, Mutex};

    type Files = Arc<Mutex<HashMap<String, (String, String)>>>;

    fn ics(uid: &str, title: &str, day: u32) -> String {
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nDTSTART:202610{day:02}T100000Z\r\nDTEND:202610{day:02}T110000Z\r\nSUMMARY:{title}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n")
    }

    fn ms(inner: &str) -> String {
        format!(r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/" xmlns:a="http://apple.com/ns/ical/">{inner}</d:multistatus>"#)
    }

    /// A small in-memory CalDAV server: /dav/cal/ holds events.
    fn server(with_sync: bool) -> (MockServer, Files) {
        let files: Files = Arc::new(Mutex::new(HashMap::new()));
        files.lock().unwrap().insert("/dav/cal/a.ics".into(), ("\"1\"".into(), ics("a", "Alpha", 21)));
        files.lock().unwrap().insert("/dav/cal/b.ics".into(), ("\"1\"".into(), ics("b", "Beta", 22)));
        let f = files.clone();
        let s = MockServer::start(move |req| -> Reply {
            assert_eq!(req.headers.get("authorization").map(String::as_str), Some("Basic dTpw"));
            let mut files = f.lock().unwrap();
            match (req.method.as_str(), req.path.as_str()) {
                ("PROPFIND", "/") => (404, "").into(),
                ("PROPFIND", "/.well-known/caldav") => Reply { status: 301, body: String::new(), headers: vec![("Location".into(), "/dav/".into())] },
                ("PROPFIND", "/dav/") => (207, ms("<d:response><d:href>/dav/</d:href><d:propstat><d:prop><d:current-user-principal><d:href>/dav/principals/u/</d:href></d:current-user-principal></d:prop></d:propstat></d:response>")).into(),
                ("PROPFIND", "/dav/principals/u/") => (207, ms("<d:response><d:href>/dav/principals/u/</d:href><d:propstat><d:prop><c:calendar-home-set><d:href>/dav/home/</d:href></c:calendar-home-set></d:prop></d:propstat></d:response>")).into(),
                ("PROPFIND", "/dav/home/") => (207, ms(r#"
                    <d:response><d:href>/dav/home/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
                    <d:response><d:href>/dav/cal/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Work</d:displayname><a:calendar-color>#FF2968FF</a:calendar-color><c:supported-calendar-component-set><c:comp name="VEVENT"/></c:supported-calendar-component-set></d:prop></d:propstat></d:response>
                    <d:response><d:href>/dav/todo/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/><c:calendar/></d:resourcetype><d:displayname>Reminders</d:displayname><c:supported-calendar-component-set><c:comp name="VTODO"/></c:supported-calendar-component-set><d:current-user-privilege-set><d:privilege><d:read/></d:privilege></d:current-user-privilege-set></d:prop></d:propstat></d:response>"#)).into(),
                ("PROPFIND", "/dav/cal/") => (207, ms(&format!("<d:response><d:href>/dav/cal/</d:href><d:propstat><d:prop><cs:getctag>{}</cs:getctag></d:prop></d:propstat></d:response>", files.len() * 7))).into(),
                ("REPORT", "/dav/cal/") if req.body.contains("sync-collection") => {
                    if !with_sync {
                        return (501, "").into();
                    }
                    let mut out = String::new();
                    if req.body.contains("<d:sync-token>tok1</d:sync-token>") {
                        // Since tok1: b deleted, a changed (we fake it by etag).
                        out.push_str("<d:response><d:href>/dav/cal/b.ics</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>");
                        out.push_str(&format!("<d:response><d:href>/dav/cal/a.ics</d:href><d:propstat><d:prop><d:getetag>{}</d:getetag></d:prop></d:propstat></d:response>", files["/dav/cal/a.ics"].0));
                        return (207, ms(&format!("{out}<d:sync-token>tok2</d:sync-token>"))).into();
                    }
                    for (h, (e, _)) in files.iter() {
                        out.push_str(&format!("<d:response><d:href>{h}</d:href><d:propstat><d:prop><d:getetag>{e}</d:getetag></d:prop></d:propstat></d:response>"));
                    }
                    (207, ms(&format!("{out}<d:sync-token>tok1</d:sync-token>"))).into()
                }
                ("REPORT", "/dav/cal/") if req.body.contains("calendar-query") => {
                    let out: String = files.iter().map(|(h, (e, _))| format!("<d:response><d:href>{h}</d:href><d:propstat><d:prop><d:getetag>{e}</d:getetag></d:prop></d:propstat></d:response>")).collect();
                    (207, ms(&out)).into()
                }
                ("REPORT", "/dav/cal/") if req.body.contains("calendar-multiget") => {
                    let mut out = String::new();
                    for (h, (e, data)) in files.iter() {
                        if req.body.contains(&format!("<d:href>{h}</d:href>")) {
                            out.push_str(&format!("<d:response><d:href>{h}</d:href><d:propstat><d:prop><d:getetag>{e}</d:getetag><c:calendar-data>{}</c:calendar-data></d:prop></d:propstat></d:response>", xml_escape(data)));
                        }
                    }
                    (207, ms(&out)).into()
                }
                ("PUT", path) => {
                    let current = files.get(path).map(|f| f.0.clone());
                    match (req.headers.get("if-match"), req.headers.get("if-none-match"), &current) {
                        (Some(m), _, Some(c)) if m != c => return (412, "").into(),
                        (_, Some(_), Some(_)) => return (412, "").into(),
                        _ => {}
                    }
                    files.insert(path.into(), ("\"9\"".into(), req.body.clone()));
                    Reply { status: 201, body: String::new(), headers: vec![("ETag".into(), "\"9\"".into())] }
                }
                ("DELETE", path) => {
                    files.remove(path);
                    (204, "").into()
                }
                (m, p) => (500, format!("unexpected {m} {p}")).into(),
            }
        });
        (s, files)
    }

    fn setup(with_sync: bool) -> (MockServer, Files, CalDav, Store, Calendar) {
        let (s, files) = server(with_sync);
        let mut dav = CalDav::new("d", &s.url, "u", "p");
        let cals = dav.calendars().unwrap();
        assert_eq!(cals.len(), 2);
        assert_eq!(cals[0].name, "Work");
        assert_eq!(cals[0].color, "#FF2968");
        assert_eq!(cals[1].kind, CalendarKind::Tasks);
        assert!(cals[1].read_only);
        let store = Store::in_memory().unwrap();
        store.sync_calendars("d", &cals).unwrap();
        let cal = cals[0].clone();
        (s, files, dav, store, cal)
    }

    #[test]
    fn discovery_sync_collection_and_writes() {
        let (_s, files, mut dav, store, cal) = setup(true);
        dav.sync_events(&cal, &store).unwrap();
        let titles = |store: &Store| {
            let mut t: Vec<String> = store.events_in_calendar(&cal.id).unwrap().into_iter().map(|e| e.title).collect();
            t.sort();
            t
        };
        assert_eq!(titles(&store), vec!["Alpha", "Beta"]);
        let cal = store.calendar(&cal.id).unwrap().unwrap();
        assert_eq!(cal.sync_token.as_deref(), Some("tok1"));
        let alpha_id = store.events_by_uid(&cal.id, "a").unwrap()[0].id.clone();

        // Server side: a changes, b goes away.
        files.lock().unwrap().insert("/dav/cal/a.ics".into(), ("\"2\"".into(), ics("a", "Alpha 2", 21)));
        files.lock().unwrap().remove("/dav/cal/b.ics");
        dav.sync_events(&cal, &store).unwrap();
        assert_eq!(titles(&store), vec!["Alpha 2"]);
        // Same local id after the update.
        assert_eq!(store.events_by_uid(&cal.id, "a").unwrap()[0].id, alpha_id);

        // Create, then edit with If-Match, then delete.
        let mut ev = Event { calendar_id: cal.id.clone(), title: "Gamma".into(), start: 1_800_000_000, end: 1_800_001_800, ..Default::default() };
        store.save_event(&mut ev, true).unwrap();
        dav.push_event(&cal, &store.event(&ev.id).unwrap().unwrap(), &store).unwrap();
        let pushed = store.event(&ev.id).unwrap().unwrap();
        let href = pushed.href.clone().unwrap();
        assert!(href.starts_with("/dav/cal/") && href.ends_with(".ics"));
        assert_eq!(pushed.etag.as_deref(), Some("\"9\""));
        assert!(files.lock().unwrap()[&href].1.contains("SUMMARY:Gamma"));
        let mut edit = pushed.clone();
        edit.title = "Gamma 2".into();
        store.save_event(&mut edit, true).unwrap();
        dav.push_event(&cal, &store.event(&ev.id).unwrap().unwrap(), &store).unwrap();
        assert!(files.lock().unwrap()[&href].1.contains("SUMMARY:Gamma 2"));
        // A stale etag is a conflict; the server's copy wins.
        let mut stale = store.event(&ev.id).unwrap().unwrap();
        stale.etag = Some("\"old\"".into());
        stale.dirty = Dirty::Updated;
        store.put_event(&stale).unwrap();
        dav.push_event(&cal, &stale, &store).unwrap();
        assert_eq!(store.event(&ev.id).unwrap().unwrap().dirty, Dirty::Clean);
        dav.delete_event(&cal, &pushed).unwrap();
        assert!(!files.lock().unwrap().contains_key(&href));
    }

    #[test]
    fn etag_fallback_without_sync_collection() {
        let (_s, files, mut dav, store, cal) = setup(false);
        dav.sync_events(&cal, &store).unwrap();
        assert_eq!(store.events_in_calendar(&cal.id).unwrap().len(), 2);
        let cal = store.calendar(&cal.id).unwrap().unwrap();
        assert!(cal.ctag.is_some());
        files.lock().unwrap().remove("/dav/cal/b.ics");
        dav.sync_events(&cal, &store).unwrap();
        let left = store.events_in_calendar(&cal.id).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].title, "Alpha");
    }
}
