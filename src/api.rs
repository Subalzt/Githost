//! What the server answers: the app's own window (from this laptop only), and other laptops
//! (only with a request signed with the folder's code).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::{new_code, normalize_code, pretty_code, FolderCfg};
use crate::http::{Req, Resp};
use crate::peers::{self, APP_TAG};
use crate::state::{app, Folder};
use crate::util::{mtime_ms, now_ms};
use crate::{folder_path, git, sync, window};

const PAGE: &str = include_str!("ui.html");

pub fn handle(req: &Req) -> Resp {
    if req.path.starts_with("/p/") {
        return peer(req);
    }
    if !req.remote.ip().is_loopback() {
        return Resp::err(403, "only this laptop's own window");
    }
    let port = app().port;
    let host = req.header("Host").unwrap_or("");
    if host != format!("127.0.0.1:{}", port) && host != format!("localhost:{}", port) {
        return Resp::err(403, "wrong host");
    }
    if req.method == "POST" && req.header("X-FS") != Some("1") {
        return Resp::err(403, "not from the app");
    }
    let b = req.json();
    let s = |k: &str| b.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let folder = || app().folder(if req.method == "GET" { req.q("f") } else { b.get("f").and_then(|x| x.as_str()).unwrap_or("") });
    let done = |r: Result<String, String>| match r {
        Ok(m) => Resp::ok(json!({"ok": true, "message": m})),
        Err(e) => Resp::err(400, e),
    };
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => Resp::Bytes(200, "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
        ("GET", "/api/ping") => Resp::ok(json!({"app": APP_TAG})),
        ("GET", "/api/state") => Resp::ok(state()),
        ("POST", "/api/folders/new") => done(add_folder(&s("path"), None)),
        ("POST", "/api/folders/join") => match normalize_code(&s("code")) {
            Some(c) => done(add_folder(&s("path"), Some(c))),
            None => Resp::err(400, "A code is 12 letters and digits, like 7KQ2-MX9P-4TRD"),
        },
        ("POST", "/api/folders/remove") => {
            let id = s("id");
            {
                let mut c = app().cfg();
                c.folders.retain(|f| f.id() != id);
                c.save();
            }
            app().close_folder(&id);
            Resp::ok(json!({"ok": true}))
        }
        ("POST", "/api/pick") => match window::pick_folder(&s("title")) {
            Some(p) => Resp::ok(json!({"path": p})),
            None => Resp::ok(json!({})),
        },
        ("POST", "/api/open") => match folder() {
            Some(f) => {
                let _ = open::that_detached(&f.root);
                Resp::ok(json!({"ok": true}))
            }
            None => Resp::err(404, "no such folder"),
        },
        ("POST", "/api/open-url") => {
            let u = s("url");
            if u.starts_with("https://github.com/") || u.starts_with("https://git-scm.com/") {
                let _ = open::that_detached(&u);
            }
            Resp::ok(json!({"ok": true}))
        }
        ("POST", "/api/device") => {
            let n = s("name").trim().to_string();
            if n.is_empty() || n.len() > 60 {
                return Resp::err(400, "Give this laptop a name");
            }
            let mut c = app().cfg();
            c.device_name = n;
            c.save();
            Resp::ok(json!({"ok": true}))
        }
        ("POST", "/api/peers/add") => {
            let a = s("addr").trim().to_string();
            if a.is_empty() || a.contains('/') || a.contains(' ') {
                return Resp::err(400, "Type the other laptop's address, like 192.168.1.20");
            }
            let mut c = app().cfg();
            if !c.manual_peers.contains(&a) {
                c.manual_peers.push(a);
                c.save();
            }
            Resp::ok(json!({"ok": true}))
        }
        ("POST", "/api/peers/remove") => {
            let a = s("addr");
            let mut c = app().cfg();
            c.manual_peers.retain(|m| *m != a);
            c.save();
            Resp::ok(json!({"ok": true}))
        }
        ("POST", "/api/quit") => {
            std::thread::spawn(|| {
                std::thread::sleep(Duration::from_millis(300));
                for f in app().all() {
                    f.idx.lock().unwrap().save();
                }
                std::process::exit(0);
            });
            Resp::ok(json!({"ok": true}))
        }
        ("GET", "/api/git") => match folder() {
            Some(f) => Resp::ok(git::status(&f)),
            None => Resp::err(404, "no such folder"),
        },
        ("GET", "/api/git/log") => match folder() {
            Some(f) => Resp::ok(git::log(&f)),
            None => Resp::err(404, "no such folder"),
        },
        ("GET", "/api/git/show") => match folder() {
            Some(f) => Resp::ok(git::show(&f, req.q("c"))),
            None => Resp::err(404, "no such folder"),
        },
        ("GET", "/api/git/diff") => match folder() {
            Some(f) => Resp::ok(git::diff(&f, req.q("c"), req.q("path"))),
            None => Resp::err(404, "no such folder"),
        },
        ("POST", "/api/git/connect") => match folder() {
            Some(f) => done(git::connect(&f, &s("url"), true)),
            None => Resp::err(404, "no such folder"),
        },
        ("POST", "/api/git/commit") => match folder() {
            Some(f) => done(git::commit(&f, &s("message"))),
            None => Resp::err(404, "no such folder"),
        },
        ("POST", "/api/git/pull") => match folder() {
            Some(f) => done(git::pull(&f)),
            None => Resp::err(404, "no such folder"),
        },
        ("POST", "/api/git/push") => match folder() {
            Some(f) => done(git::push(&f)),
            None => Resp::err(404, "no such folder"),
        },
        ("POST", "/api/git/identity") => match folder() {
            Some(f) => {
                if s("name").trim().is_empty() || !s("email").contains('@') {
                    return Resp::err(400, "A name and an email address");
                }
                done(git::set_identity(&f, &s("name"), &s("email")).map(|_| "Saved".into()))
            }
            None => Resp::err(404, "no such folder"),
        },
        _ => Resp::err(404, "not found"),
    }
}

