//! Discord Rich Presence ("Listening to ..." with title, artist, album and a time bar).
//!
//! Talks to the Discord desktop app over its local IPC: a named pipe `\\.\pipe\discord-ipc-N` on Windows
//! (a Unix socket on other systems, used by the tests). No Tauri dependency. A worker thread owns the
//! connection; the page only says what it wants (`Presence::set`) and the worker connects, reconnects,
//! sends and clears. Frames are: opcode (u32 LE), length (u32 LE), JSON.

use std::io::{self, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

pub trait Conn: Read + Write + Send {}
impl<T: Read + Write + Send> Conn for T {}
pub type Connector = Box<dyn Fn() -> io::Result<Box<dyn Conn>> + Send>;

/// What is playing. `start` / `end` are unix seconds (the time bar); `end` is missing when the length is unknown.
#[derive(Clone, Debug, PartialEq, serde::Deserialize)]
pub struct Activity {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub start: i64,
    pub end: Option<i64>,
    /// address of the cover picture on the internet (Discord cannot show files from this computer)
    #[serde(default)]
    pub cover: Option<String>,
}

#[derive(Clone)]
struct Want {
    client_id: String,
    activity: Option<Activity>,
}

pub struct Presence {
    tx: mpsc::Sender<Want>,
    status: Arc<Mutex<&'static str>>,
}

impl Presence {
    /// `min_gap`: shortest time between two updates sent to Discord (it limits updates); newer requests replace
    /// older ones while waiting. `retry`: how often to look for Discord again while it is not running.
    pub fn start(connector: Connector, min_gap: Duration, retry: Duration) -> Presence {
        let (tx, rx) = mpsc::channel();
        let status = Arc::new(Mutex::new("off"));
        let st = status.clone();
        thread::spawn(move || worker(rx, connector, st, min_gap, retry));
        Presence { tx, status }
    }

    /// An empty `client_id` switches the presence off (the worker disconnects).
    pub fn set(&self, client_id: String, activity: Option<Activity>) {
        let _ = self.tx.send(Want { client_id, activity });
    }

    /// "off", "connected", "no_discord" (Discord is not running) or "bad_id" (the application id is not valid).
    pub fn status(&self) -> &'static str {
        *self.status.lock().unwrap()
    }
}

/// Discord application ids are numbers of about 18 digits.
fn valid_id(id: &str) -> bool {
    (15..=25).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit())
}

/// Discord wants 2 to 128 characters in these fields.
fn pad(s: &str) -> String {
    let mut t: String = s.trim().chars().take(128).collect();
    while t.chars().count() < 2 {
        t.push('\u{200b}');
    }
    t
}

fn write_frame(c: &mut dyn Conn, op: u32, v: &Value) -> io::Result<()> {
    let body = v.to_string();
    let mut buf = Vec::with_capacity(8 + body.len());
    buf.extend_from_slice(&op.to_le_bytes());
    buf.extend_from_slice(&(body.len() as u32).to_le_bytes());
    buf.extend_from_slice(body.as_bytes());
    c.write_all(&buf)?;
    c.flush()
}

fn read_frame(c: &mut dyn Conn) -> io::Result<(u32, Value)> {
    let mut head = [0u8; 8];
    c.read_exact(&mut head)?;
    let op = u32::from_le_bytes([head[0], head[1], head[2], head[3]]);
    let len = u32::from_le_bytes([head[4], head[5], head[6], head[7]]) as usize;
    if len > 64 * 1024 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too big"));
    }
    let mut body = vec![0u8; len];
    c.read_exact(&mut body)?;
    Ok((op, serde_json::from_slice(&body).unwrap_or(Value::Null)))
}

enum Fail {
    /// Discord refused the application id
    BadId,
    Io,
}

fn handshake(c: &mut dyn Conn, client_id: &str) -> Result<(), Fail> {
    write_frame(c, 0, &json!({"v": 1, "client_id": client_id})).map_err(|_| Fail::Io)?;
    let (op, v) = read_frame(c).map_err(|_| Fail::Io)?;
    if op == 2 || v["evt"] == "ERROR" {
        return Err(Fail::BadId);
    }
    if v["evt"] == "READY" {
        Ok(())
    } else {
        Err(Fail::Io)
    }
}

fn activity_json(a: &Activity) -> Value {
    let mut out = json!({
        "type": 2, // "Listening to ...", like Spotify
        "details": pad(&a.title),
        "state": pad(if a.artist.trim().is_empty() { "Unknown artist" } else { &a.artist }),
    });
    // the cover when there is one; else "icon", the optional picture the owner uploads in the Discord developer portal (Rich Presence > Art Assets)
    let image = match &a.cover {
        Some(u) if u.starts_with("https://") && u.len() <= 300 => u.as_str(),
        _ => "icon",
    };
    let mut assets = json!({"large_image": image});
    if !a.album.trim().is_empty() {
        assets["large_text"] = json!(pad(&a.album));
    }
    out["assets"] = assets;
    let mut ts = json!({"start": a.start});
    if let Some(end) = a.end {
        if end > a.start {
            ts["end"] = json!(end);
        }
    }
    out["timestamps"] = ts;
    out
}

