//! The Rust half of the unit-17 fixture: two loopback hosts that *interpret*
//! `testdata/u17_site.json` — the very file `tools/corpus/u17/server.py` interprets when
//! the golden corpus is generated. Neither server re-declares the site, so the two cannot
//! drift, and `cargo test` needs neither Python nor a network.
//!
//! What the fixture must reproduce exactly, because the corpus compares it:
//!
//! * the **body bytes** of every route (`response_bytes` and `domains_response_bytes` are
//!   byte counts, so a stray newline would show up as a stats mismatch);
//! * the **404 fallback**, which is a real page and not an empty body;
//! * the **flap counters** — `/flaky` answers `503` twice and then `200`, keyed on the
//!   path *including* its query string and reset between scenarios;
//! * the **access log**, which is what proves a `robots.txt` gate fired before the fetch
//!   rather than after it.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde_json::Value;

/// `crates/browser-spider` — the two corpora and the site table live under `testdata/`.
#[must_use]
pub fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(name)
}

// ---------------------------------------------------------------------------
// The declarative site
// ---------------------------------------------------------------------------

/// One route out of `u17_site.json`.
#[derive(Clone, Debug)]
pub struct Route {
    pub status: u16,
    pub content_type: String,
    pub body: String,
    pub headers: Vec<(String, String)>,
    pub delay_ms: u64,
    pub flap: Option<Flap>,
}

