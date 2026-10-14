//! What this laptop remembers: who it is, the folders it shares, and laptops added by address.
//! A folder is known by its code: everyone who types the same code shares the same folder, and
//! the folder's id and the key its requests are signed with both come from the code.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::util::{hex, random_bytes, sha256};

/// Letters and digits that cannot be mistaken for one another (no 0/O, 1/I).
const ALPHABET: &[u8] = b"23456789ABCDEFGHJKLMNPQRSTUVWXYZ";

#[derive(Clone)]
pub struct FolderCfg {
    pub code: String,
    pub path: String,
}

impl FolderCfg {
    pub fn id(&self) -> String {
        folder_id(&self.code)
    }
}

pub struct Config {
    pub device_id: String,
    pub device_name: String,
    pub folders: Vec<FolderCfg>,
    pub manual_peers: Vec<String>,
    file: PathBuf,
}

pub fn new_code() -> String {
    random_bytes(12).iter().map(|b| ALPHABET[(*b % 32) as usize] as char).collect()
}

/// The code as typed, in its plain form, or None when it is not a code.
pub fn normalize_code(s: &str) -> Option<String> {
    let c: String = s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect();
    (c.len() == 12 && c.bytes().all(|b| ALPHABET.contains(&b))).then_some(c)
}

/// The code as shown: ABCD-EFGH-JKLM.
pub fn pretty_code(c: &str) -> String {
    c.as_bytes().chunks(4).map(|ch| String::from_utf8_lossy(ch).into_owned()).collect::<Vec<_>>().join("-")
}

pub fn folder_id(code: &str) -> String {
    hex(&sha256(format!("foldersync id {}", code).as_bytes()))[..24].to_string()
}

pub fn folder_key(code: &str) -> Vec<u8> {
    sha256(format!("foldersync key {}", code).as_bytes()).to_vec()
}

fn default_name() -> String {
    for k in ["COMPUTERNAME", "HOSTNAME"] {
        if let Ok(v) = std::env::var(k) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    crate::util::quiet_command("hostname")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Laptop".into())
}

impl Config {
    pub fn load(dir: &Path) -> Config {
        let file = dir.join("config.json");
        let v: Value = fs::read(&file).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null);
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let mut c = Config {
            device_id: s("device_id"),
            device_name: s("device_name"),
            folders: v
                .get("folders")
                .and_then(|f| f.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|f| {
                            Some(FolderCfg {
                                code: f.get("code")?.as_str()?.to_string(),
                                path: f.get("path")?.as_str()?.to_string(),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
            manual_peers: v
                .get("manual_peers")
                .and_then(|f| f.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default(),
            file,
        };
        if c.device_id.is_empty() {
            c.device_id = hex(&random_bytes(8));
        }
        if c.device_name.is_empty() {
            c.device_name = default_name();
        }
        c.save();
        c
    }

    pub fn save(&self) {
        let v = json!({
            "device_id": self.device_id,
            "device_name": self.device_name,
            "folders": self.folders.iter().map(|f| json!({"code": f.code, "path": f.path})).collect::<Vec<_>>(),
            "manual_peers": self.manual_peers,
        });
        let tmp = self.file.with_extension("tmp");
        if fs::write(&tmp, serde_json::to_vec_pretty(&v).unwrap()).is_ok() {
            let _ = fs::rename(&tmp, &self.file);
        }
    }
}
