//! What the server answers: the app's own window (from this laptop only), and other laptops (only
//! with requests signed with the folder's code). Also the window itself and the folder picker.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use serde_json::{json, Value};

use crate::net::{self, Req, Resp, APP_TAG};
use crate::store::{app, safe_path, Folder, FolderCfg};
use crate::{folder_path, git, log, mtime, now_ms, pair, quiet, sync};

pub fn handle(req: &Req) -> Resp {
    if let Some(rest) = req.path.strip_prefix("/p/") {
        return peer(req, rest);
    }
    let host = req.header("Host");
    if !req.remote.ip().is_loopback() || ![format!("127.0.0.1:{}", app().port), format!("localhost:{}", app().port)].iter().any(|h| h == host) {
        return Resp::err(403, "only this laptop's own window");
    }
    if req.method == "POST" && req.header("X-FS") != "1" {
        return Resp::err(403, "not from the app");
    }
    let s = |k: &str| req.body[k].as_str().unwrap_or("").to_string();
    let done = |r: Result<String, String>| r.map_or_else(|e| Resp::err(400, e), |m| Resp::Json(200, json!({"ok": true, "message": m})));
    let ok = || Resp::Json(200, json!({"ok": true}));
    let c = || app().cfg();
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/") => return Resp::Page(include_str!("ui.html")),
        ("GET", "/api/ping") => return Resp::Json(200, json!({"app": APP_TAG})),
        ("GET", "/api/state") => return Resp::Json(200, state()),
        ("POST", "/api/folders/new") => return done(add_folder(FolderCfg::new(&s("path")))),
        ("POST", "/api/folders/join") => return done(pair::join(&s("code"), &s("path"))),
        ("POST", "/api/folders/remove") => {
            let mut cfg = c();
            cfg.folders.retain(|f| f.id != s("id"));
            app().save(&cfg);
            drop(cfg);
            app().close_folder(&s("id"));
            return ok();
        }
        ("POST", "/api/pick") => return Resp::Json(200, json!({"path": pick_folder(&s("title"))})),
        ("POST", "/api/open-url") if ["https://github.com/", "https://git-scm.com/"].iter().any(|p| s("url").starts_with(p)) => {
            let _ = open::that_detached(s("url"));
            return ok();
        }
        ("POST", "/api/device") if !s("name").trim().is_empty() => {
            let mut cfg = c();
            cfg.device_name = s("name").trim().chars().take(60).collect();
            app().save(&cfg);
            return ok();
        }
        ("POST", "/api/peers/add") if !s("addr").trim().is_empty() && !s("addr").contains(['/', ' ']) => {
            let mut cfg = c();
            if !cfg.manual_peers.contains(&s("addr").trim().to_string()) {
                cfg.manual_peers.push(s("addr").trim().to_string());
                app().save(&cfg);
            }
            return ok();
        }
        ("POST", "/api/peers/remove") => {
            let mut cfg = c();
            cfg.manual_peers.retain(|m| *m != s("addr"));
            app().save(&cfg);
            return ok();
        }
        ("POST", "/api/quit") => {
            std::thread::spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(300));
                app().all().iter().for_each(|f| f.save(&mut f.idx.lock().unwrap()));
                std::process::exit(0)
            });
            return ok();
        }
        _ => {}
    }
    // The rest are about one folder.
    let Some(f) = app().folder(if req.method == "GET" { req.q("f") } else { req.body["f"].as_str().unwrap_or("") }) else {
        return Resp::err(404, "not found");
    };
    match req.path.as_str() {
        "/api/open" => drop(open::that_detached(&f.root)),
        "/api/invite" => return Resp::Json(200, pair::invite(&f.id)),
        "/api/git" => return Resp::Json(200, git::status(&f)),
        "/api/git/log" => return Resp::Json(200, git::log(&f)),
        "/api/git/show" => return Resp::Json(200, git::show(&f, req.q("c"))),
        "/api/git/diff" => return Resp::Json(200, git::diff(&f, req.q("c"), req.q("path"))),
        "/api/git/connect" => return done(git::connect(&f, &s("url"), true)),
        "/api/git/commit" => return done(git::commit(&f, &s("message"))),
        "/api/git/pull" => return done(git::pull(&f)),
        "/api/git/push" => return done(git::push(&f)),
        "/api/git/identity" if !s("name").trim().is_empty() && s("email").contains('@') => return done(git::set_identity(&f, &s("name"), &s("email"))),
        "/api/git/identity" => return Resp::err(400, "A name and an email address"),
        _ => return Resp::err(404, "not found"),
    }
    ok()
}

