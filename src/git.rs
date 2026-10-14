//! The folder's GitHub repository, through the laptop's own git (and its credential manager, so
//! signing in is GitHub's own window). Every laptop keeps its own .git; only the files travel. The
//! laptop that commits or pulls notes when its commit moved, and the others, whose files are already
//! the same, move their branch to that commit without touching a file (`git reset --mixed`): the
//! newest move wins, and a laptop with commits the others lack is never moved.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering::SeqCst;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::net::{self, Peer};
use crate::store::{app, Folder};
use crate::{log, now_ms};

#[derive(Default, Clone)]
pub struct State {
    pub repo: bool,
    pub remote: String,
    pub branch: String,
    pub head: String,
    pub ahead: u32,
    pub behind: u32,
    pub error: String,
    /// The repository another laptop sharing this folder uses, offered when this one has none.
    pub suggest: String,
    pub connect_tried: u64,
    pub no_git: bool,
}

pub fn git(dir: &Path, args: &[&str], interactive: bool) -> Result<String, String> {
    let mut c = crate::quiet("git");
    c.arg("-C").arg(dir).args(args).stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
    if !interactive {
        c.env("GCM_INTERACTIVE", "never");
    }
    let out = c.output().map_err(|e| format!("Git is not installed ({})", e))?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
    match (out.status.success(), text(&out.stderr)) {
        (true, _) => Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string()),
        (false, e) if e.is_empty() => Err(text(&out.stdout)),
        (false, e) => Err(e),
    }
}

/// Holds the folder's git for one operation at a time.
struct Busy<'a>(&'a Folder);

fn busy(f: &Folder) -> Result<Busy<'_>, String> {
    if f.git_busy.swap(true, SeqCst) {
        return Err("Git is busy with this folder; try again in a moment".into());
    }
    Ok(Busy(f))
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.git_busy.store(false, SeqCst);
    }
}

pub fn normalize_url(u: &str) -> String {
    let u = u.trim().trim_end_matches('/');
    let simple = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    match u.split_once('/') {
        Some((o, r)) if simple(o) && simple(r) => format!("https://github.com/{}/{}.git", o, r.trim_end_matches(".git")),
        _ if u.starts_with("github.com/") => format!("https://{}", u),
        _ => u.to_string(),
    }
}

/// owner/repo for a GitHub address, and its page.
pub fn web(remote: &str) -> (String, String) {
    let s = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    ["https://github.com/", "http://github.com/", "git@github.com:", "ssh://git@github.com/"]
        .iter()
        .find_map(|pre| s.strip_prefix(pre))
        .map_or((s.to_string(), String::new()), |rest| (rest.to_string(), format!("https://github.com/{}", rest)))
}

pub fn run(f: Arc<Folder>) {
    let mut last_fetch = 0;
    while !f.gone() {
        refresh(&f);
        let st = f.git.lock().unwrap().clone();
        if st.repo && !st.remote.is_empty() && now_ms() - last_fetch > 120_000 && !f.git_busy.load(SeqCst) {
            last_fetch = now_ms();
            if git(&f.root, &["fetch", "--quiet", "origin"], false).is_ok() {
                refresh(&f);
            }
        }
        std::thread::sleep(Duration::from_secs(3));
    }
}

pub fn refresh(f: &Folder) {
    let dir = &f.root;
    let repo = dir.join(".git").exists();
    let branch = if repo { git(dir, &["symbolic-ref", "--short", "-q", "HEAD"], false) } else { Ok(String::new()) };
    let no_git = branch.as_ref().is_err_and(|e| e.starts_with("Git is not installed"));
    let get = |args: &[&str]| if repo && !no_git { git(dir, args, false).unwrap_or_default() } else { String::new() };
    let (head, remote) = (get(&["rev-parse", "-q", "--verify", "HEAD"]), get(&["remote", "get-url", "origin"]));
    let counts: Vec<u32> = get(&["rev-list", "--left-right", "--count", "HEAD...@{upstream}"]).split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if let [0, _] = counts[..] {
        // A commit made or pulled outside this app, once on GitHub, is a move the others follow.
        let mut ix = f.idx.lock().unwrap();
        if !head.is_empty() && head != ix.git_head {
            (ix.git_at, ix.git_head, ix.dirty) = (now_ms(), head.clone(), true);
        }
    }
    let mut g = f.git.lock().unwrap();
    (g.repo, g.no_git, g.branch, g.head, g.remote) = (repo, no_git, branch.unwrap_or_default(), head, remote);
    (g.ahead, g.behind) = (counts.first().copied().unwrap_or(0), counts.get(1).copied().unwrap_or(0));
}

