//! Which files stay on each laptop: the folder's own .gitignore and .syncignore (at its top), plus
//! a few that never travel (the repository itself, this app's own scratch folder, system litter).

use std::fs;
use std::path::Path;

pub const SCRATCH: &str = ".foldersync-tmp";

const ALWAYS: &[&str] = &[".git", SCRATCH, ".DS_Store", "Thumbs.db", "desktop.ini", "~$*", ".~lock.*#"];

struct Rule {
    pat: String,
    anchored: bool,
    dir_only: bool,
    negate: bool,
}

pub struct Rules {
    rules: Vec<Rule>,
}

impl Rules {
    pub fn load(root: &Path) -> Rules {
        let mut rules = Vec::new();
        for name in [".gitignore", ".syncignore"] {
            let Ok(text) = fs::read_to_string(root.join(name)) else { continue };
            for line in text.lines() {
                let mut l = line.trim_end();
                if l.is_empty() || l.starts_with('#') {
                    continue;
                }
                let negate = l.starts_with('!');
                if negate {
                    l = &l[1..];
                }
                let dir_only = l.ends_with('/');
                let l = l.trim_end_matches('/');
                let anchored = l.contains('/');
                let l = l.trim_start_matches('/');
                if l.is_empty() {
                    continue;
                }
                rules.push(Rule { pat: l.to_string(), anchored, dir_only, negate });
            }
        }
        Rules { rules }
    }

    /// `rel` uses / between parts.
    pub fn ignored(&self, rel: &str, is_dir: bool) -> bool {
        let base = rel.rsplit('/').next().unwrap_or(rel);
        if ALWAYS.iter().any(|p| glob(p.as_bytes(), base.as_bytes())) {
            return true;
        }
        let mut out = false;
        for r in &self.rules {
            if r.dir_only && !is_dir {
                continue;
            }
            let hit = if r.anchored { glob(r.pat.as_bytes(), rel.as_bytes()) } else { glob(r.pat.as_bytes(), base.as_bytes()) };
            if hit {
                out = !r.negate;
            }
        }
        out
    }
}

fn eq(a: u8, b: u8) -> bool {
    a.eq_ignore_ascii_case(&b)
}

/// A .gitignore pattern: * within a part, ** across parts, ?, and [classes].
pub fn glob(p: &[u8], s: &[u8]) -> bool {
    match p.first() {
        None => s.is_empty(),
        Some(b'*') if p.get(1) == Some(&b'*') => {
            let rest = &p[2..];
            if let Some(rest) = rest.strip_prefix(b"/") {
                if glob(rest, s) {
                    return true;
                }
                (0..s.len()).any(|i| s[i] == b'/' && glob(rest, &s[i + 1..]))
            } else {
                (0..=s.len()).any(|i| glob(rest, &s[i..]))
            }
        }
        Some(b'*') => {
            let rest = &p[1..];
            for i in 0..=s.len() {
                if glob(rest, &s[i..]) {
                    return true;
                }
                if i < s.len() && s[i] == b'/' {
                    break;
                }
            }
            false
        }
        Some(b'?') => !s.is_empty() && s[0] != b'/' && glob(&p[1..], &s[1..]),
        Some(b'[') => {
            let Some(end) = p.iter().skip(2).position(|&c| c == b']').map(|i| i + 2) else {
                return !s.is_empty() && s[0] == b'[' && glob(&p[1..], &s[1..]);
            };
            let Some(&c) = s.first() else { return false };
            let mut class = &p[1..end];
            let neg = matches!(class.first(), Some(b'!') | Some(b'^'));
            if neg {
                class = &class[1..];
            }
            let mut hit = false;
            let mut i = 0;
            while i < class.len() {
                if i + 2 < class.len() && class[i + 1] == b'-' {
                    let (lo, hi) = (class[i].to_ascii_lowercase(), class[i + 2].to_ascii_lowercase());
                    let cl = c.to_ascii_lowercase();
                    hit |= lo <= cl && cl <= hi;
                    i += 3;
                } else {
                    hit |= eq(class[i], c);
                    i += 1;
                }
            }
            hit != neg && c != b'/' && glob(&p[end + 1..], &s[1..])
        }
        Some(b'\\') if p.len() > 1 => !s.is_empty() && eq(s[0], p[1]) && glob(&p[2..], &s[1..]),
        Some(&c) => !s.is_empty() && eq(s[0], c) && glob(&p[1..], &s[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob(b"*.log", b"a.log"));
        assert!(!glob(b"*.log", b"a/b.log"));
        assert!(glob(b"**/x", b"a/b/x"));
        assert!(glob(b"**/x", b"x"));
        assert!(glob(b"a/**", b"a/b/c"));
        assert!(glob(b"a/**/c", b"a/c"));
        assert!(glob(b"a/**/c", b"a/b/d/c"));
        assert!(glob(b"[ab].txt", b"b.txt"));
        assert!(!glob(b"[!ab].txt", b"b.txt"));
        assert!(glob(b"file?.txt", b"file1.txt"));
        assert!(glob(b"~$*", b"~$Doc.docx"));
    }

    #[test]
    fn rules() {
        let dir = std::env::temp_dir().join(format!("fsync-ign-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(".gitignore"), "node_modules/\n*.log\n!keep.log\n/build\ndocs/*.tmp\n").unwrap();
        let r = Rules::load(&dir);
        assert!(r.ignored("node_modules", true));
        assert!(!r.ignored("node_modules", false));
        assert!(r.ignored("x/y.log", false));
        assert!(!r.ignored("keep.log", false));
        assert!(r.ignored("build", true));
        assert!(!r.ignored("src/build", true));
        assert!(r.ignored("docs/a.tmp", false));
        assert!(r.ignored(".git", true));
        assert!(r.ignored("a/Thumbs.db", false));
        assert!(!r.ignored("src/main.rs", false));
        let _ = fs::remove_dir_all(&dir);
    }
}
