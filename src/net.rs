//! Plain HTTP/1.1, one request per connection, and finding the other laptops: every two seconds
//! each says who it is and which folders it shares, by UDP broadcast on every network it is on, and
//! answers at once when a new one asks. Laptops that cannot hear broadcasts can be added by address.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::store::app;
use crate::{log, url_decode, PORT};

pub const APP_TAG: &str = "foldersync/3";

pub struct Req {
    pub method: String,
    pub path: String,
    /// The path and query exactly as sent, which is what requests are signed over.
    pub target: String,
    pub query: HashMap<String, String>,
    pub headers: HashMap<String, String>,
    pub body: Value,
    pub remote: SocketAddr,
}

impl Req {
    pub fn header(&self, name: &str) -> &str {
        self.headers.get(&name.to_ascii_lowercase()).map_or("", |s| s.as_str())
    }
    pub fn q(&self, name: &str) -> &str {
        self.query.get(name).map_or("", |s| s.as_str())
    }
}

pub enum Resp {
    Json(u16, Value),
    Page(&'static str),
    File(PathBuf),
}

impl Resp {
    pub fn err(status: u16, msg: impl Into<String>) -> Resp {
        Resp::Json(status, json!({"error": msg.into()}))
    }
}

/// Reads a request's or reply's head: the first line and the headers, lower-cased.
fn read_head(r: &mut impl BufRead) -> io::Result<(String, HashMap<String, String>)> {
    let (mut first, mut line, mut headers) = (String::new(), String::new(), HashMap::new());
    r.read_line(&mut first)?;
    while r.read_line(&mut line)? > 2 && headers.len() < 100 {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
        line.clear();
    }
    Ok((first, headers))
}

pub fn serve(listener: TcpListener, handler: fn(&Req) -> Resp) {
    for s in listener.incoming().flatten() {
        thread::spawn(move || handle(s, handler));
    }
}

fn handle(s: TcpStream, handler: fn(&Req) -> Resp) -> io::Result<()> {
    s.set_read_timeout(Some(Duration::from_secs(30)))?;
    let remote = s.peer_addr()?;
    let mut r = BufReader::new(&s);
    let (first, headers) = read_head(&mut r)?;
    let mut parts = first.split_whitespace();
    let (method, target) = (parts.next().unwrap_or("").to_string(), parts.next().unwrap_or("/").to_string());
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0).min(16 << 20);
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    let (path, qs) = target.split_once('?').unwrap_or((&target, ""));
    let query = qs.split('&').filter_map(|p| p.split_once('=').map(|(k, v)| (url_decode(k), url_decode(v)))).collect();
    let body = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let req = Req { method, path: url_decode(path), query, headers, body, remote, target: target.clone() };
    let mut w = BufWriter::with_capacity(256 * 1024, &s);
    let mut send = |code: u16, ctype: &str, len: u64| {
        let head = format!("HTTP/1.1 {} X\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n", code, ctype, len);
        w.write_all(head.as_bytes())
    };
    match handler(&req) {
        Resp::Page(p) => send(200, "text/html; charset=utf-8", p.len() as u64).and_then(|_| w.write_all(p.as_bytes()))?,
        Resp::File(p) => {
            let mut f = File::open(&p)?;
            let len = f.metadata()?.len();
            send(200, "application/octet-stream", len)?;
            io::copy(&mut (&mut f).take(len), &mut w)?;
        }
        Resp::Json(code, v) => {
            let b = serde_json::to_vec(&v).unwrap();
            send(code, "application/json; charset=utf-8", b.len() as u64)?;
            w.write_all(&b)?;
        }
    }
    w.flush()
}

pub struct Reply {
    pub status: u16,
    pub len: Option<u64>,
    pub body: BufReader<TcpStream>,
}