fn send_activity(c: &mut dyn Conn, a: Option<&Activity>, nonce: u64) -> io::Result<()> {
    let msg = json!({
        "cmd": "SET_ACTIVITY",
        "args": {"pid": std::process::id(), "activity": a.map(activity_json)},
        "nonce": nonce.to_string(),
    });
    write_frame(c, 1, &msg)?;
    // Discord answers every command: read it so the pipe never fills up. An ERROR answer (for example a
    // field it did not like) is not a broken connection, so it is ignored.
    read_frame(c).map(|_| ())
}

struct Worker {
    connector: Connector,
    status: Arc<Mutex<&'static str>>,
    conn: Option<(Box<dyn Conn>, String)>,
    bad: Option<String>,
    sent: Option<(String, Option<Activity>)>,
    last: Option<Instant>,
    nonce: u64,
}

impl Worker {
    fn set_status(&self, s: &'static str) {
        *self.status.lock().unwrap() = s;
    }

    fn sync(&mut self, want: &Want) {
        if want.client_id.is_empty() {
            self.conn = None;
            self.sent = None;
            self.bad = None;
            return self.set_status("off");
        }
        if !valid_id(&want.client_id) {
            self.conn = None;
            self.sent = None;
            return self.set_status("bad_id");
        }
        if matches!(&self.conn, Some((_, id)) if *id != want.client_id) {
            self.conn = None;
            self.sent = None;
        }
        if self.bad.as_deref().is_some_and(|b| b != want.client_id) {
            self.bad = None;
        }
        if self.bad.is_some() {
            return self.set_status("bad_id");
        }
        if self.conn.is_none() {
            let mut c = match (self.connector)() {
                Ok(c) => c,
                Err(_) => return self.set_status("no_discord"),
            };
            match handshake(&mut *c, &want.client_id) {
                Ok(()) => {
                    self.conn = Some((c, want.client_id.clone()));
                    self.sent = None;
                }
                Err(Fail::BadId) => {
                    self.bad = Some(want.client_id.clone());
                    return self.set_status("bad_id");
                }
                Err(Fail::Io) => return self.set_status("no_discord"),
            }
        }
        self.set_status("connected");
        let wanted = (want.client_id.clone(), want.activity.clone());
        if self.sent.as_ref() == Some(&wanted) {
            return;
        }
        self.nonce += 1;
        let nonce = self.nonce;
        let Some((c, _)) = self.conn.as_mut() else { return };
        match send_activity(&mut **c, want.activity.as_ref(), nonce) {
            Ok(()) => {
                self.sent = Some(wanted);
                self.last = Some(Instant::now());
            }
            Err(_) => {
                self.conn = None;
                self.sent = None;
                self.set_status("no_discord");
            }
        }
    }
}

