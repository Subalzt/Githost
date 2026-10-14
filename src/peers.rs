//! Finding the other laptops: every two seconds each says who it is and which folders it shares,
//! by UDP broadcast on every network it is on, and answers at once when a new one asks. Laptops
//! that cannot hear broadcasts (some guest Wi-Fi keeps them apart) can be added by address.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::http;
use crate::log;
use crate::state::app;

pub const APP_TAG: &str = "foldersync/1";

#[derive(Clone)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub addr: SocketAddr,
    pub folders: Vec<String>,
    pub seen: Instant,
    pub manual: bool,
}

impl Peer {
    pub fn online(&self) -> bool {
        self.seen.elapsed() < Duration::from_secs(12)
    }
}

pub fn hello(ask: bool) -> Value {
    let a = app();
    let (id, name) = {
        let c = a.cfg();
        (c.device_id.clone(), c.device_name.clone())
    };
    json!({
        "app": APP_TAG,
        "id": id,
        "name": name,
        "port": a.port,
        "folders": a.all().iter().map(|f| f.id.clone()).collect::<Vec<_>>(),
        "ask": ask,
    })
}

fn upsert(v: &Value, ip: IpAddr, manual: bool) -> bool {
    if v.get("app").and_then(|x| x.as_str()) != Some(APP_TAG) {
        return false;
    }
    let Some(id) = v.get("id").and_then(|x| x.as_str()) else { return false };
    if id == app().me() {
        return false;
    }
    let Some(port) = v.get("port").and_then(|x| x.as_u64()) else { return false };
    let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
    let folders = v
        .get("folders")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect())
        .unwrap_or_default();
    app().learn_name(id, &name);
    let mut peers = app().peers.lock().unwrap();
    let fresh = !peers.get(id).map(|p| p.online()).unwrap_or(false);
    if fresh {
        log!("laptop {} ({}) at {}:{}", name, id, ip, port);
    }
    peers.insert(id.to_string(), Peer { id: id.to_string(), name, addr: SocketAddr::new(ip, port as u16), folders, seen: Instant::now(), manual });
    true
}

/// The laptops sharing this folder that answered lately.
pub fn for_folder(fid: &str) -> Vec<Peer> {
    app().peers.lock().unwrap().values().filter(|p| p.online() && p.folders.iter().any(|f| f == fid)).cloned().collect()
}

fn broadcast_addrs() -> Vec<Ipv4Addr> {
    let mut v = vec![Ipv4Addr::BROADCAST];
    let nets = sysinfo::Networks::new_with_refreshed_list();
    for (_, d) in nets.list() {
        for n in d.ip_networks() {
            if let IpAddr::V4(a) = n.addr {
                if a.is_loopback() || a.is_link_local() || n.prefix == 0 || n.prefix >= 31 {
                    continue;
                }
                let mask = u32::MAX << (32 - n.prefix as u32);
                v.push(Ipv4Addr::from(u32::from(a) | !mask));
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

pub fn start() {
    let a = app();
    let sock = match UdpSocket::bind(("0.0.0.0", a.disc_port)) {
        Ok(s) => s,
        Err(e) => {
            log!("cannot listen for laptops on UDP {}: {}", a.disc_port, e);
            thread::spawn(manual_loop);
            return;
        }
    };
    let _ = sock.set_broadcast(true);
    let rx = sock.try_clone().expect("udp clone");
    thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            let Ok((n, from)) = rx.recv_from(&mut buf) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&buf[..n]) else { continue };
            if upsert(&v, from.ip(), false) && v.get("ask").and_then(|x| x.as_bool()) == Some(true) {
                let _ = rx.send_to(&serde_json::to_vec(&hello(false)).unwrap(), from);
            }
        }
    });
    thread::spawn(move || {
        let mut addrs = broadcast_addrs();
        let mut last_addrs = Instant::now();
        let mut n = 0u32;
        loop {
            if last_addrs.elapsed() > Duration::from_secs(30) {
                addrs = broadcast_addrs();
                last_addrs = Instant::now();
            }
            // The first few ask for answers, so laptops already running show up at once.
            let msg = serde_json::to_vec(&hello(n < 3)).unwrap();
            for ip in &addrs {
                for port in &app().disc_send {
                    let _ = sock.send_to(&msg, (*ip, *port));
                }
            }
            n += 1;
            thread::sleep(Duration::from_secs(2));
        }
    });
    thread::spawn(manual_loop);
}

/// Asks the laptops added by address who they are, every few seconds.
fn manual_loop() {
    loop {
        let list = app().cfg().manual_peers.clone();
        for m in list {
            let target = if m.contains(':') { m.clone() } else { format!("{}:{}", m, crate::DEFAULT_PORT) };
            let Some(addr) = target.to_socket_addrs().ok().and_then(|mut a| a.next()) else { continue };
            if let Ok(r) = http::request("GET", addr, "/p/hello", &[], b"", Duration::from_secs(3)) {
                if let Ok(v) = r.json() {
                    upsert(&v, addr.ip(), true);
                }
            }
        }
        thread::sleep(Duration::from_secs(3));
    }
}
