//! A deliberately small HTTP/1.1 server for the Hub's endpoints
//! (ADR-0004): loopback only, one request per connection, `Content-Length`
//! bodies only, hard limits on every size and a total deadline per request.
//!
//! Every request is checked before it reaches a handler:
//! * `Host` must name the loopback listener (`127.0.0.1:<port>` or
//!   `localhost:<port>`), and any `Origin` header is refused: together they
//!   defeat DNS rebinding from a browser (MCP transport security);
//! * everything except the public agent card needs `Authorization: Bearer`
//!   with a token [`Hub::authenticate`] accepts.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::json;

use crate::Hub;
use crate::model::Caller;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub workers: usize,
    /// Pending connections beyond the busy workers; more get `503`.
    pub backlog: usize,
    pub max_header_bytes: usize,
    pub max_body_bytes: usize,
    /// Total time allowed to receive one request (slow clients are cut
    /// off; a worker is never held longer than this plus the handler).
    pub request_deadline: Duration,
    pub write_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        ServerConfig {
            workers: 8,
            backlog: 32,
            max_header_bytes: 16 << 10,
            max_body_bytes: 1 << 20,
            request_deadline: Duration::from_secs(10),
            write_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug)]
pub struct Request {
    pub method: String,
    pub path: String,
    /// Header names lower-cased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let mut it = self.headers.iter().filter(|(n, _)| n == name);
        let first = it.next().map(|(_, v)| v.as_str());
        // A repeated header is ambiguous; treat it as absent.
        if it.next().is_some() { None } else { first }
    }

    fn has_header(&self, name: &str) -> bool {
        self.headers.iter().any(|(n, _)| n == name)
    }
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub content_type: &'static str,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, v: &serde_json::Value) -> Response {
        Response {
            status,
            content_type: "application/json",
            headers: Vec::new(),
            body: v.to_string().into_bytes(),
        }
    }

    pub fn empty(status: u16) -> Response {
        Response {
            status,
            content_type: "text/plain",
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    pub fn error(status: u16, msg: &str) -> Response {
        Response::json(status, &json!({ "error": msg }))
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        409 => "Conflict",
        202 => "Accepted",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "Status",
    }
}

fn write_response(stream: &mut TcpStream, r: &Response) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\n",
        r.status,
        reason(r.status),
        r.content_type,
        r.body.len()
    );
    for (n, v) in &r.headers {
        head.push_str(&format!("{n}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(&r.body)?;
    stream.flush()
}

/// Read and parse one request within the deadline. `Err` carries the
/// response to send instead.
fn read_request(stream: &mut TcpStream, cfg: &ServerConfig) -> Result<Request, Response> {
    let deadline = Instant::now() + cfg.request_deadline;
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let timeout = || Response::error(408, "request timeout");
    let mut read_some = |stream: &mut TcpStream, buf: &mut Vec<u8>| -> Result<usize, Response> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(timeout());
        }
        stream
            .set_read_timeout(Some(left))
            .map_err(|_| Response::error(500, "socket"))?;
        match stream.read(&mut chunk) {
            Ok(0) => Err(Response::error(400, "connection closed")),
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                Ok(n)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Err(timeout())
            }
            Err(_) => Err(Response::error(400, "read error")),
        }
    };
    let head_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > cfg.max_header_bytes {
            return Err(Response::error(431, "headers too large"));
        }
        read_some(stream, &mut buf)?;
    };
    if head_end > cfg.max_header_bytes {
        return Err(Response::error(431, "headers too large"));
    }
    let head = std::str::from_utf8(&buf[..head_end])
        .map_err(|_| Response::error(400, "headers are not UTF-8"))?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(Response::error(400, "malformed request line"));
    };
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(Response::error(400, "unsupported HTTP version"));
    }
    let mut headers = Vec::new();
    for line in lines {
        let Some((n, v)) = line.split_once(':') else {
            return Err(Response::error(400, "malformed header"));
        };
        if n.is_empty() || n.contains(|c: char| c.is_whitespace() || c.is_control()) {
            return Err(Response::error(400, "malformed header name"));
        }
        if v.contains(|c: char| c.is_control() && c != '\t') {
            return Err(Response::error(400, "malformed header value"));
        }
        headers.push((n.to_ascii_lowercase(), v.trim().to_string()));
    }
    let mut req = Request {
        method: method.to_string(),
        path: target.to_string(),
        headers,
        body: Vec::new(),
    };
    if req.has_header("transfer-encoding") {
        return Err(Response::error(
            411,
            "only Content-Length bodies are accepted",
        ));
    }
    let len = match (
        req.has_header("content-length"),
        req.header("content-length"),
    ) {
        (false, _) => 0,
        (true, None) => return Err(Response::error(400, "repeated Content-Length")),
        (true, Some(v)) => v
            .parse::<usize>()
            .map_err(|_| Response::error(400, "bad Content-Length"))?,
    };
    if len > cfg.max_body_bytes {
        return Err(Response::error(413, "body too large"));
    }
    if req.method == "POST" && !req.has_header("content-length") {
        return Err(Response::error(411, "Content-Length required"));
    }
    let mut body = buf.split_off(head_end + 4);
    while body.len() < len {
        read_some(stream, &mut body)?;
    }
    if body.len() > len {
        return Err(Response::error(400, "unexpected bytes after the body"));
    }
    req.body = body;
    Ok(req)
}