fn worker(rx: mpsc::Receiver<Want>, connector: Connector, status: Arc<Mutex<&'static str>>, min_gap: Duration, retry: Duration) {
    let mut w = Worker { connector, status, conn: None, bad: None, sent: None, last: None, nonce: 0 };
    let mut want = Want { client_id: String::new(), activity: None };
    loop {
        // while Discord is missing (or the connection broke) look for it again from time to time
        let searching = !want.client_id.is_empty() && w.conn.is_none() && w.bad.is_none() && valid_id(&want.client_id);
        let msg = if searching {
            match rx.recv_timeout(retry) {
                Ok(m) => Some(m),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            }
        };
        if let Some(m) = msg {
            want = m;
        }
        // Discord limits how often the presence may change: wait out the gap, keeping only the newest request
        if let (Some(_), Some(last)) = (&w.conn, w.last) {
            let end = last + min_gap;
            loop {
                let left = end.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                match rx.recv_timeout(left) {
                    Ok(m) => want = m,
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        }
        w.sync(&want);
    }
}

/// Connects to the Discord desktop app running on this computer.
#[cfg(windows)]
pub fn system_connector() -> Connector {
    Box::new(|| {
        for i in 0..10 {
            if let Ok(f) = std::fs::OpenOptions::new().read(true).write(true).open(format!(r"\\.\pipe\discord-ipc-{i}")) {
                return Ok(Box::new(f) as Box<dyn Conn>);
            }
        }
        Err(io::Error::new(io::ErrorKind::NotFound, "Discord is not running"))
    })
}

#[cfg(not(windows))]
pub fn system_connector() -> Connector {
    Box::new(|| Err(io::Error::new(io::ErrorKind::NotFound, "Discord presence is only built for Windows")))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};

    const ID: &str = "123456789012345678";

    /// A tiny stand-in for the Discord app: answers the handshake and every command, records what it receives.
    struct Fake {
        path: PathBuf,
        got: Arc<Mutex<Vec<(u32, Value)>>>,
        conns: Arc<Mutex<u32>>,
        /// when set, the fake closes the connection after answering the next command (Discord quits or restarts)
        drop_next: Arc<AtomicBool>,
    }

    fn fake(name: &str, refuse_id: bool) -> Fake {
        let path = std::env::temp_dir().join(format!("mp-discord-{}-{}.sock", std::process::id(), name));
        let _ = std::fs::remove_file(&path);
        let l = UnixListener::bind(&path).unwrap();
        let got = Arc::new(Mutex::new(Vec::new()));
        let conns = Arc::new(Mutex::new(0u32));
        let drop_next = Arc::new(AtomicBool::new(false));
        let (g, cn, dn) = (got.clone(), conns.clone(), drop_next.clone());
        thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { break };
                *cn.lock().unwrap() += 1;
                let (g, dn) = (g.clone(), dn.clone());
                thread::spawn(move || {
                    let Ok((op, v)) = read_frame(&mut s) else { return };
                    g.lock().unwrap().push((op, v));
                    if refuse_id {
                        let _ = write_frame(&mut s, 2, &json!({"code": 4000, "message": "Invalid Client ID"}));
                        return;
                    }
                    let _ = write_frame(&mut s, 1, &json!({"cmd": "DISPATCH", "evt": "READY", "data": {}}));
                    while let Ok((op, v)) = read_frame(&mut s) {
                        let nonce = v["nonce"].clone();
                        g.lock().unwrap().push((op, v));
                        if write_frame(&mut s, 1, &json!({"cmd": "SET_ACTIVITY", "evt": null, "nonce": nonce})).is_err() || dn.swap(false, Ordering::SeqCst) {
                            return;
                        }
                    }
                });
            }
        });
        Fake { path, got, conns, drop_next }
    }

    fn connector_to(path: PathBuf) -> Connector {
        Box::new(move || UnixStream::connect(&path).map(|s| Box::new(s) as Box<dyn Conn>))
    }

    fn until(f: impl Fn() -> bool) -> bool {
        for _ in 0..200 {
            if f() {
                return true;
            }
            thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn song(title: &str) -> Activity {
        Activity { title: title.into(), artist: "Kendrick Lamar".into(), album: "Mr. Morale & the Big Steppers".into(), start: 1000, end: Some(1200), cover: None }
    }

    fn activities(f: &Fake) -> Vec<Value> {
        f.got.lock().unwrap().iter().filter(|(op, v)| *op == 1 && v["cmd"] == "SET_ACTIVITY").map(|(_, v)| v.clone()).collect()
    }

    #[test]
    fn sends_a_listening_activity() {
        let f = fake("a", false);
        let p = Presence::start(connector_to(f.path.clone()), Duration::ZERO, Duration::from_millis(20));
        p.set(ID.into(), Some(song("Prayer")));
        assert!(until(|| activities(&f).len() == 1));
        assert_eq!(p.status(), "connected");
        let got = f.got.lock().unwrap();
        assert_eq!(got[0].0, 0);
        assert_eq!(got[0].1, json!({"v": 1, "client_id": ID}));
        drop(got);
        let a = &activities(&f)[0]["args"];
        assert_eq!(a["pid"], std::process::id());
        let act = &a["activity"];
        assert_eq!(act["type"], 2);
        assert_eq!(act["details"], "Prayer");
        assert_eq!(act["state"], "Kendrick Lamar");
        assert_eq!(act["assets"]["large_text"], "Mr. Morale & the Big Steppers");
        assert_eq!(act["timestamps"], json!({"start": 1000, "end": 1200}));
        assert_eq!(act["assets"]["large_image"], "icon", "no cover: the default picture");
    }

    #[test]
    fn cover_address_is_used_only_when_it_is_https() {
        let mut a = song("Prayer");
        a.cover = Some("https://is1-ssl.mzstatic.com/x/512x512bb.jpg".into());
        assert_eq!(activity_json(&a)["assets"]["large_image"], "https://is1-ssl.mzstatic.com/x/512x512bb.jpg");
        for bad in ["http://x/a.jpg", "file:///C:/a.jpg", "", &format!("https://{}", "a".repeat(400))] {
            a.cover = Some(bad.into());
            assert_eq!(activity_json(&a)["assets"]["large_image"], "icon", "{}", bad);
        }
    }

    #[test]
    fn clears_and_does_not_repeat_itself() {
        let f = fake("b", false);
        let p = Presence::start(connector_to(f.path.clone()), Duration::ZERO, Duration::from_millis(20));
        p.set(ID.into(), Some(song("Prayer")));
        p.set(ID.into(), Some(song("Prayer"))); // same thing twice: sent once
        assert!(until(|| activities(&f).len() == 1));
        thread::sleep(Duration::from_millis(100));
        assert_eq!(activities(&f).len(), 1);
        p.set(ID.into(), None);
        assert!(until(|| activities(&f).len() == 2));
        assert!(activities(&f)[1]["args"]["activity"].is_null());
        assert_eq!(p.status(), "connected");
        assert_eq!(*f.conns.lock().unwrap(), 1, "one connection is reused");
    }

    #[test]
    fn bad_application_id() {
        let f = fake("c", true);
        let p = Presence::start(connector_to(f.path.clone()), Duration::ZERO, Duration::from_millis(20));
        p.set(ID.into(), Some(song("Prayer")));
        assert!(until(|| p.status() == "bad_id"));
        thread::sleep(Duration::from_millis(150));
        assert_eq!(*f.conns.lock().unwrap(), 1, "no reconnect loop with an id Discord refused");
        // not even a number: Discord is not contacted at all
        p.set("abc".into(), Some(song("Prayer")));
        assert!(until(|| p.status() == "bad_id"));
        assert_eq!(*f.conns.lock().unwrap(), 1);
    }

    #[test]
    fn off_when_the_id_is_empty() {
        let f = fake("d", false);
        let p = Presence::start(connector_to(f.path.clone()), Duration::ZERO, Duration::from_millis(20));
        assert_eq!(p.status(), "off");
        p.set(ID.into(), None);
        assert!(until(|| p.status() == "connected"));
        p.set(String::new(), Some(song("Prayer")));
        assert!(until(|| p.status() == "off"));
        assert_eq!(activities(&f).len(), 1, "only the first (empty) activity was sent");
    }

    #[test]
    fn finds_discord_when_it_starts_later() {
        let path = std::env::temp_dir().join(format!("mp-discord-{}-late.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let p = Presence::start(connector_to(path.clone()), Duration::ZERO, Duration::from_millis(20));
        p.set(ID.into(), Some(song("Prayer")));
        assert!(until(|| p.status() == "no_discord"));
        let f = fake("late", false); // same path as above: Discord starts now
        assert_eq!(f.path, path);
        assert!(until(|| activities(&f).len() == 1), "the activity arrives by itself once Discord is there");
        assert_eq!(p.status(), "connected");
    }

    #[test]
    fn reconnects_after_discord_restarts() {
        let f = fake("g", false);
        let p = Presence::start(connector_to(f.path.clone()), Duration::ZERO, Duration::from_millis(20));
        p.set(ID.into(), Some(song("One")));
        assert!(until(|| activities(&f).len() == 1));
        f.drop_next.store(true, Ordering::SeqCst);
        p.set(ID.into(), Some(song("Two"))); // answered, then the fake hangs up
        assert!(until(|| activities(&f).len() == 2));
        p.set(ID.into(), Some(song("Three"))); // goes to the dead connection, the worker notices and reconnects
        assert!(until(|| *f.conns.lock().unwrap() == 2 && activities(&f).iter().any(|a| a["args"]["activity"]["details"] == "Three")));
        assert!(until(|| p.status() == "connected"));
    }

    #[test]
    fn newest_request_wins_inside_the_gap() {
        let f = fake("e", false);
        let p = Presence::start(connector_to(f.path.clone()), Duration::from_millis(300), Duration::from_millis(20));
        p.set(ID.into(), Some(song("One")));
        assert!(until(|| activities(&f).len() == 1));
        p.set(ID.into(), Some(song("Two")));
        p.set(ID.into(), Some(song("Three")));
        assert!(until(|| activities(&f).len() == 2));
        thread::sleep(Duration::from_millis(400));
        let a = activities(&f);
        assert_eq!(a.len(), 2, "Two was skipped");
        assert_eq!(a[1]["args"]["activity"]["details"], "Three");
    }

    #[test]
    fn small_helpers() {
        assert!(valid_id(ID) && !valid_id("") && !valid_id("12345") && !valid_id("12345678901234567x"));
        assert_eq!(pad("a").chars().count(), 2);
        assert_eq!(pad("").chars().count(), 2);
        assert_eq!(pad(&"x".repeat(300)).chars().count(), 128);
        let a = Activity { title: "T".into(), artist: " ".into(), album: "".into(), start: 5, end: None, cover: None };
        let j = activity_json(&a);
        assert_eq!(j["state"], "Unknown artist");
        assert!(j["assets"].get("large_text").is_none());
        assert_eq!(j["timestamps"], json!({"start": 5}));
        let b = Activity { end: Some(3), ..a };
        assert_eq!(activity_json(&b)["timestamps"], json!({"start": 5}), "an end before the start is dropped");
    }
}
