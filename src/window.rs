//! The app's window (Edge or Chrome in app mode: no tabs, no address bar, its own place on the
//! taskbar) and the system's own folder picker.

use std::path::PathBuf;
use std::process::Stdio;

use crate::log;
use crate::state::app;
use crate::util::quiet_command;

fn browser() -> Option<PathBuf> {
    let mut c: Vec<PathBuf> = Vec::new();
    #[cfg(windows)]
    for var in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Ok(base) = std::env::var(var) {
            let b = PathBuf::from(base);
            c.push(b.join(r"Microsoft\Edge\Application\msedge.exe"));
            c.push(b.join(r"Google\Chrome\Application\chrome.exe"));
            c.push(b.join(r"BraveSoftware\Brave-Browser\Application\brave.exe"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        c.push("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into());
        c.push("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge".into());
        c.push("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser".into());
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    for n in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser", "microsoft-edge", "brave-browser"] {
        for d in ["/usr/bin", "/usr/local/bin", "/snap/bin"] {
            c.push(PathBuf::from(d).join(n));
        }
    }
    c.into_iter().find(|p| p.is_file())
}

pub fn open_window(port: u16) {
    let url = format!("http://127.0.0.1:{}/", port);
    match browser() {
        Some(b) => {
            let profile = app().data.join("window");
            let r = std::process::Command::new(&b)
                .arg(format!("--app={}", url))
                .arg(format!("--user-data-dir={}", profile.display()))
                .arg("--window-size=1180,780")
                .arg("--no-first-run")
                .arg("--no-default-browser-check")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            if let Err(e) = r {
                log!("cannot open {}: {}", b.display(), e);
                let _ = open::that_detached(&url);
            }
        }
        None => {
            let _ = open::that_detached(&url);
        }
    }
}

#[cfg(windows)]
const PICKER: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;
public static class FolderSyncPicker {
  [ComImport, Guid("DC1C5A9C-E88A-4dde-A5A1-60F82A20AEF7")] class FileOpenDialog {}
  [ComImport, Guid("42f85136-db7e-439c-85f1-e4075d135fc8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IFileDialog {
    [PreserveSig] int Show(IntPtr parent);
    void SetFileTypes(uint n, IntPtr specs);
    void SetFileTypeIndex(uint i);
    void GetFileTypeIndex(out uint i);
    void Advise(IntPtr sink, out uint cookie);
    void Unadvise(uint cookie);
    void SetOptions(uint fos);
    void GetOptions(out uint fos);
    void SetDefaultFolder(IShellItem item);
    void SetFolder(IShellItem item);
    void GetFolder(out IShellItem item);
    void GetCurrentSelection(out IShellItem item);
    void SetFileName([MarshalAs(UnmanagedType.LPWStr)] string name);
    void GetFileName([MarshalAs(UnmanagedType.LPWStr)] out string name);
    void SetTitle([MarshalAs(UnmanagedType.LPWStr)] string title);
    void SetOkButtonLabel([MarshalAs(UnmanagedType.LPWStr)] string text);
    void SetFileNameLabel([MarshalAs(UnmanagedType.LPWStr)] string label);
    void GetResult(out IShellItem item);
  }
  [ComImport, Guid("43826D1E-E718-42EE-BC55-A1E261C37BFE"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
  interface IShellItem {
    void BindToHandler(IntPtr pbc, ref Guid bhid, ref Guid riid, out IntPtr ppv);
    void GetParent(out IShellItem item);
    void GetDisplayName(uint sigdn, [MarshalAs(UnmanagedType.LPWStr)] out string name);
  }
  [DllImport("user32.dll")] static extern IntPtr GetForegroundWindow();
  public static string Pick(string title) {
    var d = (IFileDialog)new FileOpenDialog();
    uint o; d.GetOptions(out o); d.SetOptions(o | 0x20 | 0x40);
    if (!String.IsNullOrEmpty(title)) d.SetTitle(title);
    if (d.Show(GetForegroundWindow()) != 0) return "";
    IShellItem item; d.GetResult(out item);
    string path; item.GetDisplayName(0x80058000, out path);
    return path;
  }
}
"@
[FolderSyncPicker]::Pick($env:FS_TITLE)
"#;

/// The system's folder picker, in front of the app's window; None when it was closed.
pub fn pick_folder(title: &str) -> Option<String> {
    #[cfg(windows)]
    let out = {
        let script = app().data.join("pick.ps1");
        std::fs::write(&script, PICKER).ok()?;
        quiet_command("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-STA", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .env("FS_TITLE", title)
            .stdin(Stdio::null())
            .output()
            .ok()?
    };
    #[cfg(target_os = "macos")]
    let out = quiet_command("osascript")
        .arg("-e")
        .arg(format!("POSIX path of (choose folder with prompt \"{}\")", title.replace('"', "")))
        .output()
        .ok()?;
    #[cfg(all(unix, not(target_os = "macos")))]
    let out = quiet_command("zenity").args(["--file-selection", "--directory", "--title", title]).output().ok()?;
    if !out.status.success() {
        let e = String::from_utf8_lossy(&out.stderr);
        if !e.trim().is_empty() {
            log!("folder picker: {}", e.trim());
        }
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().trim_end_matches(['/', '\\']).to_string();
    let p = if p.is_empty() && cfg!(unix) { String::new() } else { p };
    (!p.is_empty()).then_some(p)
}
