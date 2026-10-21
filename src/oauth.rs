//! OAuth 2.0 for installed apps (RFC 8252): authorization code with PKCE
//! and a loopback redirect. The browser opens the provider's sign-in page,
//! which sends the code back to a one-shot server on 127.0.0.1.

use crate::providers::{form_post, HttpError};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct OAuthConfig {
    pub auth_url: String,
    pub token_url: String,
    pub client_id: String,
    /// Google's "Desktop app" clients come with a (non-confidential) secret
    /// that the token endpoint still asks for.
    pub client_secret: Option<String>,
    pub scopes: Vec<String>,
    pub extra: Vec<(String, String)>,
    /// "127.0.0.1" (Google) or "localhost" (Microsoft only accepts the
    /// registered `http://localhost`, with any port).
    pub redirect_host: String,
}

#[derive(Clone, Debug, Default)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: Option<String>,
    /// Unix time.
    pub expires_at: i64,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

pub fn random_string(len: usize) -> String {
    (0..len).map(|_| UNRESERVED[fastrand::usize(..UNRESERVED.len())] as char).collect()
}

/// (verifier, S256 challenge).
pub fn pkce_pair() -> (String, String) {
    let verifier = random_string(64);
    let challenge = challenge_for(&verifier);
    (verifier, challenge)
}