/// What other laptops are told about this folder's repository.
pub fn for_peers(f: &Folder) -> Value {
    let g = f.git.lock().unwrap().clone();
    if !g.repo || g.remote.is_empty() {
        return Value::Null;
    }
    json!({"remote": g.remote, "branch": g.branch, "head": g.head, "at": f.idx.lock().unwrap().git_at})
}

/// Another laptop said where its repository is: offer it, connect to it, or follow its commit.
pub fn from_peer(f: &Folder, p: &Peer, g: &Value) {
    let s = |k: &str| g[k].as_str().unwrap_or("").to_string();
    let (remote, branch, head, at) = (s("remote"), s("branch"), s("head"), g["at"].as_u64().unwrap_or(0));
    let mine = f.git.lock().unwrap().clone();
    let Some(fa) = app().folder(&f.id).filter(|_| !remote.is_empty() && !mine.no_git && !f.git_busy.load(SeqCst)) else { return };
    if !mine.repo {
        let ready = f.st.lock().unwrap().synced_with.contains_key(&p.id) && f.idx.lock().unwrap().files_and_bytes().0 > 0;
        let mut gm = f.git.lock().unwrap();
        gm.suggest = remote.clone();
        if ready && now_ms() - gm.connect_tried > 600_000 {
            gm.connect_tried = now_ms();
            std::thread::spawn(move || match connect(&fa, &remote, false) {
                Ok(_) => log!("{}: connected to {} as the others are", fa.name(), remote),
                Err(e) => fa.git.lock().unwrap().error = e,
            });
        }
        return;
    }
    let same = |a: &str| a.trim().to_ascii_lowercase().trim_end_matches('/').trim_end_matches(".git").replace("git@github.com:", "github.com/").replace("https://", "");
    if same(&mine.remote) != same(&remote) || head.is_empty() {
        return;
    }
    let mut ix = f.idx.lock().unwrap();
    if head == mine.head && at > ix.git_at {
        (ix.git_at, ix.git_head, ix.dirty) = (at, head, true);
    } else if head != mine.head && at > ix.git_at {
        std::thread::spawn(move || {
            if let Err(e) = follow(&fa, &head, &branch, at) {
                fa.git.lock().unwrap().error = e;
            }
        });
    }
}

fn follow(f: &Folder, head: &str, branch: &str, at: u64) -> Result<(), String> {
    let _b = busy(f)?;
    let (dir, target) = (&f.root, format!("{}^{{commit}}", head));
    if git(dir, &["cat-file", "-e", &target], false).is_err() {
        git(dir, &["fetch", "--quiet", "origin"], false)?;
        git(dir, &["cat-file", "-e", &target], false).map_err(|_| "The newest commit is not on GitHub yet")?;
    }
    let cur = f.git.lock().unwrap().clone();
    if !cur.head.is_empty() && git(dir, &["merge-base", "--is-ancestor", &cur.head, head], false).is_err() {
        return Err("This laptop has commits the others do not have; push or pull them first".into());
    }
    if !branch.is_empty() && branch != cur.branch {
        git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)], false)?;
    }
    git(dir, &["reset", "-q", "--mixed", head], false)?;
    let _ = git(dir, &["branch", "-q", &format!("--set-upstream-to=origin/{}", branch)], false);
    moved(f, at);
    f.note("followed", &head[..head.len().min(7)], "");
    Ok(())
}

/// The commit moved here: the others follow it when `at` is newer than theirs.
fn moved(f: &Folder, at: u64) {
    refresh(f);
    let head = f.git.lock().unwrap().head.clone();
    f.git.lock().unwrap().error.clear();
    let mut ix = f.idx.lock().unwrap();
    (ix.git_at, ix.git_head, ix.dirty) = (at, head, true);
    f.save(&mut ix);
}

