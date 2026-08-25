//! The Rust twin of `tools/corpus/u14/server.py`.
//!
//! Interprets `testdata/u14_fixture.json` — it re-declares nothing, so the
//! Python server that generated the corpus and this one cannot drift.
//!
//! Raw `tokio` sockets rather than a framework: the fixture has to be able to
//! accept a connection and then *never answer* (the navigation-timeout case),
//! to hang up with zero bytes on the first hit of a path (the retry case), and
//! to serve several virtual hosts off one port.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

/// One endpoint of the declarative fixture table.
#[derive(Clone, Debug, Deserialize)]
pub struct Endpoint {
    /// The path this endpoint answers, without a query string.
    pub path: String,
    /// The HTTP status to send.
    pub status: u16,
    /// The reason phrase to send verbatim.
    pub reason: String,
    /// The `Content-Type` header value.
    pub content_type: String,
    /// Extra header pairs, sent after `Content-Type`/`Content-Length`.
    pub headers: Vec<Vec<String>>,
    /// The body, with `{BASE}` / `{MAIN}` / … placeholders unexpanded.
    pub body: String,
    /// A base64 body, for the binary endpoints. Wins over `body`.
    pub body_b64: Option<String>,
    /// How long to stall before answering.
    pub delay_ms: u64,
    /// `normal`, `hang` or `drop_first`.
    pub mode: String,
}

/// The whole fixture table.
#[derive(Clone, Debug, Deserialize)]
pub struct FixtureTable {
    /// Placeholder name (`MAIN`, `CDN`, …) to bare host name.
    pub hosts: HashMap<String, String>,
    /// Every endpoint, in declaration order.
    pub endpoints: Vec<Endpoint>,
}

/// One request the server saw.
#[derive(Clone, Debug)]
pub struct SeenRequest {
    /// The `Host` header, including the port.
    pub host: String,
    /// The request path, query string stripped.
    pub path: String,
    /// Every request header, lower-cased.
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Default)]
struct Recorder {
    seen: Vec<SeenRequest>,
    counts: HashMap<String, usize>,
}

/// A running fixture server.
#[derive(Debug)]
pub struct Fixture {
    addr: SocketAddr,
    table: Arc<FixtureTable>,
    recorder: Arc<Mutex<Recorder>>,
}

/// Read the fixture table shipped in `testdata/`.
///
/// # Panics
///
/// If the corpus file is missing or malformed — both are bugs in the tree, not
/// conditions a test should tolerate.
pub fn table() -> FixtureTable {
    let raw = include_str!("../../testdata/u14_fixture.json");
    serde_json::from_str(raw).expect("testdata/u14_fixture.json parses")
}

impl Fixture {
    /// Bind a port and start serving the fixture table.
    ///
    /// # Panics
    ///
    /// If the loopback port cannot be bound.
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback port");
        let addr = listener.local_addr().expect("a local address");
        let table = Arc::new(table());
        let recorder = Arc::new(Mutex::new(Recorder::default()));

        let accept_table = Arc::clone(&table);
        let accept_recorder = Arc::clone(&recorder);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let table = Arc::clone(&accept_table);
                let recorder = Arc::clone(&accept_recorder);
                tokio::spawn(async move {
                    let _ = serve(stream, addr, &table, &recorder).await;
                });
            }
        });

        Self {
            addr,
            table,
            recorder,
        }
    }

    /// The port this run happened to get.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// The `host:port` behind a placeholder name (`MAIN`, `CDN`, `ADS`, `API`).
    ///
    /// # Panics
    ///
    /// If the fixture table has no such host.
    pub fn host(&self, key: &str) -> String {
        let name = self
            .table
            .hosts
            .get(key)
            .unwrap_or_else(|| panic!("the fixture declares the host {key}"));
        format!("{name}:{}", self.addr.port())
    }

    /// A URL on one of the fixture hosts.
    pub fn url(&self, path: &str, host_key: &str) -> String {
        format!("http://{}{path}", self.host(host_key))
    }

    /// A URL on the page host.
    pub fn main_url(&self, path: &str) -> String {
        self.url(path, "MAIN")
    }

    /// The `--host-resolver-rules` flag that points every fixture host at
    /// loopback, i.e. the flag `generate.py` used.
    pub fn resolver_rule(&self) -> String {
        let mut names: Vec<&String> = self.table.hosts.values().collect();
        names.sort();
        let rules = names
            .iter()
            .map(|name| format!("MAP {name} 127.0.0.1"))
            .collect::<Vec<_>>()
            .join(",");
        format!("--host-resolver-rules={rules}")
    }

    /// Forget every recorded request.
    pub fn reset(&self) {
        let mut recorder = self.recorder.lock();
        recorder.seen.clear();
        recorder.counts.clear();
    }

    /// `(bare host, path)` for every request, in arrival order.
    pub fn received(&self) -> Vec<(String, String)> {
        self.recorder
            .lock()
            .seen
            .iter()
            .map(|request| {
                (
                    request
                        .host
                        .split(':')
                        .next()
                        .unwrap_or_default()
                        .to_owned(),
                    request.path.clone(),
                )
            })
            .collect()
    }

    /// The same, de-duplicated and sorted — the shape the corpus records.
    ///
    /// The favicon is dropped: Chrome fetches one for every top-level page but
    /// whether it lands before the tab is snapshotted is a race, and the corpus
    /// shows it appearing and disappearing between otherwise identical runs.
    pub fn received_set(&self) -> Vec<(String, String)> {
        let mut pairs: Vec<(String, String)> = self
            .received()
            .into_iter()
            .filter(|(_, path)| path != "/favicon.ico")
            .collect();
        pairs.sort();
        pairs.dedup();
        pairs
    }

    /// Whether a given `(host, path)` ever reached the socket.
    pub fn saw(&self, host: &str, path: &str) -> bool {
        self.received().iter().any(|(h, p)| h == host && p == path)
    }

    /// How many times a path was requested, across all hosts.
    pub fn hits(&self, path: &str) -> usize {
        self.received().iter().filter(|(_, p)| p == path).count()
    }

    /// The last value of `header` seen on a request for `path`.
    pub fn header_of(&self, path: &str, header: &str) -> Option<String> {
        self.recorder
            .lock()
            .seen
            .iter()
            .rev()
            .find(|request| request.path == path)
            .and_then(|request| request.headers.get(header).cloned())
    }
}

