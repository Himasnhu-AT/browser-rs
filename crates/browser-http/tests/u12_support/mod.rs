//! The Rust half of the unit-12 differential fixture.
//!
//! It *interprets* `testdata/u12_endpoints.json` — the very table the Python
//! fixture server in `tools/corpus/u12/server.py` interprets — so the two
//! servers cannot drift apart. Raw sockets on purpose: the corpus compares
//! response headers, so the server must put exactly the declared bytes on the
//! wire and nothing else (no `Date`, no `Server`, no reason-phrase rewriting,
//! duplicate and oddly-cased header names preserved).
//!
//! Nothing here touches the network beyond `127.0.0.1`, and nothing here needs
//! Python.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

/// The header names `mode: "echo"` reports back, mirroring
/// `endpoints.ECHO_HEADER_WHITELIST`.
pub const ECHO_HEADER_WHITELIST: [&str; 9] = [
    "cookie",
    "x-caller",
    "x-session",
    "x-override",
    "authorization",
    "referer",
    "content-type",
    "content-length",
    "accept-language",
];

/// One fixture server: a table, a port, and a little mutable state.
pub struct Fixture {
    table: Arc<Value>,
    pub port: u16,
    state: Arc<State>,
    running: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    flap: Mutex<HashMap<String, u32>>,
    hits: Mutex<Vec<String>>,
    ports: Mutex<(u16, u16)>,
}

/// Start the pair of fixtures (`A` and `B`) the corpus was recorded against.
///
/// # Panics
///
/// If a loopback port cannot be bound.
pub fn start_pair(table: Value) -> (Fixture, Fixture) {
    let table = Arc::new(table);
    let listener_a = TcpListener::bind("127.0.0.1:0").expect("bind A");
    let listener_b = TcpListener::bind("127.0.0.1:0").expect("bind B");
    let port_a = listener_a.local_addr().expect("addr A").port();
    let port_b = listener_b.local_addr().expect("addr B").port();
    let a = Fixture::spawn(Arc::clone(&table), listener_a, port_a, port_a, port_b);
    let b = Fixture::spawn(table, listener_b, port_b, port_a, port_b);
    (a, b)
}

impl Fixture {
    fn spawn(
        table: Arc<Value>,
        listener: TcpListener,
        port: u16,
        port_a: u16,
        port_b: u16,
    ) -> Self {
        let state = Arc::new(State::default());
        *state.ports.lock().expect("ports") = (port_a, port_b);
        let running = Arc::new(AtomicBool::new(true));
        let fixture = Self {
            table: Arc::clone(&table),
            port,
            state: Arc::clone(&state),
            running: Arc::clone(&running),
        };
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if !running.load(Ordering::Acquire) {
                    return;
                }
                let Ok(stream) = stream else { continue };
                let table = Arc::clone(&table);
                let state = Arc::clone(&state);
                std::thread::spawn(move || {
                    let _ = serve(&table, &state, stream);
                });
            }
        });
        fixture
    }

    /// Zero the flap counters and the hit log — the Rust twin of `/__reset`.
    pub fn reset(&self) {
        self.state.flap.lock().expect("flap").clear();
        self.state.hits.lock().expect("hits").clear();
    }

    /// Every request line this fixture has seen since the last reset.
    pub fn hits(&self) -> Vec<String> {
        self.state.hits.lock().expect("hits").clone()
    }

    /// Stop accepting.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
    }
}

fn subst(text: &str, port_a: u16, port_b: u16) -> String {
    text.replace("{A}", &format!("http://127.0.0.1:{port_a}"))
        .replace("{B}", &format!("http://127.0.0.1:{port_b}"))
        .replace("{HOSTA}", &format!("127.0.0.1:{port_a}"))
        .replace("{HOSTB}", &format!("127.0.0.1:{port_b}"))
}