pub fn connect(f: &Folder, url: &str, interactive: bool) -> Result<String, String> {
    let (url, dir) = (normalize_url(url), &f.root);
    if url.is_empty() {
        return Err("Paste the repository's address".into());
    }
    let _b = busy(f)?;
    let empty = std::fs::read_dir(dir).map_or(true, |rd| rd.flatten().all(|e| e.file_name() == crate::sync::SCRATCH));
    if dir.join(".git").exists() {
        let verb = if git(dir, &["remote", "get-url", "origin"], false).is_ok() { "set-url" } else { "add" };
        git(dir, &["remote", verb, "origin", &url], false)?;
        git(dir, &["fetch", "--quiet", "origin"], interactive)?;
    } else if interactive && empty && net::for_folder(&f.id).is_empty() {
        git(dir, &["clone", "--quiet", &url, "."], true)?;
    } else {
        // The files are already here (or arriving from the others): keep them and take only the history.
        git(dir, &["init", "--quiet"], false)?;
        git(dir, &["remote", "add", "origin", &url], false)?;
        if let Err(e) = git(dir, &["fetch", "--quiet", "origin"], interactive) {
            let _ = std::fs::remove_dir_all(dir.join(".git"));
            return Err(e);
        }
        let sym = git(dir, &["ls-remote", "--symref", "origin", "HEAD"], interactive).unwrap_or_default();
        let branch = sym.lines().find_map(|l| l.strip_prefix("ref: refs/heads/")?.split_whitespace().next()).unwrap_or("main");
        git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)], false)?;
        if git(dir, &["reset", "-q", "--mixed", &format!("origin/{}", branch)], false).is_ok() {
            let _ = git(dir, &["branch", "-q", &format!("--set-upstream-to=origin/{}", branch)], false);
        }
    }
    moved(f, now_ms());
    Ok("Connected".into())
}

fn push_now(f: &Folder) -> Result<String, String> {
    let dir = &f.root;
    let branch = git(dir, &["symbolic-ref", "--short", "-q", "HEAD"], false).unwrap_or_else(|_| "main".into());
    let push = || git(dir, &["push", "--quiet", "-u", "origin", &branch], true);
    if let Err(e) = push() {
        if !["rejected", "fetch first", "non-fast-forward"].iter().any(|r| e.contains(r)) {
            return Err(e);
        }
        if let Err(e) = git(dir, &["pull", "--quiet", "--rebase", "--autostash", "origin", &branch], true) {
            let _ = git(dir, &["rebase", "--abort"], false);
            return Err(format!("Someone else's commit clashes with this one: {}", e));
        }
        push()?;
    }
    moved(f, now_ms());
    Ok(format!("Pushed to {}", branch))
}

pub fn commit(f: &Folder, message: &str) -> Result<String, String> {
    let (_b, dir) = (busy(f)?, &f.root);
    if !dir.join(".git").exists() {
        return Err("Connect a repository first".into());
    }
    git(dir, &["add", "-A"], false)?;
    if git(dir, &["diff", "--cached", "--quiet"], false).is_ok() {
        return Err("Nothing to commit".into());
    }
    let names = git(dir, &["diff", "--cached", "--name-only"], false).unwrap_or_default();
    let files: Vec<&str> = names.lines().map(|l| l.rsplit('/').next().unwrap_or(l)).collect();
    let msg = match (message.trim(), files.len()) {
        ("", 1) => format!("Update {}", files[0]),
        ("", 2) => format!("Update {} and {}", files[0], files[1]),
        ("", n) => format!("Update {}, {} and {} more", files[0], files[1], n - 2),
        (m, _) => m.to_string(),
    };
    git(dir, &["commit", "--quiet", "-m", &msg], false).map_err(|e| if e.contains("tell me who you are") { "Set your name and email for commits first".into() } else { e })?;
    if git(dir, &["remote", "get-url", "origin"], false).is_err() {
        refresh(f);
        return Ok("Committed on this laptop".into());
    }
    push_now(f).map(|p| format!("Committed. {}", p)).map_err(|e| format!("Committed here, but not pushed: {}", e))
}

pub fn push(f: &Folder) -> Result<String, String> {
    let _b = busy(f)?;
    push_now(f)
}

pub fn pull(f: &Folder) -> Result<String, String> {
    let _b = busy(f)?;
    git(&f.root, &["pull", "--quiet", "--rebase", "--autostash"], true)?;
    moved(f, now_ms());
    Ok("Up to date with GitHub".into())
}

