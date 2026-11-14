//! Google sign-in and Google Drive storage.
//!
//! Google is only the door and the shelf: the user signs in so we may use their private
//! `appDataFolder` (a hidden app folder in their Drive, scope `drive.appdata`). Everything we put
//! there is already encrypted with a key Google never sees. Sign-in is the standard desktop flow:
//! browser + loopback redirect on 127.0.0.1 + PKCE; no password of the user's Google account
//! ever reaches this program.

#![allow(clippy::result_large_err)] // ureq::Error is big; these are cold paths

use super::backend::Backend;
use crate::daemon::ipc::base64_encode;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, ErrorKind, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// The OAuth client built into release binaries. It is not in the source: the release build
/// (`.github/workflows/release.yml`) takes it from the repository secrets through these two
/// environment variables at compile time. Empty in other builds; then `config.txt`
/// (`google_client_id`, `google_client_secret`) must provide one.
/// For installed apps these values are public by design (RFC 8252): they only tell Google which
/// app asks. Your data is protected by your own sign-in and by your sync password.
pub const DEFAULT_CLIENT_ID: &str = match option_env!("CLIPKEEP_GOOGLE_CLIENT_ID") {
    Some(v) => v,
    None => "",
};
pub const DEFAULT_CLIENT_SECRET: &str = match option_env!("CLIPKEEP_GOOGLE_CLIENT_SECRET") {
    Some(v) => v,
    None => "",
};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
const UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files";
const SCOPE: &str = "https://www.googleapis.com/auth/drive.appdata";