fn serve(table: &Value, state: &State, stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let line = line.trim_end().to_owned();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            return Ok(());
        };
        let (method, target) = (method.to_owned(), target.to_owned());

        let mut headers: Vec<(String, String)> = Vec::new();
        loop {
            let mut raw = String::new();
            if reader.read_line(&mut raw)? == 0 {
                break;
            }
            if raw == "\r\n" || raw == "\n" {
                break;
            }
            if (raw.starts_with(' ') || raw.starts_with('\t')) && !headers.is_empty() {
                let last = headers.len() - 1;
                headers[last].1.push_str(raw.trim_end());
                continue;
            }
            let (name, value) = raw.split_once(':').unwrap_or((raw.as_str(), ""));
            headers.push((name.trim().to_owned(), value.trim().to_owned()));
        }
        let lookup: HashMap<String, String> = headers
            .iter()
            .map(|(name, value)| (name.to_lowercase(), value.clone()))
            .collect();

        let mut body = Vec::new();
        if let Some(length) = lookup.get("content-length") {
            let length: usize = length.parse().unwrap_or(0);
            body.resize(length, 0);
            reader.read_exact(&mut body)?;
        } else if lookup
            .get("transfer-encoding")
            .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
        {
            loop {
                let mut size_line = String::new();
                reader.read_line(&mut size_line)?;
                let size = usize::from_str_radix(
                    size_line.trim().split(';').next().unwrap_or("0").trim(),
                    16,
                )
                .unwrap_or(0);
                if size == 0 {
                    let mut trailer = String::new();
                    reader.read_line(&mut trailer)?;
                    break;
                }
                let mut chunk = vec![0u8; size];
                reader.read_exact(&mut chunk)?;
                body.extend_from_slice(&chunk);
                let mut crlf = String::new();
                reader.read_line(&mut crlf)?;
            }
        }

        if !respond(table, state, &mut out, &method, &target, &lookup, &body)? {
            return Ok(());
        }
    }
}