/// A route that answers `status` for its first `first_n` hits before behaving normally.
#[derive(Clone, Debug)]
pub struct Flap {
    pub first_n: usize,
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

fn header_pairs(value: Option<&Value>) -> Vec<(String, String)> {
    value
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn parse_routes(entries: &Value) -> HashMap<String, Route> {
    let mut routes = HashMap::new();
    for entry in entries.as_array().expect("a host is a list of routes") {
        let path = entry["path"].as_str().expect("path").to_owned();
        let flap = entry
            .get("flap")
            .filter(|value| !value.is_null())
            .map(|flap| Flap {
                first_n: usize::try_from(flap["first_n"].as_u64().unwrap_or(0)).unwrap_or(0),
                status: u16::try_from(flap["status"].as_u64().unwrap_or(200)).unwrap_or(200),
                headers: header_pairs(flap.get("headers")),
                body: flap
                    .get("body")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            });
        routes.insert(
            path,
            Route {
                status: u16::try_from(entry["status"].as_u64().unwrap_or(200)).unwrap_or(200),
                content_type: entry["content_type"]
                    .as_str()
                    .unwrap_or("text/html")
                    .to_owned(),
                body: entry["body"].as_str().unwrap_or("").to_owned(),
                headers: header_pairs(entry.get("headers")),
                delay_ms: entry["delay_ms"].as_u64().unwrap_or(0),
                flap,
            },
        );
    }
    routes
}

// ---------------------------------------------------------------------------
// One host
// ---------------------------------------------------------------------------

/// The two base URLs every `{{PLACEHOLDER}}` expands to.
#[derive(Clone, Debug, Default)]
pub struct Bases {
    pub primary: String,
    pub secondary: String,
}

impl Bases {
    fn substitute(&self, text: &str) -> String {
        text.replace("{{PRIMARY}}", &self.primary)
            .replace("{{SECONDARY}}", &self.secondary)
    }
}

/// One loopback host, serving one list of routes and logging every request it saw.
pub struct Host {
    routes: HashMap<String, Route>,
    hits: Mutex<HashMap<String, usize>>,
    log: Mutex<Vec<String>>,
    bases: OnceLock<Bases>,
    port: u16,
    running: Arc<AtomicBool>,
}

impl Host {
    fn start(routes: HashMap<String, Route>) -> Arc<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a loopback port");
        let port = listener.local_addr().expect("local addr").port();
        let host = Arc::new(Self {
            routes,
            hits: Mutex::new(HashMap::new()),
            log: Mutex::new(Vec::new()),
            bases: OnceLock::new(),
            port,
            running: Arc::new(AtomicBool::new(true)),
        });

        let accept = Arc::clone(&host);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if !accept.running.load(Ordering::Acquire) {
                    break;
                }
                match stream {
                    Ok(stream) => {
                        let connection = Arc::clone(&accept);
                        std::thread::spawn(move || connection.serve(stream));
                    }
                    Err(_) => break,
                }
            }
        });
        host
    }

    /// The host's `http://127.0.0.1:<port>` base.
    #[must_use]
    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// The host's netloc, which is what `Request::domain` and `allowed_domains` compare.
    #[must_use]
    pub fn netloc(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    /// Every request line the host saw, in arrival order (`"GET /cat/0"`).
    #[must_use]
    pub fn log(&self) -> Vec<String> {
        self.log.lock().expect("not poisoned").clone()
    }

    /// Forget the access log and the flap counters — one scenario's worth of state.
    pub fn reset(&self) {
        self.log.lock().expect("not poisoned").clear();
        self.hits.lock().expect("not poisoned").clear();
    }

    fn bases(&self) -> &Bases {
        self.bases
            .get()
            .expect("bases are set before the first request")
    }

    /// A single connection, kept alive for as many requests as the client sends.
    fn serve(&self, stream: TcpStream) {
        let Ok(write_half) = stream.try_clone() else {
            return;
        };
        let mut writer = write_half;
        let mut reader = BufReader::new(stream);
        loop {
            let mut request_line = String::new();
            match reader.read_line(&mut request_line) {
                Ok(0) => return,
                Ok(_) => {}
                Err(_) => return,
            }
            if request_line.trim().is_empty() {
                return;
            }
            let mut fields = request_line.split_whitespace();
            let method = fields.next().unwrap_or("GET").to_owned();
            let target = fields.next().unwrap_or("/").to_owned();

            let mut content_length = 0_usize;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => return,
                    Ok(_) if line == "\r\n" || line == "\n" => break,
                    Ok(_) => {
                        let lower = line.to_ascii_lowercase();
                        if let Some(rest) = lower.strip_prefix("content-length:") {
                            content_length = rest.trim().parse().unwrap_or(0);
                        }
                    }
                    Err(_) => return,
                }
            }
            if content_length > 0 {
                let mut body = vec![0_u8; content_length];
                if reader.read_exact(&mut body).is_err() {
                    return;
                }
            }

            if !self.respond(&mut writer, &method, &target) {
                return;
            }
        }
    }

    /// `false` means the connection must be closed.
    fn respond(&self, writer: &mut TcpStream, method: &str, target: &str) -> bool {
        // A client never sends the fragment, but the Python fixture strips it anyway and
        // the two must key their counters identically.
        let path = target.split('#').next().unwrap_or(target).to_owned();
        {
            let mut log = self.log.lock().expect("not poisoned");
            log.push(format!("{method} {path}"));
        }
        let hits = {
            let mut hits = self.hits.lock().expect("not poisoned");
            let counter = hits.entry(path.clone()).or_insert(0);
            *counter += 1;
            *counter
        };

        let route_path = path.split('?').next().unwrap_or(&path);
        let Some(route) = self.routes.get(route_path) else {
            // Byte-identical to `server.py`'s fallback: the corpus counts these bytes.
            let body = b"<!DOCTYPE html><html><body><h1>404</h1></body></html>";
            return write_response(writer, 404, "text/html; charset=utf-8", &[], body, method);
        };

        if route.delay_ms > 0 {
            std::thread::sleep(Duration::from_millis(route.delay_ms));
        }

        let (status, headers, body_text) = match &route.flap {
            Some(flap) if hits <= flap.first_n => {
                (flap.status, flap.headers.clone(), flap.body.clone())
            }
            _ => (route.status, route.headers.clone(), route.body.clone()),
        };
        let bases = self.bases();
        let body = bases.substitute(&body_text);
        let headers: Vec<(String, String)> = headers
            .into_iter()
            .map(|(key, value)| (key, bases.substitute(&value)))
            .collect();
        write_response(
            writer,
            status,
            &route.content_type,
            &headers,
            body.as_bytes(),
            method,
        )
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        301 => "Moved Permanently",
        404 => "Not Found",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn write_response(
    writer: &mut TcpStream,
    status: u16,
    content_type: &str,
    headers: &[(String, String)],
    body: &[u8],
    method: &str,
) -> bool {
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n",
        reason(status),
        body.len()
    );
    for (key, value) in headers {
        head.push_str(&format!("{key}: {value}\r\n"));
    }
    head.push_str("Connection: keep-alive\r\n\r\n");
    if writer.write_all(head.as_bytes()).is_err() {
        return false;
    }
    if method != "HEAD" && writer.write_all(body).is_err() {
        return false;
    }
    writer.flush().is_ok()
}

