//! A loopback HTTP fixture, lifted from
//! `crates/browser-rs/tests/integration.rs::local_site`.
//!
//! `wiremock` is declared in `[workspace.dependencies]` but is **not** in
//! `Cargo.lock`, and crates.io is unreachable here, so every test server in this
//! workspace is hand-rolled `std::net`. This one is the fifth copy and it is
//! deliberately the *same* copy: a `fn(&str) -> Option<Page>` route table, one
//! thread per connection, and a half-close (`Shutdown::Write` plus a drain to
//! EOF) rather than an RST, so the client is never reset before it has read the
//! body.
//!
//! The open internet is not reachable at `cargo test` time, so nothing in this
//! crate's test suite touches a real host.

#![allow(
    dead_code,
    reason = "each integration test file uses a different subset"
)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One reply.
pub struct Page {
    /// HTTP status.
    pub status: u16,
    /// The `Content-Type` header, charset included.
    pub content_type: &'static str,
    /// The body.
    pub body: String,
}

impl Page {
    /// A `200 text/html; charset=utf-8` reply.
    pub fn html(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
        }
    }

    /// A `200 text/plain; charset=utf-8` reply.
    pub fn text(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/plain; charset=utf-8",
            body: body.into(),
        }
    }
}

/// The route table: a path in, a reply (or `None` for a 404) out.
pub type Route = fn(&str) -> Option<Page>;

/// One request as the socket saw it.
#[derive(Clone, Debug)]
pub struct Hit {
    /// The request line's method.
    pub method: String,
    /// The request line's target, query string included.
    pub path: String,
    /// Every header, lowercased name to value.
    pub headers: Vec<(String, String)>,
    /// The request body, if any.
    pub body: String,
}

impl Hit {
    /// The first value of `name`, matched case-insensitively.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// A running loopback server, stopped when it is dropped.
pub struct Site {
    port: u16,
    hits: Arc<Mutex<Vec<Hit>>>,
    running: Arc<AtomicBool>,
}

impl Site {
    /// Bind an ephemeral port and start serving `route`.
    pub fn start(route: Route) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let running = Arc::new(AtomicBool::new(true));

        let thread_hits = Arc::clone(&hits);
        let thread_running = Arc::clone(&running);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if !thread_running.load(Ordering::Acquire) {
                    break;
                }
                match stream {
                    Ok(stream) => {
                        let hits = Arc::clone(&thread_hits);
                        std::thread::spawn(move || serve(stream, route, &hits));
                    }
                    Err(_) => break,
                }
            }
        });

        Self {
            port,
            hits,
            running,
        }
    }

    /// An absolute URL for `path`.
    pub fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }

    /// Every request the socket saw, in arrival order.
    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().expect("not poisoned").clone()
    }

    /// Every path the socket saw, in arrival order.
    pub fn paths(&self) -> Vec<String> {
        self.hits().into_iter().map(|hit| hit.path).collect()
    }

    /// The most recent request, panicking if there was none.
    pub fn last(&self) -> Hit {
        self.hits().pop().expect("at least one request")
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(mut stream: TcpStream, route: Route, hits: &Arc<Mutex<Vec<Hit>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
        return;
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {
                if let Some((name, value)) = line.trim_end().split_once(':') {
                    headers.push((name.trim().to_lowercase(), value.trim().to_owned()));
                }
            }
            Err(_) => return,
        }
    }

    // Read exactly `Content-Length` bytes, so a POST body is observable.
    let length: usize = headers
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0_u8; length];
    if length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }

    let mut fields = request_line.split_whitespace();
    let method = fields.next().unwrap_or("GET").to_owned();
    let path = fields.next().unwrap_or("/").to_owned();

    hits.lock().expect("not poisoned").push(Hit {
        method,
        path: path.clone(),
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    });

    // The query string is not part of the route key; `?a=1` still lands on the
    // page, which is what `--params` needs.
    let key = path.split('?').next().unwrap_or("/");
    let page = route(key).unwrap_or(Page {
        status: 404,
        content_type: "text/plain; charset=utf-8",
        body: "not found".to_owned(),
    });
    let reason = if page.status == 200 {
        "OK"
    } else {
        "Not Found"
    };
    let head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        page.status,
        page.content_type,
        page.body.len(),
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(page.body.as_bytes());
    let _ = stream.flush();
    let _ = stream.shutdown(Shutdown::Write);
    let mut sink = Vec::new();
    let _ = reader.read_to_end(&mut sink);
}
