//! Calendar providers. Each one maps a service's calendars, events and
//! tasks onto the cache: it lists the account's calendars, pulls changes
//! incrementally into the [`Store`], and sends local edits back.

pub mod caldav;
pub mod google;
pub mod microsoft;

use crate::config::{Account, Settings};
use crate::model::{AccountKind, Calendar, Event, Task};
use crate::store::Store;
use anyhow::{anyhow, bail, Result};
use material_3_ui::oauth::OAuthConfig;
use material_3_ui::secrets::Secrets;
use std::sync::Arc;

pub use material_3_ui::http::{agent, path_segment, send, status_of, HttpError};
pub use material_3_ui::oauth::TokenSource;

/// The User-Agent of every request.
pub const USER_AGENT: &str = concat!("m3-calendar/", env!("CARGO_PKG_VERSION"));

pub trait Provider {
    /// The account's calendars and task lists, with local ids.
    fn calendars(&mut self) -> Result<Vec<Calendar>>;
    /// Pulls changes since the calendar's sync token into the store.
    fn sync_events(&mut self, cal: &Calendar, store: &Store) -> Result<()>;
    /// Creates or updates an event (and its edited instances); marks it clean.
    fn push_event(&mut self, cal: &Calendar, ev: &Event, store: &Store) -> Result<()>;
    fn delete_event(&mut self, cal: &Calendar, ev: &Event) -> Result<()>;
    fn sync_tasks(&mut self, _cal: &Calendar, _store: &Store) -> Result<()> {
        Ok(())
    }
    fn push_task(&mut self, _cal: &Calendar, _t: &Task, _store: &Store) -> Result<()> {
        bail!("this account doesn't support tasks")
    }
    fn delete_task(&mut self, _cal: &Calendar, _t: &Task) -> Result<()> {
        bail!("this account doesn't support tasks")
    }
}

/// Service base URLs; tests point them at mock servers.
#[derive(Clone, Debug)]
pub struct Endpoints {
    pub google_api: String,
    pub google_auth: String,
    pub google_token: String,
    pub graph: String,
    pub ms_auth: String,
    pub ms_token: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            google_api: "https://www.googleapis.com".into(),
            google_auth: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            google_token: "https://oauth2.googleapis.com/token".into(),
            graph: "https://graph.microsoft.com/v1.0".into(),
            ms_auth: "https://login.microsoftonline.com/common/oauth2/v2.0/authorize".into(),
            ms_token: "https://login.microsoftonline.com/common/oauth2/v2.0/token".into(),
        }
    }
}

pub const ICLOUD_URL: &str = "https://caldav.icloud.com";

pub fn google_oauth(ep: &Endpoints, client_id: &str, client_secret: Option<String>) -> OAuthConfig {
    OAuthConfig {
        auth_url: ep.google_auth.clone(),
        token_url: ep.google_token.clone(),
        client_id: client_id.into(),
        client_secret,
        scopes: vec![
            "https://www.googleapis.com/auth/calendar".into(),
            "https://www.googleapis.com/auth/tasks".into(),
        ],
        // Without these Google doesn't hand out a refresh token.
        extra: vec![("access_type".into(), "offline".into()), ("prompt".into(), "consent".into())],
        redirect_host: "127.0.0.1".into(),
        app_name: "Calendar".into(),
        ..Default::default()
    }
}

pub fn microsoft_oauth(ep: &Endpoints, client_id: &str) -> OAuthConfig {
    OAuthConfig {
        auth_url: ep.ms_auth.clone(),
        token_url: ep.ms_token.clone(),
        client_id: client_id.into(),
        client_secret: None,
        scopes: vec![
            "offline_access".into(),
            "User.Read".into(),
            "Calendars.ReadWrite".into(),
            "Tasks.ReadWrite".into(),
        ],
        extra: vec![("prompt".into(), "select_account".into())],
        redirect_host: "localhost".into(),
        app_name: "Calendar".into(),
        ..Default::default()
    }
}

/// Keyring key for a Google client's secret.
pub fn google_secret_key(client_id: &str) -> String {
    format!("google-client-secret:{client_id}")
}

/// The provider for an account, with its secret from the keyring.
pub fn for_account(
    account: &Account,
    settings: &Settings,
    secrets: Arc<dyn Secrets>,
    ep: &Endpoints,
) -> Result<Option<Box<dyn Provider>>> {
    if account.kind == AccountKind::Local {
        return Ok(None);
    }
    let secret = secrets
        .get(&account.secret_key())?
        .ok_or_else(|| anyhow!("{} needs you to sign in again", account.name))?;
    let label = format!("Material 3 Calendar: {}", account.name);
    Ok(Some(match account.kind {
        AccountKind::Google => {
            let client = Some(account.client_id.clone())
                .filter(|c| !c.is_empty())
                .or_else(|| settings.google_client())
                .ok_or_else(|| anyhow!("set a Google client ID in Preferences"))?;
            let client_secret = secrets.get(&google_secret_key(&client))?;
            let tokens = TokenSource::new(google_oauth(ep, &client, client_secret), secret, secrets, account.secret_key(), label);
            Box::new(google::Google::new(&account.id, &ep.google_api, tokens))
        }
        AccountKind::Microsoft => {
            let client = Some(account.client_id.clone())
                .filter(|c| !c.is_empty())
                .or_else(|| settings.microsoft_client())
                .ok_or_else(|| anyhow!("set a Microsoft client ID in Preferences"))?;
            let tokens = TokenSource::new(microsoft_oauth(ep, &client), secret, secrets, account.secret_key(), label);
            Box::new(microsoft::Microsoft::new(&account.id, &ep.graph, tokens))
        }
        AccountKind::Icloud | AccountKind::Caldav => {
            let url = if account.url.is_empty() { ICLOUD_URL.to_string() } else { account.url.clone() };
            Box::new(caldav::CalDav::new(&account.id, &url, &account.username, &secret))
        }
        AccountKind::Local => unreachable!(),
    }))
}

