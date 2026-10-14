//! The folder's GitHub repository, through the git on the laptop (and its credential manager, so
//! signing in is GitHub's own window, never a password typed into this app).
//!
//! Every laptop keeps its own .git; only the files travel between them. So that one Commit button
//! serves all of them, the laptop that commits (or pulls) notes when its commit moved, and the
//! others, whose files are already the same, move their own branch to that commit without touching
//! a file (`git reset --mixed`): the newest move wins, and a laptop with commits the others lack
//! is never moved.

use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use serde_json::{json, Value};

use crate::log;
use crate::peers::{self, Peer};
use crate::state::{app, Folder};
use crate::util::{now_ms, quiet_command};

#[derive(Default, Clone)]
pub struct State {
    pub repo: bool,
    pub remote: String,
    pub branch: String,
    pub head: String,
    pub ahead: u32,
    pub behind: u32,
    pub upstream: bool,
    pub fetched: u64,
    pub error: String,
    /// The repository another laptop sharing this folder uses, offered when this one has none.
    pub suggest: String,
    pub connect_tried: u64,
    pub no_git: bool,
}

pub fn git(dir: &Path, args: &[&str], interactive: bool) -> Result<String, String> {
    let mut c = quiet_command("git");
    c.arg("-C").arg(dir).args(args).stdin(Stdio::null()).env("GIT_TERMINAL_PROMPT", "0");
    if !interactive {
        c.env("GCM_INTERACTIVE", "never");
    }
    let out = c.output().map_err(|e| format!("Git is not installed ({})", e))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        let e = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let o = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Err(if e.is_empty() { o } else { e })
    }
}

/// Holds the folder's git for one operation at a time.
struct Busy<'a>(&'a Folder);

impl<'a> Busy<'a> {
    fn take(f: &'a Folder) -> Result<Busy<'a>, String> {
        if f.git_busy.swap(true, Ordering::SeqCst) {
            return Err("Git is busy with this folder; try again in a moment".into());
        }
        Ok(Busy(f))
    }
}

impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.git_busy.store(false, Ordering::SeqCst);
    }
}

pub fn normalize_url(u: &str) -> String {
    let u = u.trim().trim_end_matches('/');
    let simple = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
    if let Some((o, r)) = u.split_once('/') {
        if simple(o) && simple(r) {
            return format!("https://github.com/{}/{}.git", o, r.trim_end_matches(".git"));
        }
    }
    if u.starts_with("github.com/") {
        return format!("https://{}", u);
    }
    u.to_string()
}

/// A repository address in one form, to tell whether two laptops use the same one.
pub fn same_repo(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        let s = s.trim().to_ascii_lowercase();
        let s = s.trim_end_matches('/').trim_end_matches(".git").to_string();
        s.replace("git@github.com:", "github.com/").replace("https://", "").replace("http://", "").replace("ssh://git@", "")
    };
    !a.is_empty() && norm(a) == norm(b)
}

/// owner/repo for a GitHub address, and the page to open for it.
pub fn web(remote: &str) -> (String, String) {
    let s = remote.trim().trim_end_matches('/').trim_end_matches(".git");
    for pre in ["https://github.com/", "http://github.com/", "git@github.com:", "ssh://git@github.com/"] {
        if let Some(rest) = s.strip_prefix(pre) {
            return (rest.to_string(), format!("https://github.com/{}", rest));
        }
    }
    (s.to_string(), String::new())
}

pub fn run(f: Arc<Folder>) {
    let mut last_fetch = 0u64;
    while !f.gone() {
        refresh(&f);
        let st = f.git.lock().unwrap().clone();
        if st.repo && !st.remote.is_empty() && now_ms() - last_fetch > 120_000 && !f.git_busy.load(Ordering::SeqCst) {
            last_fetch = now_ms();
            if git(&f.root, &["fetch", "--quiet", "origin"], false).is_ok() {
                f.git.lock().unwrap().fetched = now_ms();
                refresh(&f);
            }
        }
        thread::sleep(Duration::from_secs(3));
    }
}

