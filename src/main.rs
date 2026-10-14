//! Folder Sync: folders kept the same on every laptop on the Wi-Fi, laptop to laptop with nothing
//! in between, and a GitHub repository behind each one that anyone commits to with one button.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod api;
mod config;
mod git;
mod http;
mod ignore;
mod index;
mod peers;
mod scan;
mod state;
mod sync;
mod util;
mod window;

use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::time::Duration;

use state::app;

pub const DEFAULT_PORT: u16 = 47800;
const DISC_PORT: u16 = 47801;

/// A path inside the folder, from the / form used everywhere else.
pub fn folder_path(root: &Path, rel: &str) -> PathBuf {
    let mut p = root.to_path_buf();
    for part in rel.split('/') {
        p.push(part);
    }
    p
}

fn data_dir() -> PathBuf {
    #[cfg(windows)]
    if let Ok(a) = std::env::var("APPDATA") {
        return PathBuf::from(a).join("FolderSync");
    }
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()));
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/FolderSync")
    } else {
        home.join(".config/foldersync")
    }
}

fn main() {
    // --data DIR, --port N, --disc N, --disc-send N,N (to run two on one laptop), --no-window
    let args: Vec<String> = std::env::args().collect();
    let arg = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned();
    let data = arg("--data").map(PathBuf::from).unwrap_or_else(data_dir);
    let port: u16 = arg("--port").and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PORT);
    let disc: u16 = arg("--disc").and_then(|p| p.parse().ok()).unwrap_or(DISC_PORT);
    let disc_send: Vec<u16> =
        arg("--disc-send").map(|s| s.split(',').filter_map(|x| x.parse().ok()).collect()).unwrap_or_else(|| vec![disc]);
    let no_window = args.iter().any(|a| a == "--no-window");

    let _ = std::fs::create_dir_all(&data);
    util::open_log(&data.join("log.txt"));

    let listener = match TcpListener::bind(("0.0.0.0", port)) {
        Ok(l) => l,
        Err(_) => {
            // Already running: show its window.
            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            let running = http::request("GET", addr, "/api/ping", &[], b"", Duration::from_secs(2))
                .and_then(|r| r.json())
                .map(|v| v.get("app").and_then(|x| x.as_str()) == Some(peers::APP_TAG))
                .unwrap_or(false);
            if running && !no_window {
                state::init(data, port, disc, disc_send);
                window::open_window(port);
            } else if !running {
                log!("port {} is taken by another program", port);
            }
            return;
        }
    };
    state::init(data, port, disc, disc_send);
    log!("Folder Sync {} as {} on port {}", env!("CARGO_PKG_VERSION"), app().cfg().device_name, port);
    let folders = app().cfg().folders.clone();
    for fc in &folders {
        app().open_folder(fc);
    }
    peers::start();
    if !no_window {
        window::open_window(port);
    }
    http::serve(listener, api::handle);
}