async fn serve(
    stream: TcpStream,
    addr: SocketAddr,
    table: &FixtureTable,
    recorder: &Mutex<Recorder>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).await? == 0 {
        return Ok(());
    }
    let raw_path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_owned();
    let path = raw_path.split('?').next().unwrap_or(&raw_path).to_owned();

    let mut headers = HashMap::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim().to_lowercase();
            let value = value.trim().to_owned();
            if name == "content-length" {
                content_length = value.parse().unwrap_or(0);
            }
            headers.insert(name, value);
        }
    }
    if content_length > 0 {
        let mut body = vec![0u8; content_length];
        tokio::io::AsyncReadExt::read_exact(&mut reader, &mut body).await?;
    }

    let host = headers.get("host").cloned().unwrap_or_default();
    let nth = {
        let mut guard = recorder.lock();
        guard.seen.push(SeenRequest {
            host,
            path: path.clone(),
            headers: headers.clone(),
        });
        let count = guard.counts.entry(path.clone()).or_insert(0);
        *count += 1;
        *count
    };

    let Some(endpoint) = table.endpoints.iter().find(|e| e.path == path) else {
        return write_response(
            &mut write_half,
            404,
            "Not Found",
            "text/html; charset=utf-8",
            &[],
            b"<html><body>missing</body></html>",
        )
        .await;
    };

    match endpoint.mode.as_str() {
        // Accept, read, and never answer: Chrome sees a navigation that never
        // commits. `Page.navigate` must be bounded by `timeout` or the request
        // hangs for the whole CDP command budget.
        "hang" => {
            tokio::time::sleep(Duration::from_secs(120)).await;
            return Ok(());
        }
        // Hang up with zero bytes: `net::ERR_EMPTY_RESPONSE`, the class of
        // failure the retry loop exists for.
        "drop_first" if nth == 1 => return Ok(()),
        _ => {}
    }

    if endpoint.delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(endpoint.delay_ms)).await;
    }

    let body: Vec<u8> = if let Some(encoded) = endpoint.body_b64.as_deref() {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("the fixture's base64 bodies decode")
    } else {
        let base = format!("http://{addr}");
        let mut text = endpoint.body.replace("{BASE}", &base);
        for (key, name) in &table.hosts {
            text = text.replace(&format!("{{{key}}}"), &format!("{name}:{}", addr.port()));
        }
        text = text.replace(
            "{PROBE}",
            headers.get("x-probe").map_or("absent", String::as_str),
        );
        text = text.replace(
            "{REFERER}",
            headers.get("referer").map_or("absent", String::as_str),
        );
        text.into_bytes()
    };

    let base = format!("http://{addr}");
    let extra: Vec<(String, String)> = endpoint
        .headers
        .iter()
        .filter_map(|pair| {
            let name = pair.first()?;
            let value = pair.get(1)?;
            Some((name.clone(), value.replace("{BASE}", &base)))
        })
        .collect();

    write_response(
        &mut write_half,
        endpoint.status,
        &endpoint.reason,
        &endpoint.content_type,
        &extra,
        &body,
    )
    .await
}

async fn write_response<W>(
    writer: &mut W,
    status: u16,
    reason: &str,
    content_type: &str,
    extra: &[(String, String)],
    body: &[u8],
) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    for (name, value) in extra {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes()).await?;
    writer.write_all(body).await?;
    writer.flush().await
}