pub fn refresh(f: &Folder) {
    let dir = &f.root;
    if !dir.join(".git").exists() {
        let mut g = f.git.lock().unwrap();
        g.repo = false;
        g.remote.clear();
        g.head.clear();
        return;
    }
    let branch = git(dir, &["symbolic-ref", "--short", "-q", "HEAD"], false);
    if let Err(e) = &branch {
        if e.starts_with("Git is not installed") {
            let mut g = f.git.lock().unwrap();
            g.no_git = true;
            return;
        }
    }
    let branch = branch.unwrap_or_default();
    let head = git(dir, &["rev-parse", "-q", "--verify", "HEAD"], false).unwrap_or_default();
    let remote = git(dir, &["remote", "get-url", "origin"], false).unwrap_or_default();
    let counts = git(dir, &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"], false);
    let (ahead, behind, upstream) = match &counts {
        Ok(s) => {
            let mut it = s.split_whitespace().map(|x| x.parse::<u32>().unwrap_or(0));
            (it.next().unwrap_or(0), it.next().unwrap_or(0), true)
        }
        Err(_) => (0, 0, false),
    };
    {
        // A commit made or pulled outside this app (in a terminal or an editor), once on GitHub,
        // counts as a move the other laptops follow.
        let mut ix = f.idx.lock().unwrap();
        if !head.is_empty() && head != ix.git_head && upstream && ahead == 0 {
            ix.git_at = now_ms();
            ix.git_head = head.clone();
            ix.dirty = true;
        }
    }
    let mut g = f.git.lock().unwrap();
    g.repo = true;
    g.no_git = false;
    g.branch = branch;
    g.head = head;
    g.remote = remote;
    g.ahead = ahead;
    g.behind = behind;
    g.upstream = upstream;
}

/// What other laptops are told about this folder's repository.
pub fn for_peers(f: &Folder) -> Value {
    let g = f.git.lock().unwrap().clone();
    if !g.repo || g.remote.is_empty() {
        return Value::Null;
    }
    let ix = f.idx.lock().unwrap();
    json!({"remote": g.remote, "branch": g.branch, "head": g.head, "at": ix.git_at})
}

/// Another laptop said where its repository is: offer it, connect to it, or follow its commit.
pub fn from_peer(f: &Folder, p: &Peer, g: Option<&Value>) {
    let Some(g) = g.filter(|g| g.is_object()) else { return };
    let s = |k: &str| g.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let (remote, branch, head) = (s("remote"), s("branch"), s("head"));
    let at = g.get("at").and_then(|x| x.as_u64()).unwrap_or(0);
    if remote.is_empty() {
        return;
    }
    let mine = f.git.lock().unwrap().clone();
    if f.git_busy.load(Ordering::SeqCst) || mine.no_git {
        return;
    }
    let Some(fa) = app().folder(&f.id) else { return };
    if !mine.repo {
        let synced = f.st.lock().unwrap().synced_with.contains_key(&p.id);
        let has_files = f.idx.lock().unwrap().files_and_bytes().0 > 0;
        let mut gm = f.git.lock().unwrap();
        gm.suggest = remote.clone();
        if synced && has_files && now_ms() - gm.connect_tried > 600_000 {
            gm.connect_tried = now_ms();
            drop(gm);
            thread::spawn(move || match connect(&fa, &remote, false) {
                Ok(_) => log!("{}: connected to {} as the others are", fa.name(), remote),
                Err(e) => {
                    log!("{}: could not connect to {} by itself: {}", fa.name(), remote, e);
                    fa.git.lock().unwrap().error = e;
                }
            });
        }
        return;
    }
    if !same_repo(&mine.remote, &remote) || head.is_empty() {
        return;
    }
    let (my_at, _) = {
        let ix = f.idx.lock().unwrap();
        (ix.git_at, ix.git_head.clone())
    };
    if head == mine.head {
        if at > my_at {
            let mut ix = f.idx.lock().unwrap();
            ix.git_at = at;
            ix.git_head = head;
            ix.dirty = true;
        }
        return;
    }
    if at <= my_at {
        return;
    }
    let who = p.name.clone();
    thread::spawn(move || {
        if let Err(e) = follow(&fa, &head, &branch, at) {
            log!("{}: following {}'s commit: {}", fa.name(), who, e);
            fa.git.lock().unwrap().error = e;
        }
    });
}

fn follow(f: &Folder, head: &str, branch: &str, at: u64) -> Result<(), String> {
    let _b = Busy::take(f)?;
    let dir = &f.root;
    let target = format!("{}^{{commit}}", head);
    if git(dir, &["cat-file", "-e", &target], false).is_err() {
        git(dir, &["fetch", "--quiet", "origin"], false)?;
        git(dir, &["cat-file", "-e", &target], false).map_err(|_| "The newest commit is not on GitHub yet".to_string())?;
    }
    let cur = f.git.lock().unwrap().clone();
    if !cur.head.is_empty() && git(dir, &["merge-base", "--is-ancestor", &cur.head, head], false).is_err() {
        return Err("This laptop has commits the others do not have; push or pull them first".into());
    }
    if !branch.is_empty() && branch != cur.branch {
        git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)], false)?;
    }
    git(dir, &["reset", "-q", "--mixed", head], false)?;
    if !branch.is_empty() {
        let _ = git(dir, &["branch", "-q", &format!("--set-upstream-to=origin/{}", branch)], false);
    }
    {
        let mut ix = f.idx.lock().unwrap();
        ix.git_at = at;
        ix.git_head = head.to_string();
        ix.dirty = true;
        ix.save();
    }
    f.git.lock().unwrap().error.clear();
    refresh(f);
    f.note("followed", &head[..head.len().min(7)], "");
    Ok(())
}