// ---- XML ----

/// A namespace-agnostic XML tree (WebDAV answers are small).
#[derive(Debug, Default, Clone)]
pub struct Node {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub text: String,
    pub children: Vec<Node>,
}

impl Node {
    pub fn child(&self, name: &str) -> Option<&Node> {
        self.children.iter().find(|c| c.name == name)
    }

    /// First descendant with this local name.
    pub fn find(&self, name: &str) -> Option<&Node> {
        for c in &self.children {
            if c.name == name {
                return Some(c);
            }
            if let Some(n) = c.find(name) {
                return Some(n);
            }
        }
        None
    }

    pub fn find_all<'a>(&'a self, name: &str, out: &mut Vec<&'a Node>) {
        for c in &self.children {
            if c.name == name {
                out.push(c);
            } else {
                c.find_all(name, out);
            }
        }
    }

    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    pub fn text_of(&self, name: &str) -> Option<String> {
        self.find(name).map(|n| n.text.trim().to_string())
    }
}

pub fn parse_xml(text: &str) -> Result<Node> {
    use quick_xml::events::Event as X;
    let mut reader = quick_xml::Reader::from_str(text);
    let mut stack = vec![Node::default()];
    loop {
        match reader.read_event()? {
            X::Start(e) => stack.push(element(&e)),
            X::Empty(e) => {
                let node = element(&e);
                stack.last_mut().unwrap().children.push(node);
            }
            X::End(_) => {
                if stack.len() > 1 {
                    let node = stack.pop().unwrap();
                    stack.last_mut().unwrap().children.push(node);
                }
            }
            X::Text(t) => stack.last_mut().unwrap().text.push_str(&t.decode()?),
            X::CData(t) => stack.last_mut().unwrap().text.push_str(&t.decode()?),
            X::GeneralRef(r) => {
                let s = if let Some(c) = r.resolve_char_ref()? {
                    c.to_string()
                } else {
                    match r.decode()?.as_ref() {
                        "amp" => "&",
                        "lt" => "<",
                        "gt" => ">",
                        "quot" => "\"",
                        "apos" => "'",
                        _ => "",
                    }
                    .to_string()
                };
                stack.last_mut().unwrap().text.push_str(&s);
            }
            X::Eof => break,
            _ => {}
        }
    }
    while stack.len() > 1 {
        let node = stack.pop().unwrap();
        stack.last_mut().unwrap().children.push(node);
    }
    Ok(stack.pop().unwrap())
}

fn element(e: &quick_xml::events::BytesStart) -> Node {
    Node {
        name: String::from_utf8_lossy(e.local_name().as_ref()).into_owned(),
        attrs: e
            .attributes()
            .flatten()
            .map(|a| {
                (
                    String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned(),
                    a.unescape_value().map(|v| v.into_owned()).unwrap_or_default(),
                )
            })
            .collect(),
        ..Default::default()
    }
}

pub fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A Google/Graph style color, normalized to `#rrggbb`.
pub fn norm_color(c: &str, fallback_index: usize) -> String {
    let c = c.trim();
    if c.len() >= 7 && c.starts_with('#') && c[1..7].chars().all(|x| x.is_ascii_hexdigit()) {
        c[..7].to_uppercase()
    } else {
        crate::model::CALENDAR_COLORS[fallback_index % crate::model::CALENDAR_COLORS.len()].to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use material_3_ui::testing::MockServer;

    #[test]
    fn refresh_rotates_the_stored_token() {
        let server = MockServer::start(|req| {
            assert!(req.body.contains("grant_type=refresh_token"));
            assert!(req.body.contains("refresh_token=old"));
            (200u16, r#"{"access_token":"fresh","refresh_token":"new","expires_in":3600}"#)
        });
        let secrets: Arc<dyn Secrets> = Arc::new(material_3_ui::secrets::Memory::default());
        let mut cfg = microsoft_oauth(&Endpoints::default(), "cid");
        cfg.token_url = format!("{}/token", server.url);
        let mut ts = TokenSource::new(cfg, "old".into(), secrets.clone(), "account:x".into(), "label".into());
        assert_eq!(ts.bearer().unwrap(), "Bearer fresh");
        // Cached: no second request.
        assert_eq!(ts.bearer().unwrap(), "Bearer fresh");
        assert_eq!(server.requests().len(), 1);
        assert_eq!(secrets.get("account:x").unwrap().as_deref(), Some("new"));
    }

    #[test]
    fn xml_tree_ignores_namespaces_and_decodes_entities() {
        let x = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
            <d:response><d:href>/cal/a.ics</d:href><d:propstat><d:prop><d:getetag>"1"</d:getetag>
            <c:calendar-data>BEGIN:VCALENDAR&#13;
SUMMARY:Tom &amp; Jerry</c:calendar-data><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response></d:multistatus>"#;
        let root = parse_xml(x).unwrap();
        let mut responses = Vec::new();
        root.find_all("response", &mut responses);
        assert_eq!(responses.len(), 1);
        assert_eq!(responses[0].text_of("href").as_deref(), Some("/cal/a.ics"));
        assert_eq!(responses[0].text_of("getetag").as_deref(), Some("\"1\""));
        assert!(responses[0].find("calendar-data").unwrap().text.contains("Tom & Jerry"));
        assert!(responses[0].find("collection").is_some());
        assert_eq!(norm_color("#9fe1e7ff", 0), "#9FE1E7");
        
    }
}
