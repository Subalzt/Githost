# Folder Sync

Folders kept the same on every laptop on the same Wi-Fi, laptop to laptop with nothing in
between, and a GitHub repository behind each one that anyone commits to with one button.

- **Share a folder** and **Invite**: your pals type the 4-digit code (`4821`) under **Join with a code**.
  Every save on one laptop shows up on the others within a second or two: new files, edits,
  deletes, moves, whole folders.
- **Connect a GitHub repository** once, on any laptop. The others connect to it by themselves.
- **Commit and push** from any laptop. Everyone's history moves with it, nobody pulls.
- **History**: every commit with who made it and when, the files it changed, and their diffs.
- **Changes**: what is not committed yet, and which laptop changed each file.

## Getting started

1. Run `folder-sync.exe`. It opens its own window (Edge or Chrome in app mode).
2. Windows asks whether it may use the network: allow it on **Private networks**. Without that,
   other laptops cannot reach this one.
3. **New shared folder**, choose the folder, and read out the 4-digit code it shows. On the other
   laptops: **Join with a code**, type it, and choose an empty folder to keep it in. The code works
   for 10 minutes, for as many laptops as you give it to; **Invite** makes a new one later.
4. In **Changes**, paste the repository (`owner/repo` is enough) and **Connect**. The first push
   opens GitHub's own sign-in (Git Credential Manager, which comes with Git for Windows). Each pal
   needs to be a collaborator on the repository to push.

Closing the window keeps the folders syncing. Opening the app again brings the window back;
**Settings → Quit** stops it.

Git must be installed ([git-scm.com](https://git-scm.com/downloads)) for the repository part;
syncing works without it.

## What travels

Everything in the folder except what its `.gitignore` or `.syncignore` (at the top of the folder)
lists, and except `.git` itself, which each laptop keeps for its own. So `node_modules/`, build
output and logs stay put when the repository ignores them.

When two laptops change the same file before either has the other's change, the later edit keeps
the name and the other is kept beside it as `name (LAPTOP's copy).ext`, on both laptops. A file
edited on one laptop and deleted on another is kept.

## How it works

- Laptops find each other by UDP broadcast on port 47801 every two seconds; the app listens on
  TCP 47800. On networks that keep laptops apart (some guest and hotspot networks), add the
  other laptop by address in **Settings**.
- Each folder has a random 256-bit key, and every request between laptops is signed with it
  (HMAC-SHA256), so only laptops that joined can read or change the folder. A laptop joins by
  proving it was told the invite's code through SPAKE2 (over Ristretto): each try tests one guess,
  nothing seen on the network lets anyone test more, and five tries that do not finish close the
  invite. Only then is the folder's key passed across, hidden under the handshake's own key. Folders
  shared with a 12-letter code before 0.3 keep working. The files themselves cross the
  network unencrypted, as on a file share; use it on networks you trust.
- Each laptop keeps an index of the folder: a SHA-256 hash and a version vector per file. Every
  second each laptop asks the others what changed since it last asked, takes what is newer, and
  checks every file's hash before putting it in place. A file that moved is copied from where it
  is now rather than sent again.
- Every laptop has its own `.git`; only the files travel. The laptop that commits (or pulls) tells
  the others which commit it is on, and they move their branch to it with `git reset --mixed`,
  which changes no file, since their files are already the same. A laptop with commits the others
  do not have is never moved.

## Limits

- Same network only (or one reachable by address).
- `.gitignore` files inside subfolders are not read, only the top one.
- Empty folders and symbolic links do not travel.
- Built and tested on Windows; the code builds for macOS and Linux (the window uses Chrome or
  Edge there too) but has not been tried on them.

## Building

```bash
cargo build --release
```

The app is `target/release/folder-sync.exe`. For trying two on one laptop:
`--data DIR --port N --disc N --disc-send N,M --no-window`.
