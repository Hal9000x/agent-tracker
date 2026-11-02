//! Sign-in for a tracker reachable from the internet. The owner signs in with the
//! username and password given in the environment; harness hooks send the ingest
//! token instead. A tracker on the local machine has no `Auth` and serves anyone
//! who can reach it.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, header};

const COOKIE: &str = "agent_tracker_session";
const SESSION_LIFETIME: Duration = Duration::from_secs(30 * 24 * 60 * 60);
/// After this many failed sign-ins from one client within the window, that client is
/// refused until its oldest failure leaves the window. Others sign in as usual.
const MAX_FAILURES: usize = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);

pub struct Auth {
    username: String,
    password: String,
    ingest_token: String,
    /// --public-url, where hooks reach this tracker.
    public_url: Option<String>,
    /// Its host name, served besides the local names.
    public_host: Option<String>,
    /// Signed-in session ids and when they expire.
    sessions: Mutex<HashMap<String, Instant>>,
    /// Recent failed sign-ins by client address.
    failures: Mutex<HashMap<String, VecDeque<Instant>>>,
}

pub enum SignIn {
    /// The `Set-Cookie` value for the new session.
    Accepted(String),
    Wrong,
    Locked,
}

impl Auth {
    pub fn new(username: String, password: String, ingest_token: String, public_url: Option<&str>) -> Auth {
        let host = public_url.and_then(|u| u.split_once("://")).and_then(|(_, rest)| rest.split(['/', ':']).next());
        Auth {
            username,
            password,
            ingest_token,
            public_host: host.filter(|h| !h.is_empty()).map(str::to_ascii_lowercase),
            public_url: public_url.map(str::to_string),
            sessions: Mutex::default(),
            failures: Mutex::default(),
        }
    }

    pub fn public_url(&self) -> Option<&str> {
        self.public_url.as_deref()
    }

    pub fn public_host(&self) -> Option<&str> {
        self.public_host.as_deref()
    }

    pub fn ingest_token(&self) -> &str {
        &self.ingest_token
    }

    pub fn username(&self) -> &str {
        &self.username
    }

    /// Whether the request carries `Authorization: Bearer <ingest token>`.
    pub fn has_token(&self, headers: &HeaderMap) -> bool {
        let given = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer "));
        given.is_some_and(|t| same(t.trim().as_bytes(), self.ingest_token.as_bytes()))
    }

    pub fn signed_in(&self, headers: &HeaderMap) -> bool {
        let Some(id) = session_id(headers) else { return false };
        let mut sessions = self.sessions.lock().unwrap();
        let now = Instant::now();
        sessions.retain(|_, expires| *expires > now);
        sessions.contains_key(id)
    }

    /// `client` is the address the attempt came from (see `api::client_address`).
    pub fn sign_in(&self, client: &str, username: &str, password: &str) -> SignIn {
        let mut failures = self.failures.lock().unwrap();
        let now = Instant::now();
        for times in failures.values_mut() {
            while times.front().is_some_and(|t| now.duration_since(*t) >= FAILURE_WINDOW) {
                times.pop_front();
            }
        }
        failures.retain(|_, times| !times.is_empty());
        if failures.get(client).is_some_and(|times| times.len() >= MAX_FAILURES) {
            return SignIn::Locked;
        }
        // Both are compared in full, so the time taken doesn't tell which one was wrong.
        if !(same(username.as_bytes(), self.username.as_bytes()) & same(password.as_bytes(), self.password.as_bytes())) {
            failures.entry(client.to_string()).or_default().push_back(now);
            return SignIn::Wrong;
        }
        let id = random_hex(32);
        self.sessions.lock().unwrap().insert(id.clone(), now + SESSION_LIFETIME);
        SignIn::Accepted(self.cookie(&id, SESSION_LIFETIME))
    }

    /// Ends the request's session; returns the `Set-Cookie` value that clears it.
    pub fn sign_out(&self, headers: &HeaderMap) -> String {
        if let Some(id) = session_id(headers) {
            self.sessions.lock().unwrap().remove(id);
        }
        self.cookie("", Duration::ZERO)
    }

    fn cookie(&self, value: &str, max_age: Duration) -> String {
        let secure = if self.public_url().is_some_and(|u| u.starts_with("https://")) { "; Secure" } else { "" };
        format!("{COOKIE}={value}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}{secure}", max_age.as_secs())
    }
}

fn session_id(headers: &HeaderMap) -> Option<&str> {
    let cookies = headers.get_all(header::COOKIE).into_iter().filter_map(|v| v.to_str().ok());
    cookies.flat_map(|v| v.split(';')).find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

/// Equal contents, compared without stopping at the first difference.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |diff, (x, y)| diff | (x ^ y)) == 0
}

fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("the operating system provides random numbers");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// The ingest token saved in the data folder, made the first time it is needed.
pub fn load_ingest_token(data: &Path) -> io::Result<String> {
    let path = data.join("ingest-token");
    match fs::read_to_string(&path) {
        Ok(token) if !token.trim().is_empty() => return Ok(token.trim().to_string()),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let token = random_hex(32);
    fs::create_dir_all(data)?;
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(&path)?.write_all(token.as_bytes())?;
    Ok(token)
}