fn add_folder(path: &str, code: Option<String>) -> Result<String, String> {
    let p = PathBuf::from(path.trim());
    if path.trim().is_empty() || !p.is_absolute() {
        return Err("Choose a folder".into());
    }
    let joining = code.is_some();
    if !p.exists() {
        std::fs::create_dir_all(&p).map_err(|e| format!("Cannot make that folder: {}", e))?;
    }
    if !p.is_dir() {
        return Err("That is a file, not a folder".into());
    }
    let p = p.canonicalize().map(strip_verbatim).unwrap_or(p);
    let code = code.unwrap_or_else(new_code);
    let fc = FolderCfg { code: code.clone(), path: p.display().to_string() };
    {
        let mut c = app().cfg();
        if c.folders.iter().any(|f| f.id() == fc.id()) {
            return Err("This laptop already has that folder".into());
        }
        for f in &c.folders {
            let other = Path::new(&f.path);
            if p.starts_with(other) || other.starts_with(&p) {
                return Err(format!("{} is already shared, and one folder cannot be inside another", other.display()));
            }
        }
        c.folders.push(fc.clone());
        c.save();
    }
    app().open_folder(&fc);
    Ok(if joining { "Joined".into() } else { pretty_code(&code) })
}

/// Windows writes canonical paths as \\?\C:\...; show them as people write them.
fn strip_verbatim(p: PathBuf) -> PathBuf {
    let s = p.display().to_string();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if !rest.starts_with("UNC") => PathBuf::from(rest),
        _ => p,
    }
}