#[allow(clippy::too_many_lines)]
fn respond(
    table: &Value,
    state: &State,
    out: &mut TcpStream,
    method: &str,
    target: &str,
    headers: &HashMap<String, String>,
    body: &[u8],
) -> std::io::Result<bool> {
    let (port_a, port_b) = *state.ports.lock().expect("ports");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    state
        .hits
        .lock()
        .expect("hits")
        .push(format!("{method} {target}"));

    if path == "/__reset" {
        state.flap.lock().expect("flap").clear();
        state.hits.lock().expect("hits").clear();
        out.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")?;
        return Ok(true);
    }

    let Some(spec) = table.get(path) else {
        let payload = b"<html><body>no fixture</body></html>";
        out.write_all(
            format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
                payload.len()
            )
            .as_bytes(),
        )?;
        out.write_all(payload)?;
        return Ok(true);
    };

    let status = spec["status"].as_u64().unwrap_or(200);
    let reason = spec["reason"].as_str().unwrap_or("");
    let close = spec["close"].as_bool().unwrap_or(false);
    let mut mode = spec["mode"].as_str().unwrap_or("normal").to_owned();

    if mode == "flap" {
        let seen = {
            let mut flap = state.flap.lock().expect("flap");
            let counter = flap.entry(path.to_owned()).or_insert(0);
            let seen = *counter;
            *counter += 1;
            seen
        };
        if seen < spec["flap_fail"].as_u64().unwrap_or(0) as u32 {
            // Slam the connection shut without a byte of reply: a transport
            // failure, which is what `retries=` is about. `SO_LINGER 0` (a hard
            // RST, which is what the Python fixture sends) is still unstable in
            // std, so this is an immediate FIN instead; both leave the client
            // with a connection that died before a single response byte.
            let _ = out.shutdown(Shutdown::Both);
            return Ok(false);
        }
        mode = "normal".to_owned();
    }

    if mode == "slow" {
        std::thread::sleep(Duration::from_millis(
            spec["delay_ms"].as_u64().unwrap_or(0),
        ));
        mode = "normal".to_owned();
    }

    let declared: Vec<(String, String)> = spec["headers"]
        .as_array()
        .map(|pairs| {
            pairs
                .iter()
                .filter_map(|pair| {
                    let name = pair.get(0)?.as_str()?.to_owned();
                    let value = subst(pair.get(1)?.as_str()?, port_a, port_b);
                    Some((name, value))
                })
                .collect()
        })
        .unwrap_or_default();

    if mode == "echo" {
        // `json.dumps(..., sort_keys=True, separators=(",", ":"))` byte for
        // byte: the corpus compares the echo body and its `Content-Length`.
        let mut echoed: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
        for name in ECHO_HEADER_WHITELIST {
            if let Some(value) = headers.get(name) {
                echoed.insert(name, value.as_str());
            }
        }
        let echoed: Vec<String> = echoed
            .into_iter()
            .map(|(name, value)| format!("{}:{}", json_string(name), json_string(value)))
            .collect();
        let payload = format!(
            "{{\"body\":{},\"headers\":{{{}}},\"method\":{},\"path\":{},\"query\":{}}}",
            json_string(&String::from_utf8_lossy(body)),
            echoed.join(","),
            json_string(method),
            json_string(path),
            json_string(query),
        )
        .into_bytes();
        let mut head = format!("HTTP/1.1 {status} {reason}");
        for (name, value) in &declared {
            head.push_str(&format!("\r\n{name}: {value}"));
        }
        head.push_str(&format!("\r\nContent-Length: {}", payload.len()));
        if close {
            head.push_str("\r\nConnection: close");
        }
        head.push_str("\r\n\r\n");
        out.write_all(head.as_bytes())?;
        out.write_all(&payload)?;
        return Ok(!close);
    }

    let raw = decode_hex(spec["body_hex"].as_str().unwrap_or(""));
    let raw = if raw.windows(3).any(|w| w == b"{A}" || w == b"{B}") {
        subst(&String::from_utf8_lossy(&raw), port_a, port_b).into_bytes()
    } else {
        raw
    };

    let mut head = format!("HTTP/1.1 {status} {reason}");
    for (name, value) in &declared {
        head.push_str(&format!("\r\n{name}: {value}"));
    }
    match mode.as_str() {
        "chunked" => head.push_str("\r\nTransfer-Encoding: chunked"),
        "nolength" => {}
        _ => head.push_str(&format!("\r\nContent-Length: {}", raw.len())),
    }
    if close {
        head.push_str("\r\nConnection: close");
    }
    head.push_str("\r\n\r\n");
    out.write_all(head.as_bytes())?;

    if mode == "chunked" {
        let count = spec["chunks"].as_u64().unwrap_or(1).max(1) as usize;
        for piece in split(&raw, count) {
            out.write_all(format!("{:X}\r\n", piece.len()).as_bytes())?;
            out.write_all(piece)?;
            out.write_all(b"\r\n")?;
        }
        out.write_all(b"0\r\n\r\n")?;
    } else {
        out.write_all(&raw)?;
    }
    out.flush()?;
    if close {
        let _ = out.shutdown(Shutdown::Write);
    }
    Ok(!close)
}

/// Serialise one JSON string the way `json.dumps` does for ASCII input.
fn json_string(text: &str) -> String {
    Value::String(text.to_owned()).to_string()
}

fn split(data: &[u8], count: usize) -> Vec<&[u8]> {
    if data.is_empty() {
        return Vec::new();
    }
    if count <= 1 {
        return vec![data];
    }
    let size = data.len().div_ceil(count).max(1);
    data.chunks(size).collect()
}

/// Decode the lowercase hex the corpus stores bodies as.
pub fn decode_hex(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut index = 0;
    while index + 1 < bytes.len() {
        let hi = (bytes[index] as char).to_digit(16).unwrap_or(0);
        let lo = (bytes[index + 1] as char).to_digit(16).unwrap_or(0);
        out.push(((hi << 4) | lo) as u8);
        index += 2;
    }
    out
}

/// Encode bytes the way the corpus does, so a mismatch prints comparably.
pub fn encode_hex(data: &[u8]) -> String {
    use std::fmt::Write as _;
    data.iter().fold(String::new(), |mut acc, byte| {
        let _ = write!(acc, "{byte:02x}");
        acc
    })
}