impl Reply {
    pub fn json(self) -> io::Result<Value> {
        let mut v = Vec::new();
        self.body.take(self.len.unwrap_or(u64::MAX)).read_to_end(&mut v)?;
        serde_json::from_slice(&v).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

pub fn request(method: &str, addr: SocketAddr, target: &str, headers: &[(&str, String)], timeout: Duration) -> io::Result<Reply> {
    let s = TcpStream::connect_timeout(&addr, Duration::from_secs(3))?;
    s.set_read_timeout(Some(timeout))?;
    let extra: String = headers.iter().map(|(k, v)| format!("{}: {}\r\n", k, v)).collect();
    (&s).write_all(format!("{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n{}Content-Length: 0\r\n\r\n", method, target, addr, extra).as_bytes())?;
    let mut body = BufReader::with_capacity(256 * 1024, s);
    let (first, h) = read_head(&mut body)?;
    let status = first.split_whitespace().nth(1).and_then(|x| x.parse().ok()).ok_or(io::ErrorKind::InvalidData)?;
    Ok(Reply { status, len: h.get("content-length").and_then(|v| v.parse().ok()), body })
}

#[derive(Clone)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub addr: SocketAddr,
    pub folders: Vec<String>,
    pub seen: Instant,
    /// It has an invite open, so a code typed here may be for it.
    pub inviting: bool,
}

impl Peer {
    pub fn online(&self) -> bool {
        self.seen.elapsed() < Duration::from_secs(12)
    }
}

pub fn hello(ask: bool) -> Value {
    let a = app();
    let (id, name) = (a.me(), a.cfg().device_name.clone());
    json!({"app": APP_TAG, "id": id, "name": name, "port": a.port, "folders": a.all().iter().map(|f| &f.id).collect::<Vec<_>>(), "ask": ask,
           "invite": crate::pair::inviting(&a.pairing.lock().unwrap()), "handle": a.handle.lock().unwrap().clone()})
}

fn upsert(v: &Value, ip: IpAddr) -> bool {
    let (Some(id), Some(port)) = (v["id"].as_str(), v["port"].as_u64()) else { return false };
    if v["app"] != APP_TAG || id == app().me() {
        return false;
    }
    let name = v["name"].as_str().unwrap_or("").to_string();
    let folders = v["folders"].as_array().into_iter().flatten().filter_map(|s| s.as_str().map(String::from)).collect();
    app().learn_name(id, &name, v["handle"].as_str().unwrap_or(""));
    let mut peers = app().peers.lock().unwrap();
    if !peers.get(id).is_some_and(Peer::online) {
        log!("laptop {} ({}) at {}:{}", name, id, ip, port);
    }
    let addr = SocketAddr::new(ip, port as u16);
    peers.insert(id.to_string(), Peer { id: id.to_string(), name, addr, folders, seen: Instant::now(), inviting: v["invite"] == true });
    true
}

/// The laptops sharing this folder that answered lately.
pub fn for_folder(fid: &str) -> Vec<Peer> {
    app().peers.lock().unwrap().values().filter(|p| p.online() && p.folders.iter().any(|f| f == fid)).cloned().collect()
}

pub fn local_v4() -> Vec<(Ipv4Addr, u8)> {
    let nets = sysinfo::Networks::new_with_refreshed_list();
    let mut v: Vec<_> = nets
        .list()
        .values()
        .flat_map(|d| d.ip_networks().iter())
        .filter_map(|n| match n.addr {
            IpAddr::V4(a) if !a.is_loopback() && !a.is_link_local() => Some((a, n.prefix)),
            _ => None,
        })
        .collect();
    v.sort();
    v.dedup();
    v
}

pub fn start() {
    let a = app();
    thread::spawn(manual_loop);
    let sock = match UdpSocket::bind(("0.0.0.0", a.disc_port)) {
        Ok(s) => s,
        Err(e) => return log!("cannot listen for laptops on UDP {}: {}", a.disc_port, e),
    };
    let _ = sock.set_broadcast(true);
    let rx = sock.try_clone().expect("udp clone");
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        while let Ok((n, from)) = rx.recv_from(&mut buf) {
            let v: Value = serde_json::from_slice(&buf[..n]).unwrap_or_default();
            if upsert(&v, from.ip()) && v["ask"] == true {
                let _ = rx.send_to(&serde_json::to_vec(&hello(false)).unwrap(), from);
            }
        }
    });
    thread::spawn(move || {
        let mut addrs = Vec::new();
        for n in 0u64.. {
            // Every network's broadcast address, looked up again every half minute.
            if n % 15 == 0 {
                addrs = vec![Ipv4Addr::BROADCAST];
                addrs.extend(local_v4().iter().filter(|(_, p)| (1..31).contains(p)).map(|(a, p)| Ipv4Addr::from(u32::from(*a) | (u32::MAX >> p))));
            }
            // The first few ask for answers, so laptops already running show up at once.
            let msg = serde_json::to_vec(&hello(n < 3)).unwrap();
            for (ip, port) in addrs.iter().flat_map(|ip| app().disc_send.iter().map(move |p| (*ip, *p))) {
                let _ = sock.send_to(&msg, (ip, port));
            }
            thread::sleep(Duration::from_secs(2));
        }
    });
}

/// Asks the laptops added by address who they are, every few seconds.
fn manual_loop() {
    loop {
        for m in app().cfg().manual_peers.clone() {
            let target = if m.contains(':') { m } else { format!("{}:{}", m, PORT) };
            let Some(addr) = target.to_socket_addrs().ok().and_then(|mut a| a.next()) else { continue };
            if let Ok(v) = request("GET", addr, "/p/hello", &[], Duration::from_secs(3)).and_then(Reply::json) {
                upsert(&v, addr.ip());
            }
        }
        thread::sleep(Duration::from_secs(3));
    }
}