// ---------------------------------------------------------------------------
// The pair
// ---------------------------------------------------------------------------

/// Both hosts, with the cross-references resolved.
pub struct Fixture {
    pub primary: Arc<Host>,
    pub secondary: Arc<Host>,
    bases: Bases,
}

impl Fixture {
    /// Bind two loopback ports and serve `testdata/u17_site.json` on them.
    ///
    /// # Panics
    ///
    /// If the site table is missing or malformed, or a port cannot be bound.
    #[must_use]
    pub fn start() -> Self {
        let raw = std::fs::read_to_string(testdata("u17_site.json")).expect("u17_site.json");
        let site: Value = serde_json::from_str(&raw).expect("u17_site.json parses");
        let primary = Host::start(parse_routes(&site["hosts"]["primary"]));
        let secondary = Host::start(parse_routes(&site["hosts"]["secondary"]));
        let bases = Bases {
            primary: primary.base(),
            secondary: secondary.base(),
        };
        primary.bases.set(bases.clone()).expect("set once");
        secondary.bases.set(bases.clone()).expect("set once");
        Self {
            primary,
            secondary,
            bases,
        }
    }

    /// Clear both access logs and both flap counters.
    pub fn reset(&self) {
        self.primary.reset();
        self.secondary.reset();
    }

    /// Expand `{{PRIMARY}}` / `{{SECONDARY}}` / `{{PRIMARY_HOST}}` / `{{SECONDARY_HOST}}`.
    #[must_use]
    pub fn expand(&self, text: &str) -> String {
        self.bases
            .substitute(text)
            .replace("{{PRIMARY_HOST}}", &self.primary.netloc())
            .replace("{{SECONDARY_HOST}}", &self.secondary.netloc())
    }

    /// The inverse of [`Fixture::expand`], so an observation can be compared with a corpus
    /// row that was recorded on different ports.
    #[must_use]
    pub fn normalise(&self, text: &str) -> String {
        text.replace(&self.bases.primary, "{{PRIMARY}}")
            .replace(&self.bases.secondary, "{{SECONDARY}}")
            .replace(&self.primary.netloc(), "{{PRIMARY_HOST}}")
            .replace(&self.secondary.netloc(), "{{SECONDARY_HOST}}")
    }

    /// [`Fixture::normalise`] applied through a whole JSON value, keys included.
    #[must_use]
    pub fn normalise_value(&self, value: &Value) -> Value {
        match value {
            Value::String(text) => Value::String(self.normalise(text)),
            Value::Array(entries) => Value::Array(
                entries
                    .iter()
                    .map(|entry| self.normalise_value(entry))
                    .collect(),
            ),
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, value)| (self.normalise(key), self.normalise_value(value)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }

    /// Both access logs, normalised, in the shape the corpus stores them.
    #[must_use]
    pub fn access_log(&self) -> Value {
        let map = |host: &Host| {
            Value::Array(
                host.log()
                    .into_iter()
                    .map(|line| Value::String(self.normalise(&line)))
                    .collect(),
            )
        };
        let mut out = serde_json::Map::new();
        out.insert("primary".into(), map(&self.primary));
        out.insert("secondary".into(), map(&self.secondary));
        Value::Object(out)
    }
}