fn folder_json(f: &Folder) -> Value {
    let peers = peers::for_folder(&f.id);
    let (files, bytes) = f.idx.lock().unwrap().files_and_bytes();
    let st = f.st.lock().unwrap();
    let synced = st.pending == 0 && peers.iter().all(|p| st.synced_with.get(&p.id).map(|t| now_ms() - t < 8000).unwrap_or(false));
    let g = f.git.lock().unwrap().clone();
    let a = app();
    let activity: Vec<Value> = f
        .act
        .lock()
        .unwrap()
        .iter()
        .take(60)
        .map(|x| json!({"at": x.at, "kind": x.kind, "path": x.path, "who": a.name_of(&x.who), "mine": x.who == a.me()}))
        .collect();
    json!({
        "id": f.id, "name": f.name(), "path": f.root.display().to_string(), "code": pretty_code(&f.code),
        "files": files, "bytes": bytes, "pending": st.pending, "current": st.current, "error": st.error,
        "synced": synced, "peers": peers.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
        "git": {"repo": g.repo, "name": git::web(&g.remote).0, "branch": g.branch, "ahead": g.ahead, "behind": g.behind},
        "activity": activity,
    })
}

fn state() -> Value {
    let a = app();
    let folders = a.all();
    let mine: Vec<String> = folders.iter().map(|f| f.id.clone()).collect();
    let mut peers: Vec<Value> = a
        .peers
        .lock()
        .unwrap()
        .values()
        .filter(|p| p.online())
        .map(|p| {
            let shared: Vec<String> = folders.iter().filter(|f| p.folders.contains(&f.id)).map(|f| f.name()).collect();
            json!({"id": p.id, "name": p.name, "ip": p.addr.ip().to_string(), "manual": p.manual, "shared": shared,
                   "others": p.folders.iter().filter(|x| !mine.contains(x)).count()})
        })
        .collect();
    peers.sort_by(|x, y| x["name"].as_str().cmp(&y["name"].as_str()));
    let folders: Vec<Value> = folders.iter().map(|f| folder_json(f)).collect();
    let c = a.cfg();
    json!({
        "device": {"id": c.device_id, "name": c.device_name},
        "version": env!("CARGO_PKG_VERSION"),
        "port": a.port,
        "folders": folders,
        "peers": peers,
        "manual": c.manual_peers,
        "addresses": addresses(),
    })
}

/// This laptop's own addresses, to give to a laptop that cannot find it by itself.
fn addresses() -> Vec<String> {
    let mut v: Vec<String> = sysinfo::Networks::new_with_refreshed_list()
        .list()
        .values()
        .flat_map(|d| d.ip_networks().iter().map(|n| n.addr).collect::<Vec<_>>())
        .filter(|a| a.is_ipv4() && !a.is_loopback() && !a.to_string().starts_with("169.254."))
        .map(|a| a.to_string())
        .collect();
    v.sort();
    v.dedup();
    v
}

fn peer(req: &Req) -> Resp {
    let parts: Vec<&str> = req.path.trim_start_matches("/p/").split('/').collect();
    if parts == ["hello"] {
        return Resp::ok(peers::hello(false));
    }
    if parts.len() != 2 {
        return Resp::err(404, "not found");
    }
    let Some(f) = app().folder(parts[0]) else { return Resp::err(404, "no such folder") };
    if !sync::verify(&f, req) {
        return Resp::err(403, "wrong code");
    }
    match parts[1] {
        "changes" => {
            let since: u64 = req.q("since").parse().unwrap_or(0);
            let (seq, entries) = {
                let ix = f.idx.lock().unwrap();
                (ix.seq, ix.changes_since(since))
            };
            Resp::ok(json!({"seq": seq, "name": app().cfg().device_name.clone(), "entries": entries, "git": git::for_peers(&f)}))
        }
        "file" => {
            let path = req.q("path");
            if !crate::index::safe_path(path) {
                return Resp::err(400, "bad path");
            }
            let full = folder_path(&f.root, path);
            let ok = {
                let ix = f.idx.lock().unwrap();
                match (ix.entries.get(path), std::fs::metadata(&full)) {
                    (Some(e), Ok(md)) => !e.deleted && e.hash == req.q("hash") && md.len() == e.size && mtime_ms(&md) == e.mtime,
                    _ => false,
                }
            };
            if ok {
                Resp::File(full)
            } else {
                Resp::err(409, "changed")
            }
        }
        _ => Resp::err(404, "not found"),
    }
}