pub fn set_identity(f: &Folder, name: &str, email: &str) -> Result<String, String> {
    git(&f.root, &["config", "user.name", name.trim()], false)?;
    git(&f.root, &["config", "user.email", email.trim()], false).map(|_| "Saved".into())
}

/// The state for the window, with the files changed since the last commit and who changed each.
pub fn status(f: &Folder) -> Value {
    let g = f.git.lock().unwrap().clone();
    let mut changes = Vec::new();
    let out = if g.repo { git(&f.root, &["status", "--porcelain=v1", "-z", "-uall"], false).unwrap_or_default() } else { String::new() };
    let mut items = out.split('\0');
    // An empty item ends the list; a short one cannot be "XY path".
    while let Some(item) = items.next().filter(|i| i.len() > 3) {
        let (xy, path) = item.split_at(3);
        if xy.starts_with(['R', 'C']) {
            items.next(); // the old name
        }
        let kind = ["new", "deleted", "renamed", "edited"][[xy.contains(['?', 'A']), xy.contains('D'), xy.contains('R'), true].iter().position(|&b| b).unwrap()];
        let by = f.idx.lock().unwrap().entries.get(path).map(|e| app().name_of(&e.by)).unwrap_or_default();
        changes.push(json!({"path": path, "kind": kind, "by": by}));
    }
    let id = |k: &str| git(if g.repo { &f.root } else { Path::new(".") }, &["config", k], false).unwrap_or_default();
    let (repo_name, repo_web) = web(&g.remote);
    json!({
        "repo": g.repo, "remote": g.remote, "repo_name": repo_name, "repo_web": repo_web, "branch": g.branch, "head": g.head,
        "ahead": g.ahead, "behind": g.behind, "error": g.error, "suggest": if g.repo { "" } else { g.suggest.as_str() }, "no_git": g.no_git,
        "busy": f.git_busy.load(SeqCst), "changes": changes, "identity": {"name": id("user.name"), "email": id("user.email")},
    })
}

pub fn log(f: &Folder) -> Value {
    let g = f.git.lock().unwrap().clone();
    let out = if g.head.is_empty() { String::new() } else { git(&f.root, &["log", "-n", "300", "--format=%H%x1f%an%x1f%at%x1f%s%x1e"], false).unwrap_or_default() };
    let commits: Vec<Value> = out
        .split('\x1e')
        .map(|r| r.trim().split('\x1f').collect::<Vec<_>>())
        .filter(|p| p.len() == 4)
        .enumerate()
        .map(|(i, p)| json!({"hash": p[0], "author": p[1], "at": p[2].parse::<u64>().unwrap_or(0) * 1000, "subject": p[3], "local": (i as u32) < g.ahead}))
        .collect();
    json!({"commits": commits, "repo_web": web(&g.remote).1})
}

fn is_hash(c: &str) -> bool {
    (4..=64).contains(&c.len()) && c.chars().all(|x| x.is_ascii_hexdigit())
}

pub fn show(f: &Folder, c: &str) -> Value {
    let run = |args: &[&str]| if is_hash(c) { git(&f.root, args, false).unwrap_or_default() } else { String::new() };
    let nums = run(&["show", "--no-renames", "--format=", "--numstat", c]);
    let files: Vec<Value> = run(&["show", "--no-renames", "--format=", "--name-status", c])
        .lines()
        .zip(nums.lines().chain(std::iter::repeat("")))
        .filter_map(|(l, n)| {
            let ((k, path), mut n) = (l.split_once('\t')?, n.split('\t'));
            Some(json!({"path": path, "kind": match k { "A" => "new", "D" => "deleted", _ => "edited" }, "added": n.next(), "removed": n.next()}))
        })
        .collect();
    json!({"message": run(&["show", "-s", "--format=%B", c]), "files": files})
}

pub fn diff(f: &Folder, c: &str, path: &str) -> Value {
    let ok = is_hash(c) && crate::store::safe_path(path);
    let d = if ok { git(&f.root, &["show", "--no-renames", "--format=", c, "--", path], false).unwrap_or_default() } else { String::new() };
    json!({"diff": d.chars().take(200_000).collect::<String>()})
}
