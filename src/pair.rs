//! Joining with a 4-digit code. A folder's key is 256 random bits that are never shown; a laptop
//! gets it by proving it was told the code of an open invite, through SPAKE2 over Ristretto: each try
//! tests one guess, and nothing seen on the network lets anyone test more. An invite lasts ten
//! minutes, lets in any number of laptops, and closes after five tries that did not finish.
//! A laptop can have an invite open for each of its folders, each with its own code. A try is
//! answered for all of them at once and the joining laptop finds which one its code fits, so a try
//! is still one guess, and it counts against every invite open.

use std::collections::HashMap;
use std::time::Duration;

use curve25519_dalek::constants::RISTRETTO_BASEPOINT_POINT as G;
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};
use curve25519_dalek::scalar::Scalar;
use serde_json::{json, Value};
use sha2::{Digest, Sha256, Sha512};

use crate::net::{self, Reply};
use crate::store::{app, FolderCfg};
use crate::{hex, hmac, now_ms, random, same, unhex};

pub struct Invite {
    pub folder: String,
    pub code: String,
    pub until: u64,
    tries: u32,
}

#[derive(Default)]
pub struct Pairing {
    invites: Vec<Invite>,
    /// Handshakes under way: a key and folder per invite that was open, and until when they may finish.
    sessions: HashMap<String, (Vec<(Vec<u8>, String)>, u64)>,
}

fn is_open(i: &Invite) -> bool {
    i.until > now_ms() && i.tries < 5
}

pub fn invite_for<'a>(p: &'a Pairing, folder: &str) -> Option<&'a Invite> {
    p.invites.iter().find(|i| i.folder == folder && is_open(i))
}

pub fn inviting(p: &Pairing) -> bool {
    p.invites.iter().any(is_open)
}

fn wide(parts: &[&[u8]]) -> [u8; 64] {
    parts.iter().fold(Sha512::new(), |h, p| h.chain_update(p)).finalize().into()
}

/// The two points SPAKE2 blinds with (nobody knows their logarithms) and the code as a number.
fn setup(code: &str) -> (RistrettoPoint, RistrettoPoint, Scalar) {
    let m = RistrettoPoint::from_uniform_bytes(&wide(&[b"foldersync pake M"]));
    let n = RistrettoPoint::from_uniform_bytes(&wide(&[b"foldersync pake N"]));
    (m, n, Scalar::from_bytes_mod_order_wide(&wide(&[b"foldersync pake w ", code.as_bytes()])))
}

fn random_scalar() -> Scalar {
    Scalar::from_bytes_mod_order_wide(&random(64).try_into().unwrap())
}

fn point(h: &str) -> Option<RistrettoPoint> {
    CompressedRistretto::from_slice(&unhex(h)?).ok()?.decompress()
}

fn session_key(x: &RistrettoPoint, y: &RistrettoPoint, k: &RistrettoPoint, w: &Scalar) -> Vec<u8> {
    let parts = [x.compress().to_bytes(), y.compress().to_bytes(), k.compress().to_bytes(), w.to_bytes()];
    parts.iter().fold(Sha256::new_with_prefix(b"foldersync pake key"), |h, p| h.chain_update(p)).finalize().to_vec()
}

/// The folder's key, hidden under the handshake's own key while it crosses the network.
fn wrap(key: &[u8], session: &[u8]) -> Vec<u8> {
    key.iter().zip(Sha256::new_with_prefix(session).chain_update(b"foldersync wrap").finalize()).map(|(a, b)| a ^ b).collect()
}

/// Opens an invite to a folder, in place of the one it had, with a code no other open invite here has.
pub fn invite(folder: &str) -> Value {
    let mut p = app().pairing.lock().unwrap();
    p.invites.retain(|i| i.folder != folder && is_open(i));
    let code = loop {
        let c = format!("{:04}", u32::from_le_bytes(random(4).try_into().unwrap()) % 10_000);
        if !p.invites.iter().any(|i| i.code == c) {
            break c;
        }
    };
    let until = now_ms() + 600_000;
    p.invites.push(Invite { folder: folder.to_string(), code: code.clone(), until, tries: 0 });
    json!({"code": code, "until": until})
}