fn other(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

fn b64url(bytes: &[u8]) -> String {
    base64_encode(bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

pub fn pkce_challenge(verifier: &str) -> String {
    b64url(&Sha256::digest(verifier.as_bytes()))
}

fn percent(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn unpercent(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Reads the redirect request Google's browser page sends to us and returns the `code`.
pub fn read_redirect(listener: &TcpListener, expected_state: &str) -> io::Result<String> {
    let (mut stream, _) = listener.accept()?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut line = String::new();
    BufReader::new(&stream).read_line(&mut line)?;
    let query = line
        .split_whitespace()
        .nth(1)
        .and_then(|p| p.split_once('?'))
        .map_or("", |x| x.1)
        .to_owned();
    let mut code = None;
    let mut state = None;
    let mut error = None;
    for kv in query.split('&') {
        match kv.split_once('=') {
            Some(("code", v)) => code = Some(unpercent(v)),
            Some(("state", v)) => state = Some(unpercent(v)),
            Some(("error", v)) => error = Some(unpercent(v)),
            _ => {}
        }
    }
    let ok = code.is_some() && state.as_deref() == Some(expected_state);
    let body = result_page(ok);
    let _ = write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if let Some(e) = error {
        return Err(other(format!("Google refused the sign-in: {e}")));
    }
    match (ok, code) {
        (true, Some(code)) => Ok(code),
        _ => Err(other(
            "the sign-in answer did not match this request (wrong state)",
        )),
    }
}

/// The page the browser shows after Google redirects back to us.
fn result_page(ok: bool) -> String {
    let (badge, mark, title, text) = if ok {
        (
            "#2FAE6B",
            "&#10003;",
            "You are signed in",
            "You can close this tab and go back to clipkeep.",
        )
    } else {
        (
            "#D9534F",
            "!",
            "Sign-in did not work",
            "Close this tab, go back to clipkeep and try again.",
        )
    };
    include_str!("../../assets/sign-in-page.html")
        .replace("@BADGE@", badge)
        .replace("@MARK@", mark)
        .replace("@TITLE@", title)
        .replace("@TEXT@", text)
}

fn open_browser(url: &str) {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let _ = std::process::Command::new(cmd)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

/// Interactive sign-in. Returns the refresh token to keep.
pub fn sign_in(client_id: &str, client_secret: &str) -> io::Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let redirect = format!("http://127.0.0.1:{}", listener.local_addr()?.port());
    let verifier = b64url(&crate::crypto::random_key());
    let state = b64url(&crate::crypto::random_key()[..16]);
    let url = format!(
        "{AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&code_challenge={}&code_challenge_method=S256&access_type=offline&prompt=consent&state={state}",
        percent(client_id),
        percent(&redirect),
        percent(SCOPE),
        pkce_challenge(&verifier)
    );
    println!("Opening the browser to sign in with Google. If nothing opens, visit:\n\n{url}\n");
    open_browser(&url);
    let code = read_redirect(&listener, &state)?;
    let resp: serde_json::Value = ureq::post(TOKEN_URL)
        .send_form(&[
            ("code", &code),
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("redirect_uri", &redirect),
            ("grant_type", "authorization_code"),
            ("code_verifier", &verifier),
        ])
        .map_err(|e| other(format!("token request failed: {e}")))?
        .into_json()?;
    resp["refresh_token"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| other("Google returned no refresh token (try again; remove the app at myaccount.google.com/permissions first)"))
}

pub struct GoogleDrive {
    client_id: String,
    client_secret: String,
    refresh_token: String,
    access: Option<(String, Instant)>,
    ids: HashMap<String, String>,
    agent: ureq::Agent,
}

impl GoogleDrive {
    pub fn new(client_id: &str, client_secret: &str, refresh_token: &str) -> GoogleDrive {
        GoogleDrive {
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            refresh_token: refresh_token.to_owned(),
            access: None,
            ids: HashMap::new(),
            agent: ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(60))
                .build(),
        }
    }

    fn token(&mut self) -> io::Result<String> {
        if let Some((t, until)) = &self.access {
            if Instant::now() < *until {
                return Ok(t.clone());
            }
        }
        let resp: serde_json::Value = self
            .agent
            .post(TOKEN_URL)
            .send_form(&[
                ("client_id", &self.client_id),
                ("client_secret", &self.client_secret),
                ("refresh_token", &self.refresh_token),
                ("grant_type", "refresh_token"),
            ])
            .map_err(|e| other(format!("cannot refresh the Google token: {e}")))?
            .into_json()?;
        let token = resp["access_token"]
            .as_str()
            .ok_or_else(|| other("no access token in reply"))?
            .to_owned();
        let secs = resp["expires_in"]
            .as_u64()
            .unwrap_or(600)
            .saturating_sub(60);
        self.access = Some((token.clone(), Instant::now() + Duration::from_secs(secs)));
        Ok(token)
    }

    /// Runs a request with a bearer token; on 401 gets a fresh token and tries once more.
    fn call(
        &mut self,
        make: impl Fn(&ureq::Agent, &str) -> Result<ureq::Response, ureq::Error>,
    ) -> io::Result<ureq::Response> {
        for attempt in 0..2 {
            let token = self.token()?;
            match make(&self.agent, &token) {
                Ok(r) => return Ok(r),
                Err(ureq::Error::Status(401, _)) if attempt == 0 => self.access = None,
                Err(ureq::Error::Status(404, _)) => {
                    return Err(io::Error::new(ErrorKind::NotFound, "not found on Drive"))
                }
                Err(ureq::Error::Status(code, r)) => {
                    let body = r.into_string().unwrap_or_default();
                    return Err(other(format!(
                        "Google Drive answered {code}: {}",
                        body.chars().take(200).collect::<String>()
                    )));
                }
                Err(e) => return Err(other(format!("network: {e}"))),
            }
        }
        Err(other("Google refused the token twice"))
    }

    fn refresh_ids(&mut self) -> io::Result<()> {
        let mut ids = HashMap::new();
        let mut page = String::new();
        loop {
            let resp: serde_json::Value = self
                .call(|a, t| {
                    let mut r = a
                        .get(FILES_URL)
                        .set("Authorization", &format!("Bearer {t}"))
                        .query("spaces", "appDataFolder")
                        .query("pageSize", "1000")
                        .query("fields", "nextPageToken,files(id,name)");
                    if !page.is_empty() {
                        r = r.query("pageToken", &page);
                    }
                    r.call()
                })?
                .into_json()?;
            for f in resp["files"].as_array().into_iter().flatten() {
                if let (Some(id), Some(name)) = (f["id"].as_str(), f["name"].as_str()) {
                    ids.insert(name.to_owned(), id.to_owned());
                }
            }
            match resp["nextPageToken"].as_str() {
                Some(next) => page = next.to_owned(),
                None => break,
            }
        }
        self.ids = ids;
        Ok(())
    }
}

/// Body of a `multipart/related` upload: JSON metadata, then the bytes.
pub fn multipart_body(boundary: &str, name: &str, data: &[u8]) -> Vec<u8> {
    let meta = serde_json::json!({ "name": name, "parents": ["appDataFolder"] });
    let mut body = Vec::new();
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(
        format!("--{boundary}\r\nContent-Type: application/octet-stream\r\n\r\n").as_bytes(),
    );
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--").as_bytes());
    body
}

impl Backend for GoogleDrive {
    fn name(&self) -> String {
        "Google Drive (hidden app folder)".into()
    }

    fn list(&mut self) -> io::Result<Vec<String>> {
        self.refresh_ids()?;
        Ok(self.ids.keys().cloned().collect())
    }

    fn get(&mut self, name: &str) -> io::Result<Vec<u8>> {
        if !self.ids.contains_key(name) {
            self.refresh_ids()?;
        }
        let id = self
            .ids
            .get(name)
            .cloned()
            .ok_or_else(|| io::Error::new(ErrorKind::NotFound, name.to_owned()))?;
        let resp = self.call(|a, t| {
            a.get(&format!("{FILES_URL}/{id}"))
                .set("Authorization", &format!("Bearer {t}"))
                .query("alt", "media")
                .call()
        })?;
        let mut out = Vec::new();
        resp.into_reader().take(256 << 20).read_to_end(&mut out)?;
        Ok(out)
    }

    fn put(&mut self, name: &str, data: &[u8]) -> io::Result<()> {
        if self.ids.is_empty() {
            self.refresh_ids()?;
        }
        if let Some(id) = self.ids.get(name).cloned() {
            self.call(|a, t| {
                a.request("PATCH", &format!("{UPLOAD_URL}/{id}"))
                    .set("Authorization", &format!("Bearer {t}"))
                    .query("uploadType", "media")
                    .set("Content-Type", "application/octet-stream")
                    .send_bytes(data)
            })?;
            return Ok(());
        }
        let boundary = b64url(&crate::crypto::random_key()[..12]);
        let body = multipart_body(&boundary, name, data);
        let resp: serde_json::Value = self
            .call(|a, t| {
                a.post(UPLOAD_URL)
                    .set("Authorization", &format!("Bearer {t}"))
                    .query("uploadType", "multipart")
                    .query("fields", "id")
                    .set(
                        "Content-Type",
                        &format!("multipart/related; boundary={boundary}"),
                    )
                    .send_bytes(&body)
            })?
            .into_json()?;
        if let Some(id) = resp["id"].as_str() {
            self.ids.insert(name.to_owned(), id.to_owned());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_page_has_no_leftover_placeholders() {
        for ok in [true, false] {
            let page = result_page(ok);
            assert!(!page.contains('@'), "{page}");
            assert!(page.contains("<svg"));
        }
        assert!(result_page(true).contains("signed in"));
    }

    #[test]
    fn pkce_matches_the_rfc_7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn percent_encoding_roundtrip() {
        let s = "http://127.0.0.1:5000/a b?x=1&y=ж";
        assert_eq!(unpercent(&percent(s)), s);
        assert_eq!(percent("a b"), "a%20b");
        assert_eq!(unpercent("4%2F0Ab%2Dc"), "4/0Ab-c");
        assert_eq!(unpercent("100%"), "100%");
        assert_eq!(unpercent("%ж%4"), "%ж%4");
        assert_eq!(unpercent("a+b"), "a b");
    }

    #[test]
    fn redirect_is_parsed_and_state_is_checked() {
        for (state_sent, ok) in [("good", true), ("evil", false)] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let t = std::thread::spawn(move || {
                let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
                write!(
                    s,
                    "GET /?state={state_sent}&code=4%2F0AbC&scope=x HTTP/1.1\r\nHost: x\r\n\r\n"
                )
                .unwrap();
                let mut reply = String::new();
                s.read_to_string(&mut reply).unwrap();
                reply
            });
            let got = read_redirect(&listener, "good");
            let reply = t.join().unwrap();
            assert!(reply.starts_with("HTTP/1.1 200"));
            assert_eq!(got.is_ok(), ok);
            if ok {
                assert_eq!(got.unwrap(), "4/0AbC");
            }
        }
    }

    #[test]
    fn multipart_has_metadata_then_payload() {
        let body =
            String::from_utf8(multipart_body("B", "dev-1-00000001.ops", b"PAYLOAD")).unwrap();
        assert!(body.starts_with("--B\r\n"));
        assert!(body.contains("\"parents\":[\"appDataFolder\"]"));
        assert!(body.contains("\"name\":\"dev-1-00000001.ops\""));
        assert!(body.ends_with("PAYLOAD\r\n--B--"));
    }
}