/// Starts sharing a folder here, new or joined; gives its id.
pub fn add_folder(mut fc: FolderCfg) -> Result<String, String> {
    let p = PathBuf::from(fc.path.trim());
    if !p.is_absolute() {
        return Err("Choose a folder".into());
    }
    std::fs::create_dir_all(&p).map_err(|e| format!("Cannot use that folder: {}", e))?;
    // Windows writes canonical paths as \\?\C:\...; keep them as people write them.
    let p = p.canonicalize().map_or(p, |c| PathBuf::from(c.display().to_string().trim_start_matches(r"\\?\")));
    fc.path = p.display().to_string();
    let mut c = app().cfg();
    if c.folders.iter().any(|f| f.id == fc.id) {
        return Err("This laptop already has that folder".into());
    }
    if let Some(other) = c.folders.iter().map(|f| Path::new(&f.path)).find(|o| p.starts_with(o) || o.starts_with(&p)) {
        return Err(format!("{} is already shared, and one folder cannot be inside another", other.display()));
    }
    c.folders.push(fc.clone());
    app().save(&c);
    drop(c);
    app().open_folder(&fc);
    Ok(fc.id)
}

fn folder_json(f: &Folder) -> Value {
    let (a, peers) = (app(), net::for_folder(&f.id));
    let (files, bytes) = f.idx.lock().unwrap().files_and_bytes();
    let g = f.git.lock().unwrap().clone();
    let me = a.me();
    let activity: Vec<Value> = f.act.lock().unwrap().iter().take(60).map(|x| json!({"at": x.at, "kind": x.kind, "path": x.path, "who": a.name_of(&x.who), "mine": x.who == me})).collect();
    let st = f.st.lock().unwrap();
    let invite = pair::invite_for(&a.pairing.lock().unwrap(), &f.id).map(|i| json!({"code": i.code, "until": i.until}));
    let synced = st.pending == 0 && peers.iter().all(|p| st.synced_with.get(&p.id).is_some_and(|t| now_ms() - t < 8000));
    json!({
        "id": f.id, "name": f.name(), "path": f.root.display().to_string(), "invite": invite, "files": files, "bytes": bytes,
        "pending": st.pending, "current": st.current, "error": st.error, "synced": synced, "peers": peers.iter().map(|p| &p.name).collect::<Vec<_>>(),
        "git": {"repo": g.repo, "branch": g.branch}, "activity": activity,
    })
}

fn state() -> Value {
    let a = app();
    let folders = a.all();
    let mut peers: Vec<Value> = a
        .peers
        .lock()
        .unwrap()
        .values()
        .filter(|p| p.online())
        .map(|p| {
            let shared: Vec<String> = folders.iter().filter(|f| p.folders.contains(&f.id)).map(|f| f.name()).collect();
            json!({"name": p.name, "shared": shared, "others": p.folders.len() > shared.len()})
        })
        .collect();
    peers.sort_by(|x, y| x["name"].as_str().cmp(&y["name"].as_str()));
    let folders: Vec<Value> = folders.iter().map(|f| folder_json(f)).collect();
    let c = a.cfg();
    json!({
        "device": {"name": c.device_name}, "version": env!("CARGO_PKG_VERSION"), "folders": folders, "peers": peers, "manual": c.manual_peers,
        "addresses": net::local_v4().iter().map(|(ip, _)| ip.to_string()).collect::<Vec<_>>(),
    })
}

fn peer(req: &Req, rest: &str) -> Resp {
    let r = match rest {
        "hello" => return Resp::Json(200, net::hello(false)),
        "pair/start" => pair::start(req.q("x")),
        "pair/finish" => pair::finish(req.q("sid"), req.q("confirm")),
        _ => Err(String::new()),
    };
    if rest.starts_with("pair/") {
        return r.map_or_else(|e| Resp::err(403, e), |v| Resp::Json(200, v));
    }
    let Some((f, what)) = rest.split_once('/').and_then(|(id, what)| Some((app().folder(id)?, what))) else { return Resp::err(404, "no such folder") };
    if !sync::verify(&f, req) {
        return Resp::err(403, "wrong code");
    }
    let git = git::for_peers(&f); // before the index is held, as it takes it too
    let ix = f.idx.lock().unwrap();
    match what {
        "changes" => {
            let entries = ix.changes_since(req.q("since").parse().unwrap_or(0));
            Resp::Json(200, json!({"seq": ix.seq, "entries": entries, "git": git}))
        }
        "file" => {
            let (path, full) = (req.q("path"), folder_path(&f.root, req.q("path")));
            let ok = safe_path(path)
                && ix.entries.get(path).zip(std::fs::metadata(&full).ok()).is_some_and(|(e, md)| !e.deleted && e.hash == req.q("hash") && md.len() == e.size && mtime(&md) == e.mtime);
            if ok {
                Resp::File(full)
            } else {
                Resp::err(409, "changed")
            }
        }
        _ => Resp::err(404, "not found"),
    }
}

/// The app's window: Edge or Chrome in app mode, with no tabs or address bar and its own place on the taskbar.
pub fn open_window(port: u16) {
    let url = format!("http://127.0.0.1:{}/", port);
    let mut found: Vec<PathBuf> = ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"]
        .iter()
        .filter_map(|v| std::env::var(v).ok())
        .flat_map(|b| [r"Microsoft\Edge\Application\msedge.exe", r"Google\Chrome\Application\chrome.exe"].map(|e| Path::new(&b).join(e)))
        .collect();
    found.extend(["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome", "/usr/bin/google-chrome", "/usr/bin/chromium", "/usr/bin/microsoft-edge"].map(PathBuf::from));
    let spawned = found.into_iter().find(|p| p.is_file()).map(|b| {
        std::process::Command::new(b)
            .arg(format!("--app={}", url))
            .arg(format!("--user-data-dir={}", app().data.join("window").display()))
            .args(["--window-size=1180,780", "--no-first-run", "--no-default-browser-check"])
            .stderr(Stdio::null())
            .spawn()
    });
    if !matches!(spawned, Some(Ok(_))) {
        let _ = open::that_detached(&url);
    }
}

/// The system's folder picker, in front of the app's window ("" when closed). On Windows, the modern
/// one through its COM interface; only the methods up to GetResult need their places in the table.
#[cfg(windows)]
const PICKER: &str = r#"
Add-Type -TypeDefinition @"
using System; using System.Runtime.InteropServices;
public static class P {
  [ComImport, Guid("DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7")] class D {}
  [ComImport, Guid("42f85136-db7e-439c-85f1-e4075d135fc8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)] interface I {
    [PreserveSig] int Show(IntPtr p); void a(); void b(); void c(); void d(); void e(); void SetOptions(uint o); void GetOptions(out uint o);
    void f(); void g(); void h(); void i(); void j(); void k(); void SetTitle([MarshalAs(UnmanagedType.LPWStr)] string t); void l(); void m(); void GetResult(out S s); }
  [ComImport, Guid("43826D1E-E718-42EE-BC55-A1E261C37BFE"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)] interface S {
    void a(); void b(); void Name(uint n, [MarshalAs(UnmanagedType.LPWStr)] out string s); }
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  public static string Pick(string t) {
    var d = (I)new D(); uint o; d.GetOptions(out o); d.SetOptions(o | 0x60); d.SetTitle(t);
    if (d.Show(GetForegroundWindow()) != 0) return ""; S s; d.GetResult(out s); string r; s.Name(0x80058000, out r); return r; }
}
"@
[Console]::OutputEncoding = [Text.Encoding]::UTF8
[P]::Pick($env:FS_TITLE)
"#;

fn pick_folder(title: &str) -> String {
    #[cfg(windows)]
    let mut c = {
        let script = app().data.join("pick.ps1");
        let _ = std::fs::write(&script, PICKER);
        let mut c = quiet("powershell.exe");
        c.args(["-NoProfile", "-NonInteractive", "-STA", "-ExecutionPolicy", "Bypass", "-File"]).arg(script).env("FS_TITLE", title);
        c
    };
    #[cfg(target_os = "macos")]
    let mut c = quiet("osascript");
    #[cfg(target_os = "macos")]
    c.arg("-e").arg(format!("POSIX path of (choose folder with prompt \"{}\")", title.replace('"', "")));
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut c = quiet("zenity");
    #[cfg(all(unix, not(target_os = "macos")))]
    c.args(["--file-selection", "--directory", "--title", title]);
    match c.stdin(Stdio::null()).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).trim().trim_end_matches(['/', '\\']).to_string(),
        Err(e) => {
            log!("folder picker: {}", e);
            String::new()
        }
    }
}