fn moved(f: &Folder) {
    refresh(f);
    let head = f.git.lock().unwrap().head.clone();
    let mut ix = f.idx.lock().unwrap();
    ix.git_at = now_ms();
    ix.git_head = head;
    ix.dirty = true;
    ix.save();
}

fn folder_is_empty(f: &Folder) -> bool {
    std::fs::read_dir(&f.root)
        .map(|rd| rd.flatten().all(|e| e.file_name().to_string_lossy() == crate::ignore::SCRATCH))
        .unwrap_or(true)
}

pub fn connect(f: &Folder, url: &str, interactive: bool) -> Result<String, String> {
    let url = normalize_url(url);
    if url.is_empty() {
        return Err("Paste the repository's address".into());
    }
    let _b = Busy::take(f)?;
    let dir = &f.root;
    let out;
    if dir.join(".git").exists() {
        if git(dir, &["remote", "get-url", "origin"], false).is_ok() {
            git(dir, &["remote", "set-url", "origin", &url], false)?;
        } else {
            git(dir, &["remote", "add", "origin", &url], false)?;
        }
        git(dir, &["fetch", "--quiet", "origin"], interactive)?;
        out = "Connected".to_string();
    } else if interactive && folder_is_empty(f) && peers::for_folder(&f.id).is_empty() {
        git(dir, &["clone", "--quiet", &url, "."], true)?;
        out = "Cloned".to_string();
    } else {
        // The files are already here (or arriving from the others): keep them as they are and
        // take only the history.
        git(dir, &["init", "--quiet"], false)?;
        git(dir, &["remote", "add", "origin", &url], false)?;
        if let Err(e) = git(dir, &["fetch", "--quiet", "origin"], interactive) {
            let _ = std::fs::remove_dir_all(dir.join(".git"));
            return Err(e);
        }
        let sym = git(dir, &["ls-remote", "--symref", "origin", "HEAD"], interactive).unwrap_or_default();
        let branch = sym
            .lines()
            .find_map(|l| l.strip_prefix("ref: refs/heads/").map(|r| r.split_whitespace().next().unwrap_or("").to_string()))
            .filter(|b| !b.is_empty());
        match branch {
            Some(b) => {
                git(dir, &["symbolic-ref", "HEAD", &format!("refs/heads/{}", b)], false)?;
                git(dir, &["reset", "-q", "--mixed", &format!("origin/{}", b)], false)?;
                let _ = git(dir, &["branch", "-q", &format!("--set-upstream-to=origin/{}", b)], false);
            }
            None => {
                git(dir, &["symbolic-ref", "HEAD", "refs/heads/main"], false)?;
            }
        }
        out = "Connected".to_string();
    }
    f.git.lock().unwrap().error.clear();
    moved(f);
    Ok(out)
}