/// Everything after the transport: the shared core reads one bounded
/// request, hands it to the service, and writes its response. The Hub's
/// MCP/A2A routes are one service; the dashboard (`anchor ui`) is another,
/// with its own Host/Origin/auth policy.
pub trait Service: Send + Sync {
    fn handle(&self, req: &Request) -> Response;
}

/// The authenticated handler for one endpoint.
pub type Handler = dyn Fn(&Hub, Caller, &Request) -> Response + Send + Sync;

struct Routes {
    hub: Arc<Hub>,
    port: u16,
    card: Box<dyn Fn(u16) -> serde_json::Value + Send + Sync>,
    endpoints: Vec<(&'static str, Box<Handler>)>,
}

impl Service for Routes {
    fn handle(&self, req: &Request) -> Response {
        let host_ok = req.header("host").is_some_and(|h| {
            h == format!("127.0.0.1:{}", self.port) || h == format!("localhost:{}", self.port)
        });
        if !host_ok {
            return Response::error(403, "unexpected Host");
        }
        if req.has_header("origin") {
            return Response::error(403, "cross-origin requests are refused");
        }
        if req.path == "/.well-known/agent-card.json" {
            return match req.method.as_str() {
                "GET" => Response::json(200, &(self.card)(self.port)),
                _ => Response::error(405, "method not allowed"),
            };
        }
        let Some((_, handler)) = self.endpoints.iter().find(|(p, _)| *p == req.path) else {
            return Response::error(404, "not found");
        };
        if req.method != "POST" {
            return Response::error(405, "method not allowed");
        }
        let token = req
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(str::trim);
        let caller = match token.map(|t| self.hub.authenticate(t)) {
            Some(Ok(c)) => c,
            _ => {
                self.hub.audit_event(
                    "hub_unauthenticated",
                    json!({ "path": req.path, "token_presented": token.is_some() }),
                );
                let mut r = Response::error(401, "a valid bearer token is required");
                r.headers.push(("WWW-Authenticate", "Bearer".into()));
                return r;
            }
        };
        handler(&self.hub, caller, req)
    }
}

/// A running server; dropping it (or [`ServerHandle::stop`]) stops it.
#[derive(Debug)]
pub struct ServerHandle {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl ServerHandle {
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = TcpStream::connect(self.addr);
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }

    /// Block until the server stops (for `anchor hub serve`).
    pub fn wait(mut self) {
        if let Some(h) = self.accept.take() {
            let _ = h.join();
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if self.accept.is_some() {
            self.shutdown();
        }
    }
}

/// Bind `127.0.0.1:port` (0 = any free port) and serve the MCP and A2A
/// endpoints on worker threads.
pub fn serve(hub: Arc<Hub>, port: u16, cfg: ServerConfig) -> std::io::Result<ServerHandle> {
    let listener = TcpListener::bind(("127.0.0.1", port))?;
    let addr = listener.local_addr()?;
    let routes = Arc::new(Routes {
        hub,
        port: addr.port(),
        card: Box::new(crate::a2a::agent_card),
        endpoints: vec![
            ("/mcp", Box::new(crate::mcp::handle) as Box<Handler>),
            ("/a2a", Box::new(crate::a2a::handle) as Box<Handler>),
        ],
    });
    serve_on(listener, routes, cfg)
}

/// Serve `service` on an already bound loopback listener, with the shared
/// limits (size, deadline, worker pool, backlog).
pub fn serve_on(
    listener: TcpListener,
    routes: Arc<dyn Service>,
    cfg: ServerConfig,
) -> std::io::Result<ServerHandle> {
    let addr = listener.local_addr()?;
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::other(
            "refusing to serve on a non-loopback address",
        ));
    }
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = sync_channel::<TcpStream>(cfg.backlog);
    let rx = Arc::new(Mutex::new(rx));
    for _ in 0..cfg.workers.max(1) {
        let (rx, routes, cfg) = (rx.clone(), routes.clone(), cfg.clone());
        std::thread::spawn(move || {
            loop {
                let next = rx.lock().unwrap_or_else(|p| p.into_inner()).recv();
                let Ok(mut stream) = next else { return };
                let _ = stream.set_write_timeout(Some(cfg.write_timeout));
                let resp = match read_request(&mut stream, &cfg) {
                    Ok(req) => routes.handle(&req),
                    Err(r) => r,
                };
                let _ = write_response(&mut stream, &resp);
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        });
    }
    let stop2 = stop.clone();
    let accept = std::thread::spawn(move || accept_loop(listener, tx, stop2));
    Ok(ServerHandle {
        addr,
        stop,
        accept: Some(accept),
    })
}

fn accept_loop(listener: TcpListener, tx: SyncSender<TcpStream>, stop: Arc<AtomicBool>) {
    for conn in listener.incoming() {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = conn else { continue };
        if let Err(TrySendError::Full(mut s)) = tx.try_send(stream) {
            let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
            let _ = write_response(&mut s, &Response::error(503, "busy"));
        }
    }
}
