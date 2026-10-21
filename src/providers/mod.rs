//! Calendar providers. Each one maps a service's calendars, events and
//! tasks onto the cache: it lists the account's calendars, pulls changes
//! incrementally into the [`Store`], and sends local edits back.

pub mod caldav;
pub mod google;
pub mod microsoft;

use crate::config::{Account, Settings};
use crate::model::{AccountKind, Calendar, Event, Task};
use crate::oauth::{OAuthConfig, Tokens};
use crate::secrets::Secrets;
use crate::store::Store;
use anyhow::{anyhow, bail, Result};
use std::sync::Arc;
use std::time::Duration;

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
    }
}

/// Keyring key for a Google client's secret.
pub fn google_secret_key(client_id: &str) -> String {
    format!("google-client-secret:{client_id}")
}

/// Bearer tokens for an OAuth account, refreshed when they expire. A
/// rotated refresh token goes back into the keyring.
pub struct TokenSource {
    cfg: Option<OAuthConfig>,
    refresh_token: String,
    secrets: Option<(Arc<dyn Secrets>, String, String)>,
    cached: Option<Tokens>,
}

impl TokenSource {
    pub fn new(cfg: OAuthConfig, refresh_token: String, secrets: Arc<dyn Secrets>, key: String, label: String) -> Self {
        Self { cfg: Some(cfg), refresh_token, secrets: Some((secrets, key, label)), cached: None }
    }

    /// A fixed token, for tests.
    #[cfg(test)]
    pub fn fixed(token: &str) -> Self {
        Self {
            cfg: None,
            refresh_token: String::new(),
            secrets: None,
            cached: Some(Tokens { access_token: token.into(), refresh_token: None, expires_at: i64::MAX }),
        }
    }

    pub fn bearer(&mut self) -> Result<String> {
        let now = chrono::Utc::now().timestamp();
        if let Some(t) = self.cached.as_ref().filter(|t| t.expires_at > now) {
            return Ok(format!("Bearer {}", t.access_token));
        }
        let cfg = self.cfg.as_ref().ok_or_else(|| anyhow!("not signed in"))?;
        let tokens = crate::oauth::refresh(cfg, &self.refresh_token)?;
        if let (Some(new), Some((secrets, key, label))) = (&tokens.refresh_token, &self.secrets) {
            if *new != self.refresh_token {
                secrets.set(key, label, new)?;
                self.refresh_token = new.clone();
            }
        }
        let header = format!("Bearer {}", tokens.access_token);
        self.cached = Some(tokens);
        Ok(header)
    }
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

// ---- HTTP ----

/// A non-success HTTP status, so callers can react to 404/410/412.
#[derive(Debug)]
pub struct HttpError {
    pub status: u16,
    pub body: String,
}

impl HttpError {
    pub fn summary(&self) -> String {
        let body: String = self.body.chars().take(200).collect();
        format!("HTTP {}{}", self.status, if body.trim().is_empty() { String::new() } else { format!(": {}", body.trim()) })
    }
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary())
    }
}

impl std::error::Error for HttpError {}

pub fn status_of(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<HttpError>().map(|h| h.status)
}

pub fn agent(follow_redirects: bool) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(30))
        .redirects(if follow_redirects { 5 } else { 0 })
        .user_agent(concat!("m3-calendar/", env!("CARGO_PKG_VERSION")))
        .build()
}

/// Sends a request, turning error statuses into [`HttpError`].
pub fn send(req: ureq::Request, body: Option<&str>) -> Result<ureq::Response> {
    let res = match body {
        Some(b) => req.send_string(b),
        None => req.call(),
    };
    match res {
        Ok(r) => Ok(r),
        Err(ureq::Error::Status(status, r)) => {
            Err(HttpError { status, body: r.into_string().unwrap_or_default() }.into())
        }
        Err(e) => Err(anyhow!("{e}")),
    }
}

pub fn form_post(url: &str, fields: &[(&str, String)]) -> Result<String> {
    let pairs: Vec<(&str, &str)> = fields.iter().map(|(k, v)| (*k, v.as_str())).collect();
    match agent(true).post(url).send_form(&pairs) {
        Ok(r) => Ok(r.into_string()?),
        Err(ureq::Error::Status(status, r)) => {
            Err(HttpError { status, body: r.into_string().unwrap_or_default() }.into())
        }
        Err(e) => Err(anyhow!("{e}")),
    }
}

/// Percent-encodes one path segment (calendar ids contain '@' and '#').
pub fn seg(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20")
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
pub mod testing {
    //! A tiny HTTP/1.1 server for provider tests.

    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug)]
    pub struct Request {
        pub method: String,
        pub path: String,
        pub headers: HashMap<String, String>,
        pub body: String,
    }

    pub struct Reply {
        pub status: u16,
        pub body: String,
        pub headers: Vec<(String, String)>,
    }

    impl From<(u16, String)> for Reply {
        fn from((status, body): (u16, String)) -> Self {
            Reply { status, body, headers: vec![] }
        }
    }

    impl From<(u16, &str)> for Reply {
        fn from((status, body): (u16, &str)) -> Self {
            Reply { status, body: body.into(), headers: vec![] }
        }
    }

    pub struct MockServer {
        pub url: String,
        pub requests: Arc<Mutex<Vec<Request>>>,
    }

    impl MockServer {
        pub fn start<R: Into<Reply>>(handler: impl Fn(&Request) -> R + Send + Sync + 'static) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let requests = Arc::new(Mutex::new(Vec::new()));
            let log = requests.clone();
            let handler = Arc::new(move |r: &Request| handler(r).into());
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let log = log.clone();
                    let handler = handler.clone();
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut out = stream;
                        loop {
                            let mut line = String::new();
                            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                                return;
                            }
                            let mut parts = line.split_whitespace();
                            let method = parts.next().unwrap_or("").to_string();
                            let path = parts.next().unwrap_or("").to_string();
                            let mut headers = HashMap::new();
                            loop {
                                let mut h = String::new();
                                if reader.read_line(&mut h).unwrap_or(0) == 0 {
                                    return;
                                }
                                let h = h.trim_end();
                                if h.is_empty() {
                                    break;
                                }
                                if let Some((k, v)) = h.split_once(':') {
                                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
                                }
                            }
                            let len: usize = headers.get("content-length").and_then(|l| l.parse().ok()).unwrap_or(0);
                            let mut body = vec![0; len];
                            if reader.read_exact(&mut body).is_err() {
                                return;
                            }
                            let req = Request { method, path, headers, body: String::from_utf8_lossy(&body).into_owned() };
                            log.lock().unwrap().push(req.clone());
                            let reply: Reply = handler(&req);
                            let mut head = format!("HTTP/1.1 {} X\r\nContent-Length: {}\r\n", reply.status, reply.body.len());
                            for (k, v) in &reply.headers {
                                head.push_str(&format!("{k}: {v}\r\n"));
                            }
                            head.push_str("\r\n");
                            if out.write_all(head.as_bytes()).and_then(|_| out.write_all(reply.body.as_bytes())).is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            MockServer { url, requests }
        }

        pub fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_rotates_the_stored_token() {
        let server = testing::MockServer::start(|req| {
            assert!(req.body.contains("grant_type=refresh_token"));
            assert!(req.body.contains("refresh_token=old"));
            (200u16, r#"{"access_token":"fresh","refresh_token":"new","expires_in":3600}"#)
        });
        let secrets: Arc<dyn Secrets> = Arc::new(crate::secrets::Memory::default());
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
        assert_eq!(seg("a@b.com#x"), "a%40b.com%23x");
    }
}