fn rejected(e: &str) -> bool {
    e.contains("rejected") || e.contains("fetch first") || e.contains("non-fast-forward")
}

fn push_now(f: &Folder) -> Result<String, String> {
    let dir = &f.root;
    let branch = git(dir, &["symbolic-ref", "--short", "-q", "HEAD"], false).unwrap_or_else(|_| "main".into());
    match git(dir, &["push", "--quiet", "-u", "origin", &branch], true) {
        Ok(_) => {}
        Err(e) if rejected(&e) => {
            if let Err(e) = git(dir, &["pull", "--quiet", "--rebase", "--autostash", "origin", &branch], true) {
                let _ = git(dir, &["rebase", "--abort"], false);
                return Err(format!("Someone else's commit clashes with this one: {}", e));
            }
            git(dir, &["push", "--quiet", "-u", "origin", &branch], true)?;
        }
        Err(e) => return Err(e),
    }
    f.git.lock().unwrap().error.clear();
    moved(f);
    Ok(format!("Pushed to {}", branch))
}

pub fn commit(f: &Folder, message: &str) -> Result<String, String> {
    let _b = Busy::take(f)?;
    let dir = &f.root;
    if !dir.join(".git").exists() {
        return Err("Connect a repository first".into());
    }
    git(dir, &["add", "-A"], false)?;
    if git(dir, &["diff", "--cached", "--quiet"], false).is_ok() {
        return Err("Nothing to commit".into());
    }
    let msg = if message.trim().is_empty() { auto_message(dir) } else { message.trim().to_string() };
    if let Err(e) = git(dir, &["commit", "--quiet", "-m", &msg], false) {
        if e.contains("tell me who you are") || e.contains("user.email") || e.contains("user.name") {
            return Err("Set your name and email for commits first".into());
        }
        return Err(e);
    }
    let remote = git(dir, &["remote", "get-url", "origin"], false).unwrap_or_default();
    if remote.is_empty() {
        refresh(f);
        return Ok("Committed on this laptop".into());
    }
    push_now(f).map(|p| format!("Committed. {}", p)).map_err(|e| format!("Committed here, but not pushed: {}", e))
}

fn auto_message(dir: &Path) -> String {
    let names = git(dir, &["diff", "--cached", "--name-only"], false).unwrap_or_default();
    let files: Vec<&str> = names.lines().map(|l| l.rsplit('/').next().unwrap_or(l)).collect();
    match files.len() {
        0 => "Update".into(),
        1 => format!("Update {}", files[0]),
        2 => format!("Update {} and {}", files[0], files[1]),
        n => format!("Update {}, {} and {} more", files[0], files[1], n - 2),
    }
}

pub fn push(f: &Folder) -> Result<String, String> {
    let _b = Busy::take(f)?;
    push_now(f)
}

pub fn pull(f: &Folder) -> Result<String, String> {
    let _b = Busy::take(f)?;
    git(&f.root, &["pull", "--quiet", "--rebase", "--autostash"], true)?;
    f.git.lock().unwrap().error.clear();
    moved(f);
    Ok("Up to date with GitHub".into())
}

pub fn identity(f: &Folder) -> (String, String) {
    let dir = &f.root;
    let get = |k: &str| {
        let r = if dir.join(".git").exists() { git(dir, &["config", k], false) } else { git(Path::new("."), &["config", "--global", k], false) };
        r.unwrap_or_default()
    };
    (get("user.name"), get("user.email"))
}

pub fn set_identity(f: &Folder, name: &str, email: &str) -> Result<(), String> {
    let dir = &f.root;
    git(dir, &["config", "user.name", name.trim()], false)?;
    git(dir, &["config", "user.email", email.trim()], false)?;
    Ok(())
}

