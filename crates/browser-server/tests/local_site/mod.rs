//! A loopback fixture site, copied from `crates/browser-rs/tests/integration.rs`.
//!
//! Two things are added, and nothing is removed:
//!
//! 1. a route is handed an [`Incoming`] rather than a bare path, so it can see
//!    the request's `Cookie` header;
//! 2. a [`Page`] can carry extra response headers, so it can send `Set-Cookie`.
//!
//! Both exist for one test — the one that proves a named session carries a
//! cookie from one request to the next — and a fixture that cannot set or read
//! a cookie cannot prove it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// What a route gets to see of the request.
pub struct Incoming {
    /// The path, query string stripped.
    pub path: String,
    /// The `Cookie` header, verbatim, when one was sent.
    pub cookie: Option<String>,
}

/// One reply.
pub struct Page {
    /// HTTP status.
    pub status: u16,
    /// The `Content-Type` header, charset included.
    pub content_type: &'static str,
    /// The body.
    pub body: String,
    /// Extra response headers, in order.
    pub headers: Vec<(&'static str, String)>,
}

impl Page {
    /// A `200 text/html; charset=utf-8` reply.
    pub fn html(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.into(),
            headers: Vec::new(),
        }
    }

    /// Add one response header.
    #[must_use]
    pub fn with_header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
}

/// The route table: a request in, a reply (or `None` for a 404) out.
pub type Route = fn(&Incoming) -> Option<Page>;

/// A running loopback server, stopped when it is dropped.
pub struct Site {
    port: u16,
    hits: Arc<Mutex<Vec<String>>>,
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
                        // One thread per connection: a browser opens several at
                        // once and keeps them alive.
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

    /// Every path the socket saw, in arrival order.
    pub fn hits(&self) -> Vec<String> {
        self.hits.lock().expect("not poisoned").clone()
    }
}

impl Drop for Site {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
    }
}

fn serve(mut stream: TcpStream, route: Route, hits: &Arc<Mutex<Vec<String>>>) {
    let mut reader = BufReader::new(stream.try_clone().expect("clone"));
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() || request_line.trim().is_empty() {
        return;
    }

    let mut cookie = None;
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) if line == "\r\n" || line == "\n" => break,
            Ok(_) => {
                if let Some((name, value)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("cookie") {
                        cookie = Some(value.trim().to_owned());
                    }
                }
            }
            Err(_) => return,
        }
    }

    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_owned();
    hits.lock().expect("not poisoned").push(path.clone());

    // The query string is not part of the route key.
    let key = path.split('?').next().unwrap_or("/").to_owned();
    let page = route(&Incoming { path: key, cookie }).unwrap_or(Page {
        status: 404,
        content_type: "text/plain; charset=utf-8",
        body: "not found".to_owned(),
        headers: Vec::new(),
    });
    let reason = if page.status == 200 {
        "OK"
    } else {
        "Not Found"
    };
    let mut head = format!(
        "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\n\
         Connection: close\r\n",
        page.status,
        page.content_type,
        page.body.len(),
    );
    for (name, value) in &page.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");

    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(page.body.as_bytes());
    let _ = stream.flush();
    // Half-close and drain, so the peer is never RST before it has read.
    let _ = stream.shutdown(Shutdown::Write);
    let mut sink = Vec::new();
    let _ = reader.read_to_end(&mut sink);
}
