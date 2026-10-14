//! Plain HTTP/1.1, one request per connection: the server that shows the app's page and answers
//! other laptops, and the client that asks them.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use serde_json::Value;

use crate::util::url_decode;

pub struct Req {
    pub method: String,
    pub path: String,
    /// The path and query exactly as sent, which is what requests are signed over.
    pub target: String,
    pub query: HashMap<String, String>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub remote: SocketAddr,
}

impl Req {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn q(&self, name: &str) -> &str {
        self.query.get(name).map(|s| s.as_str()).unwrap_or("")
    }
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
}

pub enum Resp {
    Json(u16, Value),
    Bytes(u16, &'static str, Vec<u8>),
    File(PathBuf),
}

impl Resp {
    pub fn ok(v: Value) -> Resp {
        Resp::Json(200, v)
    }
    pub fn err(status: u16, msg: impl Into<String>) -> Resp {
        Resp::Json(status, serde_json::json!({"error": msg.into()}))
    }
}

pub fn serve(listener: TcpListener, handler: fn(&Req) -> Resp) {
    for conn in listener.incoming() {
        let Ok(s) = conn else { continue };
        thread::spawn(move || {
            let _ = handle(s, handler);
        });
    }
}

fn reason(code: u16) -> &'static str {
    match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        _ => "Error",
    }
}

fn handle(s: TcpStream, handler: fn(&Req) -> Resp) -> io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let _ = s.set_nodelay(true);
    let remote = s.peer_addr()?;
    let mut r = BufReader::new(s.try_clone()?);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    let mut total = 0usize;
    loop {
        line.clear();
        let n = r.read_line(&mut line)?;
        total += n;
        if n == 0 || total > 64 * 1024 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let len: usize = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    if len > 16 << 20 {
        return Ok(());
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
    let query = qs
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (url_decode(k), url_decode(v))
        })
        .collect();
    let req = Req { method, path: url_decode(path), target: target.clone(), query, headers, body, remote };
    let resp = handler(&req);
    let mut w = BufWriter::with_capacity(256 * 1024, s);
    let head = |code: u16, ctype: &str, len: u64| {
        format!(
            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
            code,
            reason(code),
            ctype,
            len
        )
    };
    match resp {
        Resp::Json(code, v) => {
            let b = serde_json::to_vec(&v).unwrap();
            w.write_all(head(code, "application/json; charset=utf-8", b.len() as u64).as_bytes())?;
            w.write_all(&b)?;
        }
        Resp::Bytes(code, ctype, b) => {
            w.write_all(head(code, ctype, b.len() as u64).as_bytes())?;
            w.write_all(&b)?;
        }
        Resp::File(p) => match File::open(&p) {
            Ok(mut f) => {
                let len = f.metadata()?.len();
                w.write_all(head(200, "application/octet-stream", len).as_bytes())?;
                io::copy(&mut (&mut f).take(len), &mut w)?;
            }
            Err(_) => {
                let b = br#"{"error":"gone"}"#;
                w.write_all(head(404, "application/json", b.len() as u64).as_bytes())?;
                w.write_all(b)?;
            }
        },
    }
    w.flush()
}

pub struct Reply {
    pub status: u16,
    pub len: Option<u64>,
    pub body: BufReader<TcpStream>,
}

impl Reply {
    pub fn bytes(mut self) -> io::Result<Vec<u8>> {
        let mut v = Vec::new();
        match self.len {
            Some(n) => {
                (&mut self.body).take(n).read_to_end(&mut v)?;
            }
            None => {
                self.body.read_to_end(&mut v)?;
            }
        }
        Ok(v)
    }
    pub fn json(self) -> io::Result<Value> {
        let b = self.bytes()?;
        serde_json::from_slice(&b).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

pub fn request(method: &str, addr: SocketAddr, target: &str, headers: &[(&str, String)], body: &[u8], timeout: Duration) -> io::Result<Reply> {
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    s.set_read_timeout(Some(timeout))?;
    s.set_write_timeout(Some(timeout))?;
    let _ = s.set_nodelay(true);
    let mut req = format!("{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n", method, target, addr);
    for (k, v) in headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    let mut w = &s;
    w.write_all(req.as_bytes())?;
    w.write_all(body)?;
    let mut r = BufReader::with_capacity(256 * 1024, s);
    let mut line = String::new();
    r.read_line(&mut line)?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|x| x.parse().ok())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "not HTTP"))?;
    let mut len = None;
    loop {
        line.clear();
        if r.read_line(&mut line)? == 0 {
            break;
        }
        let l = line.trim_end();
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                len = v.trim().parse().ok();
            }
        }
    }
    Ok(Reply { status, len, body: r })
}