/// The state for the page, with the files changed since the last commit and who changed each.
pub fn status(f: &Folder) -> Value {
    let g = f.git.lock().unwrap().clone();
    let (repo_name, repo_web) = web(&g.remote);
    let mut changes = Vec::new();
    if g.repo {
        if let Ok(out) = git(&f.root, &["status", "--porcelain=v1", "-z", "-uall"], false) {
            let by = f.idx.lock().unwrap().changed_by();
            let mut it = out.split('\0').filter(|s| !s.is_empty());
            while let Some(item) = it.next() {
                if item.len() < 4 {
                    continue;
                }
                let (xy, path) = item.split_at(3);
                let xy = xy.trim();
                if xy.starts_with('R') || xy.starts_with('C') {
                    it.next();
                }
                let kind = if xy.contains('?') || xy.contains('A') {
                    "new"
                } else if xy.contains('D') {
                    "deleted"
                } else if xy.contains('R') {
                    "renamed"
                } else {
                    "edited"
                };
                let who = by.get(path).and_then(|v| v.as_str()).map(|id| app().name_of(id)).unwrap_or_default();
                changes.push(json!({"path": path, "kind": kind, "by": who}));
            }
        }
    }
    let (name, email) = identity(f);
    json!({
        "repo": g.repo, "remote": g.remote, "repo_name": repo_name, "repo_web": repo_web,
        "branch": g.branch, "head": g.head, "ahead": g.ahead, "behind": g.behind, "upstream": g.upstream,
        "error": g.error, "suggest": if g.repo { String::new() } else { g.suggest.clone() }, "no_git": g.no_git,
        "busy": f.git_busy.load(Ordering::SeqCst), "changes": changes,
        "identity": {"name": name, "email": email},
    })
}

pub fn log(f: &Folder) -> Value {
    let g = f.git.lock().unwrap().clone();
    if !g.repo || g.head.is_empty() {
        return json!({"commits": []});
    }
    let out = git(&f.root, &["log", "-n", "300", "--format=%H%x1f%an%x1f%ae%x1f%at%x1f%s%x1e"], false).unwrap_or_default();
    let commits: Vec<Value> = out
        .split('\x1e')
        .map(|r| r.trim_matches(|c| c == '\n' || c == '\r'))
        .filter(|r| !r.is_empty())
        .enumerate()
        .filter_map(|(i, r)| {
            let p: Vec<&str> = r.split('\x1f').collect();
            if p.len() < 5 {
                return None;
            }
            Some(json!({"hash": p[0], "author": p[1], "email": p[2], "at": p[3].parse::<u64>().unwrap_or(0) * 1000,
                        "subject": p[4], "local": (i as u32) < g.ahead}))
        })
        .collect();
    let (_, repo_web) = web(&g.remote);
    json!({"commits": commits, "repo_web": repo_web})
}

fn is_hash(c: &str) -> bool {
    (4..=64).contains(&c.len()) && c.chars().all(|x| x.is_ascii_hexdigit())
}

pub fn show(f: &Folder, c: &str) -> Value {
    if !is_hash(c) {
        return json!({"files": []});
    }
    let dir = &f.root;
    let body = git(dir, &["show", "-s", "--format=%B", c], false).unwrap_or_default();
    let names = git(dir, &["show", "--no-renames", "--format=", "--name-status", c], false).unwrap_or_default();
    let nums = git(dir, &["show", "--no-renames", "--format=", "--numstat", c], false).unwrap_or_default();
    let nums: Vec<(String, String)> = nums
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '\t');
            Some((p.next()?.to_string(), p.next()?.to_string()))
        })
        .collect();
    let files: Vec<Value> = names
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .enumerate()
        .map(|(i, (k, path))| {
            let (a, d) = nums.get(i).cloned().unwrap_or_default();
            json!({"path": path, "kind": match &k[..1] { "A" => "new", "D" => "deleted", _ => "edited" }, "added": a, "removed": d})
        })
        .collect();
    json!({"message": body, "files": files})
}

pub fn diff(f: &Folder, c: &str, path: &str) -> Value {
    if !is_hash(c) || !crate::index::safe_path(path) {
        return json!({"diff": ""});
    }
    let mut d = git(&f.root, &["show", "--no-renames", "--format=", c, "--", path], false).unwrap_or_default();
    if d.len() > 200_000 {
        let mut cut = 200_000;
        while !d.is_char_boundary(cut) {
            cut -= 1;
        }
        d.truncate(cut);
        d.push_str("\n…");
    }
    json!({"diff": d})
}