/// The inviting laptop's half of the handshake, once for each invite it has open.
pub fn start(x: &str) -> Result<Value, String> {
    let x = point(x).ok_or("Not a point")?;
    let mut p = app().pairing.lock().unwrap();
    let (mut offers, mut keys) = (Vec::new(), Vec::new());
    for inv in p.invites.iter_mut().filter(|i| is_open(i)) {
        inv.tries += 1;
        let ((m, n, w), y) = (setup(&inv.code), random_scalar());
        let big_y = y * G + w * n;
        let key = session_key(&x, &big_y, &(y * (x - w * m)), &w);
        offers.push(json!({"y": hex(big_y.compress().as_bytes()), "confirm": hmac(&key, "B")}));
        keys.push((key, inv.folder.clone()));
    }
    if keys.is_empty() {
        return Err("No invite is open".into());
    }
    let sid = hex(&random(16));
    p.sessions.retain(|_, s| s.1 > now_ms());
    p.sessions.insert(sid.clone(), (keys, now_ms() + 60_000));
    Ok(json!({"sid": sid, "offers": offers}))
}

/// Hands the folder over once the other laptop has shown it has the same key, so the same code.
pub fn finish(sid: &str, confirm: &str) -> Result<Value, String> {
    let mut p = app().pairing.lock().unwrap();
    let (keys, until) = p.sessions.remove(sid).ok_or("No such handshake")?;
    let (key, folder) = keys.into_iter().find(|(k, _)| same(confirm, &hmac(k, "A"))).filter(|_| until > now_ms()).ok_or("Wrong code")?;
    p.invites.iter_mut().for_each(|i| i.tries = i.tries.saturating_sub(1)); // it finished, so it was no guess
    let f = app().folder(&folder).ok_or("That folder is no longer shared")?;
    Ok(json!({"id": f.id, "key": hex(&wrap(&f.key, &key))}))
}

/// The joining laptop's half: tries each laptop with an invite open until one takes the code.
pub fn join(code: &str, path: &str) -> Result<String, String> {
    let code: String = code.chars().filter(char::is_ascii_digit).collect();
    if code.len() != 4 {
        return Err("The code is 4 digits".into());
    }
    let peers: Vec<_> = app().peers.lock().unwrap().values().filter(|p| p.online() && p.inviting).cloned().collect();
    if peers.is_empty() {
        return Err("No laptop on this network is inviting right now".into());
    }
    let (m, n, w) = setup(&code);
    let ask = |p: &net::Peer, target: String| net::request("GET", p.addr, &target, &[], Duration::from_secs(10)).and_then(Reply::json).ok();
    let mut why = "That code does not match; check it with the laptop that invited you";
    for p in peers {
        let x = random_scalar();
        let big_x = x * G + w * m;
        let Some(r) = ask(&p, format!("/p/pair/start?x={}", hex(big_x.compress().as_bytes()))) else { continue };
        if r["error"] == "No invite is open" {
            why = "That invite has closed; ask for a new code";
        }
        // Of the invites open there, the one this code fits, if any.
        let fits = r["offers"].as_array().into_iter().flatten().find_map(|o| {
            let big_y = point(o["y"].as_str()?)?;
            let key = session_key(&big_x, &big_y, &(x * (big_y - w * n)), &w);
            same(o["confirm"].as_str()?, &hmac(&key, "B")).then_some(key)
        });
        let Some(key) = fits else { continue };
        let sid = r["sid"].as_str().unwrap_or("");
        let Some(v) = ask(&p, format!("/p/pair/finish?sid={}&confirm={}", sid, hmac(&key, "A"))) else { continue };
        let (Some(id), Some(wrapped)) = (v["id"].as_str(), v["key"].as_str().and_then(unhex)) else { continue };
        let fc = FolderCfg { id: id.to_string(), key: hex(&wrap(&wrapped, &key)), path: path.to_string(), ..Default::default() };
        return crate::api::add_folder(fc).map(|_| format!("Joined {}'s folder", p.name));
    }
    Err(why.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_code_same_key() {
        let handshake = |a: &str, b: &str| {
            let ((m, n, wa), (_, _, wb)) = (setup(a), setup(b));
            let (x, y) = (random_scalar(), random_scalar());
            let (big_x, big_y) = (x * G + wa * m, y * G + wb * n);
            session_key(&big_x, &big_y, &(x * (big_y - wa * n)), &wa) == session_key(&big_x, &big_y, &(y * (big_x - wb * m)), &wb)
        };
        assert!(handshake("4821", "4821"));
        assert!(!handshake("4821", "4822"));
        assert_eq!(wrap(&wrap(b"0123456789abcdef0123456789abcdef", b"k"), b"k"), b"0123456789abcdef0123456789abcdef");
    }
}