pub fn challenge_for(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

pub fn authorize_url(cfg: &OAuthConfig, redirect: &str, challenge: &str, state: &str) -> String {
    let mut url = url::Url::parse(&cfg.auth_url).expect("valid auth url");
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("client_id", &cfg.client_id)
            .append_pair("redirect_uri", redirect)
            .append_pair("response_type", "code")
            .append_pair("scope", &cfg.scopes.join(" "))
            .append_pair("code_challenge", challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        for (k, v) in &cfg.extra {
            q.append_pair(k, v);
        }
    }
    url.into()
}

/// Opens the browser and waits (up to `timeout`) for the redirect, then
/// trades the code for tokens.
pub fn sign_in(cfg: &OAuthConfig, open: impl FnOnce(&str) -> Result<()>, timeout: Duration) -> Result<Tokens> {
    let listener = TcpListener::bind("127.0.0.1:0").context("starting the sign-in listener")?;
    let redirect = format!("http://{}:{}", cfg.redirect_host, listener.local_addr()?.port());
    let (verifier, challenge) = pkce_pair();
    let state = random_string(24);
    open(&authorize_url(cfg, &redirect, &challenge, &state))?;
    let code = wait_for_code(&listener, &state, timeout)?;
    exchange_code(cfg, &code, &verifier, &redirect)
}

/// Serves one redirect on `listener` and returns the authorization code.
pub fn wait_for_code(listener: &TcpListener, state: &str, timeout: Duration) -> Result<String> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        let (mut stream, _) = match listener.accept() {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() > deadline {
                    bail!("sign-in timed out");
                }
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let target = line.split_whitespace().nth(1).unwrap_or("/");
        let url = url::Url::parse(&format!("http://127.0.0.1{target}"))?;
        let get = |k: &str| url.query_pairs().find(|(key, _)| key == k).map(|(_, v)| v.into_owned());
        // Browsers also ask for /favicon.ico; ignore anything without a result.
        if get("code").is_none() && get("error").is_none() {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            continue;
        }
        let (ok, result) = match (get("state"), get("code"), get("error")) {
            (_, _, Some(err)) => (false, Err(anyhow!("sign-in was cancelled ({err})"))),
            (Some(s), Some(code), None) if s == state => (true, Ok(code)),
            _ => (false, Err(anyhow!("the sign-in response didn't match this request"))),
        };
        let body = format!(
            "<!doctype html><meta charset=utf-8><title>Material 3 Calendar</title>\
             <body style=\"font-family:sans-serif;display:grid;place-items:center;height:90vh;background:#1b1b1f;color:#e4e1e6\">\
             <div style=\"text-align:center\"><h1>{}</h1><p>You can close this tab and go back to Calendar.</p></div>",
            if ok { "Signed in" } else { "Sign-in didn't finish" }
        );
        let _ = write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        return result;
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn token_request(cfg: &OAuthConfig, mut fields: Vec<(&str, String)>) -> Result<Tokens> {
    fields.push(("client_id", cfg.client_id.clone()));
    if let Some(secret) = &cfg.client_secret {
        fields.push(("client_secret", secret.clone()));
    }
    let body = form_post(&cfg.token_url, &fields).map_err(|e| match e.downcast_ref::<HttpError>() {
        Some(h) if h.status == 400 || h.status == 401 => anyhow!("the provider refused the sign-in ({}); sign in again", h.summary()),
        _ => e,
    })?;
    let r: TokenResponse = serde_json::from_str(&body).context("reading the token response")?;
    Ok(Tokens {
        access_token: r.access_token,
        refresh_token: r.refresh_token,
        expires_at: now() + r.expires_in.unwrap_or(3600) - 60,
    })
}

pub fn exchange_code(cfg: &OAuthConfig, code: &str, verifier: &str, redirect: &str) -> Result<Tokens> {
    token_request(
        cfg,
        vec![
            ("grant_type", "authorization_code".into()),
            ("code", code.into()),
            ("code_verifier", verifier.into()),
            ("redirect_uri", redirect.into()),
        ],
    )
}

pub fn refresh(cfg: &OAuthConfig, refresh_token: &str) -> Result<Tokens> {
    let mut fields = vec![("grant_type", "refresh_token".to_string()), ("refresh_token", refresh_token.to_string())];
    if cfg.client_secret.is_none() {
        // Microsoft wants the scopes again on refresh.
        fields.push(("scope", cfg.scopes.join(" ")));
    }
    token_request(cfg, fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_s256() {
        // BASE64URL(SHA256(verifier)) without padding, checked with Python's hashlib.
        assert_eq!(
            challenge_for("dBjftJeZ4CVP-mJ92K1hW1v3ZIdo-YmT3ZxhAxSF1jE"),
            "Gtg-8BHrMVHkHN_VdjyK0IxQhuNp4vyjnY9g1dmO4WE"
        );
        let (v, c) = pkce_pair();
        assert_eq!(v.len(), 64);
        assert_eq!(challenge_for(&v), c);
    }

    #[test]
    fn authorize_url_has_pkce_fields() {
        let cfg = OAuthConfig {
            auth_url: "https://auth.example/authorize".into(),
            token_url: String::new(),
            client_id: "cid".into(),
            client_secret: None,
            scopes: vec!["a".into(), "b".into()],
            extra: vec![("prompt".into(), "consent".into())],
            redirect_host: "127.0.0.1".into(),
        };
        let u = url::Url::parse(&authorize_url(&cfg, "http://127.0.0.1:5000", "chal", "st")).unwrap();
        let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["scope"], "a b");
        assert_eq!(q["redirect_uri"], "http://127.0.0.1:5000");
        assert_eq!(q["prompt"], "consent");
    }

    #[test]
    fn loopback_flow_end_to_end() {
        // A mock token endpoint.
        let server = crate::providers::testing::MockServer::start(|req| {
            assert_eq!(req.method, "POST");
            assert!(req.body.contains("grant_type=authorization_code"));
            assert!(req.body.contains("code=the-code"));
            assert!(req.body.contains("code_verifier="));
            (200u16, r#"{"access_token":"at","refresh_token":"rt","expires_in":3600}"#)
        });
        let cfg = OAuthConfig {
            auth_url: "https://auth.example/authorize".into(),
            token_url: format!("{}/token", server.url),
            client_id: "cid".into(),
            client_secret: None,
            scopes: vec!["s".into()],
            extra: vec![],
            redirect_host: "127.0.0.1".into(),
        };
        let tokens = sign_in(
            &cfg,
            |url| {
                // Play the browser: follow the redirect with the code.
                let u = url::Url::parse(url).unwrap();
                let q: std::collections::HashMap<_, _> = u.query_pairs().into_owned().collect();
                let back = format!("{}/?code=the-code&state={}", q["redirect_uri"], q["state"]);
                std::thread::spawn(move || {
                    let _ = ureq::get(&back).call();
                });
                Ok(())
            },
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(tokens.access_token, "at");
        assert_eq!(tokens.refresh_token.as_deref(), Some("rt"));
    }
}
