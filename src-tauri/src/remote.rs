//! Phone remote: a tiny web server on the local network.
//!
//! The desktop app (the webview) stays the player. The phone opens a web page served from here,
//! sends commands (play, next, seek, ...) and uploads songs. Nothing in this file depends on Tauri,
//! so it can be tested on its own.
//!
//! Every URL starts with `/r/<token>/`. A wrong token gets an empty 404, so the server looks
//! like it does not exist to anyone who does not hold the link (the QR code carries it).
//!
//! Data flow
//! * webview -> server: `push_state`, `push_library`, `push_cover`, `push_notice`
//! * phone  -> server -> webview: commands are queued; the webview takes them with `poll`
//! * phone  -> server: an uploaded file is saved in `incoming/`, then the webview is told to import it

use std::collections::VecDeque;
use std::fs;
use std::io::{BufWriter, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use tiny_http::{Header, Method, Request, Response, Server};

const PAGE: &str = include_str!("remote.html");
/// Largest song accepted from the phone (upload or link).
pub const MAX_UPLOAD: u64 = 300 * 1024 * 1024;
const MAX_CMD_BYTES: u64 = 4096;
const MAX_QUEUED_CMDS: usize = 200;
const MAX_HANDLERS: usize = 48;
/// How long the state request waits for news. Short on purpose: when the server is stopped, a phone that kept its
/// connection open is stuck until its own timeout (see remote.html), so a quick re-ask keeps that delay small.
const LONG_POLL: Duration = Duration::from_secs(5);
const AUDIO_EXT: [&str; 10] = ["mp3", "m4a", "flac", "wav", "ogg", "oga", "opus", "aac", "webm", "wma"];

pub struct Config {
    /// First port to try (the next 9 are tried too). 0 = let the system choose.
    pub port: u16,
    pub token: String,
    /// Folder where uploaded files wait until the app imports them.
    pub incoming: PathBuf,
}

struct Inner {
    ver: u64,
    state: String,
    lib: String,
    lib_ver: u64,
    cover: Vec<u8>,
    cover_ver: u64,
    notices: VecDeque<(u64, String)>,
    notice_seq: u64,
    cmds: VecDeque<String>,
}

struct Shared {
    token: String,
    incoming: PathBuf,
    stop: AtomicBool,
    /// true once the accept loop has ended and the port is free again
    closed: AtomicBool,
    handlers: AtomicUsize,
    file_seq: AtomicU64,
    inner: Mutex<Inner>,
    cv: Condvar,
}

pub struct Remote {
    shared: Arc<Shared>,
    port: u16,
}

impl Remote {
    pub fn start(cfg: Config) -> Result<Remote, String> {
        if cfg.token.len() < 16 || !cfg.token.chars().all(|c| c.is_ascii_alphanumeric()) {
            return Err("invalid token".into());
        }
        fs::create_dir_all(&cfg.incoming).map_err(|e| e.to_string())?;
        // leftovers of interrupted uploads
        if let Ok(rd) = fs::read_dir(&cfg.incoming) {
            for e in rd.flatten() {
                let _ = fs::remove_file(e.path());
            }
        }
        let mut server = None;
        let mut last_err = String::new();
        let tries = if cfg.port == 0 { 1 } else { 10 };
        for i in 0..tries {
            let port = if cfg.port == 0 { 0 } else { cfg.port.saturating_add(i) };
            match Server::http(("0.0.0.0", port)) {
                Ok(s) => {
                    server = Some(s);
                    break;
                }
                Err(e) => last_err = e.to_string(),
            }
        }
        let server = server.ok_or_else(|| format!("could not open a port: {}", last_err))?;
        let port = server.server_addr().to_ip().map(|a| a.port()).ok_or("no port")?;
        let shared = Arc::new(Shared {
            token: cfg.token,
            incoming: cfg.incoming,
            stop: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            handlers: AtomicUsize::new(0),
            file_seq: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                ver: 1,
                state: "{}".into(),
                lib: "{\"tracks\":[],\"playlists\":[]}".into(),
                lib_ver: 1,
                cover: Vec::new(),
                cover_ver: 1,
                notices: VecDeque::new(),
                notice_seq: 0,
                cmds: VecDeque::new(),
            }),
            cv: Condvar::new(),
        });
        let sh = shared.clone();
        thread::spawn(move || {
            while !sh.stop.load(Ordering::Relaxed) {
                match server.recv_timeout(Duration::from_millis(250)) {
                    Ok(Some(req)) => {
                        if sh.handlers.load(Ordering::Relaxed) >= MAX_HANDLERS {
                            send_empty(req, 503);
                            continue;
                        }
                        sh.handlers.fetch_add(1, Ordering::Relaxed);
                        let sh2 = sh.clone();
                        thread::spawn(move || {
                            handle(&sh2, req);
                            sh2.handlers.fetch_sub(1, Ordering::Relaxed);
                        });
                    }
                    Ok(None) => {}
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
            drop(server);
            sh.closed.store(true, Ordering::Relaxed);
        });
        Ok(Remote { shared, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Stops the server. Returns once the port is released (so a new server can use the same port).
    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        self.shared.cv.notify_all();
        let t0 = Instant::now();
        while !self.shared.closed.load(Ordering::Relaxed) && t0.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(10));
        }
    }

    /// Waits for commands sent from the phone. Returns an empty list on timeout or when stopped.
    pub fn poll(&self, timeout: Duration) -> Vec<String> {
        let deadline = Instant::now() + timeout;
        let mut g = self.shared.inner.lock().unwrap();
        loop {
            if !g.cmds.is_empty() {
                return g.cmds.drain(..).collect();
            }
            if self.shared.stop.load(Ordering::Relaxed) {
                return Vec::new();
            }
            let now = Instant::now();
            if now >= deadline {
                return Vec::new();
            }
            g = self.shared.cv.wait_timeout(g, deadline - now).unwrap().0;
        }
    }

    pub fn push_state(&self, json: String) -> Result<(), String> {
        check_json(&json)?;
        let mut g = self.shared.inner.lock().unwrap();
        if g.state != json {
            g.state = json;
            g.ver += 1;
            self.shared.cv.notify_all();
        }
        Ok(())
    }

    pub fn push_library(&self, json: String) -> Result<(), String> {
        check_json(&json)?;
        let mut g = self.shared.inner.lock().unwrap();
        if g.lib != json {
            g.lib = json;
            g.lib_ver += 1;
            g.ver += 1;
            self.shared.cv.notify_all();
        }
        Ok(())
    }

    /// JPEG (or PNG) bytes of the cover of the song that is playing; empty = no cover.
    pub fn push_cover(&self, bytes: Vec<u8>) {
        let mut g = self.shared.inner.lock().unwrap();
        g.cover = bytes;
        g.cover_ver += 1;
        g.ver += 1;
        self.shared.cv.notify_all();
    }

    /// A short message the phone shows (for example the result of an upload).
    pub fn push_notice(&self, msg: String) {
        add_notice(&self.shared, msg);
    }
}

fn check_json(s: &str) -> Result<(), String> {
    if s.len() > 8 * 1024 * 1024 {
        return Err("too large".into());
    }
    serde_json::from_str::<serde::de::IgnoredAny>(s).map(|_| ()).map_err(|e| e.to_string())
}

fn add_notice(sh: &Shared, msg: String) {
    let mut g = sh.inner.lock().unwrap();
    g.notice_seq += 1;
    let id = g.notice_seq;
    g.notices.push_back((id, msg));
    while g.notices.len() > 8 {
        g.notices.pop_front();
    }
    g.ver += 1;
    sh.cv.notify_all();
}

fn queue_cmd(sh: &Shared, cmd: String) -> bool {
    let mut g = sh.inner.lock().unwrap();
    if g.cmds.len() >= MAX_QUEUED_CMDS {
        return false;
    }
    g.cmds.push_back(cmd);
    sh.cv.notify_all();
    true
}

/// Constant-time comparison, so the token cannot be guessed from response times.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut d = 0u8;
    for i in 0..a.len() {
        d |= a[i] ^ b[i];
    }
    d == 0
}

pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|x| u8::from_str_radix(x, 16).ok());
            if let Some(v) = h {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn has_audio_ext(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((_, ext)) => AUDIO_EXT.contains(&ext.to_ascii_lowercase().as_str()),
        None => false,
    }
}

/// Keeps only a plain file name (no folders, no characters Windows does not allow).
pub fn clean_name(name: &str) -> String {
    let base = name.rsplit(|c| c == '/' || c == '\\').next().unwrap_or("");
    let cleaned: String = base
        .chars()
        .map(|c| if c.is_control() || "<>:\"|?*".contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_string();
    let cleaned: String = cleaned.chars().take(150).collect();
    cleaned
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).unwrap()
}

fn send(req: Request, status: u16, ctype: &str, body: Vec<u8>) {
    let r = Response::from_data(body)
        .with_status_code(status)
        .with_header(header("Content-Type", ctype))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("Referrer-Policy", "no-referrer"));
    let _ = req.respond(r);
}

fn send_empty(req: Request, status: u16) {
    let _ = req.respond(Response::empty(status));
}

fn send_json(req: Request, status: u16, body: String) {
    send(req, status, "application/json; charset=utf-8", body.into_bytes());
}

fn query_param<'a>(q: &'a str, key: &str) -> Option<&'a str> {
    q.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key {
            Some(v)
        } else {
            None
        }
    })
}

fn handle(sh: &Arc<Shared>, req: Request) {
    let url = req.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    // /r/<token>/...
    let rest = match path.strip_prefix("/r/") {
        Some(r) => r,
        None => return send_empty(req, 404),
    };
    let (tok, route) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if !ct_eq(tok, &sh.token) {
        return send_empty(req, 404);
    }
    let method = req.method().clone();
    match (method, route) {
        (Method::Get, "/") => send(req, 200, "text/html; charset=utf-8", PAGE.as_bytes().to_vec()),
        (Method::Get, "/api/state") => {
            let since: u64 = query_param(query, "v").and_then(|v| v.parse().ok()).unwrap_or(0);
            let body = {
                let mut g = sh.inner.lock().unwrap();
                let deadline = Instant::now() + LONG_POLL;
                while g.ver == since && !sh.stop.load(Ordering::Relaxed) {
                    let now = Instant::now();
                    if now >= deadline {
                        break;
                    }
                    g = sh.cv.wait_timeout(g, deadline - now).unwrap().0;
                }
                let notes: Vec<serde_json::Value> =
                    g.notices.iter().map(|(i, m)| serde_json::json!({"i": i, "m": m})).collect();
                format!(
                    "{{\"v\":{},\"lv\":{},\"cv\":{},\"s\":{},\"n\":{}}}",
                    g.ver,
                    g.lib_ver,
                    g.cover_ver,
                    g.state,
                    serde_json::Value::Array(notes)
                )
            };
            send_json(req, 200, body);
        }
        (Method::Get, "/api/library") => {
            let body = sh.inner.lock().unwrap().lib.clone();
            send_json(req, 200, body);
        }
        (Method::Get, "/api/cover") => {
            let c = sh.inner.lock().unwrap().cover.clone();
            if c.is_empty() {
                send_empty(req, 204);
            } else {
                let ct = if c.starts_with(&[0x89, b'P']) { "image/png" } else { "image/jpeg" };
                send(req, 200, ct, c);
            }
        }
        (Method::Post, "/api/cmd") => {
            let mut req = req;
            let mut buf = Vec::new();
            let _ = req.as_reader().take(MAX_CMD_BYTES + 1).read_to_end(&mut buf);
            // "upload" is what this server itself queues after saving a file; a phone must never be able to send it
            // (it carries a file path the app would read and then delete)
            let ok = buf.len() as u64 <= MAX_CMD_BYTES
                && serde_json::from_slice::<serde_json::Value>(&buf)
                    .map(|v| v.is_object() && v.get("op").and_then(|o| o.as_str()) != Some("upload"))
                    .unwrap_or(false)
                && queue_cmd(sh, String::from_utf8_lossy(&buf).into_owned());
            send_json(req, if ok { 200 } else { 400 }, format!("{{\"ok\":{}}}", ok));
        }
        (Method::Post, "/api/upload") => upload(sh, req),
        (Method::Post, "/api/fetch") => {
            let mut req = req;
            let mut buf = Vec::new();
            let _ = req.as_reader().take(MAX_CMD_BYTES + 1).read_to_end(&mut buf);
            let url = serde_json::from_slice::<serde_json::Value>(&buf)
                .ok()
                .and_then(|v| v.get("url").and_then(|u| u.as_str().map(|s| s.trim().to_string())));
            match url {
                Some(u) if (u.starts_with("http://") || u.starts_with("https://")) && u.len() < 2000 => {
                    let sh2 = sh.clone();
                    thread::spawn(move || download(&sh2, &u));
                    send_json(req, 200, "{\"ok\":true}".into());
                }
                _ => send_json(req, 400, "{\"ok\":false}".into()),
            }
        }
        _ => send_empty(req, 404),
    }
}

fn new_part_path(sh: &Shared) -> PathBuf {
    let n = sh.file_seq.fetch_add(1, Ordering::Relaxed);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    sh.incoming.join(format!("{}_{}.part", t, n))
}

fn upload(sh: &Arc<Shared>, mut req: Request) {
    let name = req
        .headers()
        .iter()
        .find(|h| h.field.equiv("x-filename"))
        .map(|h| clean_name(&percent_decode(h.value.as_str())))
        .unwrap_or_default();
    if !has_audio_ext(&name) {
        return send_json(req, 400, "{\"ok\":false,\"error\":\"not an audio file\"}".into());
    }
    if let Some(len) = req.body_length() {
        if len as u64 > MAX_UPLOAD {
            return send_json(req, 413, "{\"ok\":false,\"error\":\"too large\"}".into());
        }
        if len == 0 {
            return send_json(req, 400, "{\"ok\":false,\"error\":\"empty\"}".into());
        }
    }
    let part = new_part_path(sh);
    let written = (|| -> std::io::Result<u64> {
        let f = fs::File::create(&part)?;
        let mut w = BufWriter::with_capacity(256 * 1024, f);
        let n = std::io::copy(&mut req.as_reader().take(MAX_UPLOAD + 1), &mut w)?;
        w.flush()?;
        Ok(n)
    })();
    match written {
        Ok(n) if n > 0 && n <= MAX_UPLOAD => {
            let cmd = serde_json::json!({"op": "upload", "path": part.to_string_lossy(), "name": name}).to_string();
            if queue_cmd(sh, cmd) {
                send_json(req, 200, "{\"ok\":true}".into());
            } else {
                let _ = fs::remove_file(&part);
                send_json(req, 429, "{\"ok\":false,\"error\":\"busy\"}".into());
            }
        }
        Ok(n) => {
            let _ = fs::remove_file(&part);
            let too_big = n > MAX_UPLOAD;
            send_json(req, if too_big { 413 } else { 400 }, "{\"ok\":false}".into());
        }
        Err(_) => {
            let _ = fs::remove_file(&part);
            send_json(req, 500, "{\"ok\":false}".into());
        }
    }
}

fn file_name_from_headers(url: &str, disposition: Option<&str>, ctype: Option<&str>) -> String {
    if let Some(d) = disposition {
        // filename*=UTF-8''na%20me.mp3  or  filename="name.mp3"
        if let Some(i) = d.to_ascii_lowercase().find("filename*=") {
            let v = d[i + 10..].split(';').next().unwrap_or("").trim().trim_matches('"');
            let v = v.rsplit("''").next().unwrap_or(v);
            let n = clean_name(&percent_decode(v));
            if has_audio_ext(&n) {
                return n;
            }
        }
        if let Some(i) = d.to_ascii_lowercase().find("filename=") {
            let v = d[i + 9..].split(';').next().unwrap_or("").trim().trim_matches('"');
            let n = clean_name(&percent_decode(v));
            if has_audio_ext(&n) {
                return n;
            }
        }
    }
    let path = url.split(&['?', '#'][..]).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let n = clean_name(&percent_decode(last));
    if has_audio_ext(&n) {
        return n;
    }
    let ext = match ctype.map(|c| c.split(';').next().unwrap_or("").trim().to_ascii_lowercase()).as_deref() {
        Some("audio/mpeg") | Some("audio/mp3") => "mp3",
        Some("audio/mp4") | Some("audio/x-m4a") | Some("audio/m4a") => "m4a",
        Some("audio/flac") | Some("audio/x-flac") => "flac",
        Some("audio/wav") | Some("audio/x-wav") | Some("audio/wave") => "wav",
        Some("audio/ogg") => "ogg",
        Some("audio/opus") => "opus",
        Some("audio/aac") => "aac",
        _ => return String::new(),
    };
    let stem = if n.is_empty() { "download".to_string() } else { n };
    format!("{}.{}", stem, ext)
}

/// Downloads a direct link to an audio file into `incoming/`, then asks the app to import it.
fn download(sh: &Arc<Shared>, url: &str) {
    let fail = |m: &str| add_notice(sh, format!("Download failed: {}", m));
    // reqwest is built without a bundled crypto provider; install `ring` once (Err = already installed)
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = match reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(20 * 60))
        .user_agent("MyMusicPlayer/1.0")
        .build()
    {
        Ok(c) => c,
        Err(e) => return fail(&e.to_string()),
    };
    let mut resp = match client.get(url).send() {
        Ok(r) => r,
        Err(e) => return fail(&format!("could not connect ({})", e.without_url())),
    };
    if !resp.status().is_success() {
        return fail(&format!("the server answered {}", resp.status().as_u16()));
    }
    let hv = |k: &str| resp.headers().get(k).and_then(|v| v.to_str().ok()).map(|s| s.to_string());
    // the final address after redirects usually carries the real file name
    let final_url = resp.url().to_string();
    let name = file_name_from_headers(&final_url, hv("content-disposition").as_deref(), hv("content-type").as_deref());
    if name.is_empty() {
        return fail("the link is not a direct link to an audio file (mp3, m4a, flac, ...)");
    }
    if resp.content_length().map(|l| l > MAX_UPLOAD).unwrap_or(false) {
        return fail("the file is too large");
    }
    let part = new_part_path(sh);
    let res = (|| -> std::io::Result<u64> {
        let f = fs::File::create(&part)?;
        let mut w = BufWriter::with_capacity(256 * 1024, f);
        let n = std::io::copy(&mut (&mut resp).take(MAX_UPLOAD + 1), &mut w)?;
        w.flush()?;
        Ok(n)
    })();
    match res {
        Ok(n) if n > 0 && n <= MAX_UPLOAD => {
            let cmd = serde_json::json!({"op": "upload", "path": part.to_string_lossy(), "name": name}).to_string();
            if !queue_cmd(sh, cmd) {
                let _ = fs::remove_file(&part);
                fail("the app is busy");
            }
        }
        Ok(n) => {
            let _ = fs::remove_file(&part);
            fail(if n == 0 { "empty file" } else { "the file is too large" });
        }
        Err(e) => {
            let _ = fs::remove_file(&part);
            fail(&e.to_string());
        }
    }
}

/// Addresses of this computer on the local network, the most likely home/office one first.
pub fn local_addrs() -> Vec<String> {
    let mut v: Vec<(u8, String)> = Vec::new();
    if let Ok(list) = if_addrs::get_if_addrs() {
        for i in list {
            if let std::net::IpAddr::V4(ip) = i.ip() {
                if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() {
                    continue;
                }
                let o = ip.octets();
                let rank = if o[0] == 192 && o[1] == 168 {
                    0
                } else if o[0] == 10 {
                    1
                } else if o[0] == 172 && (16..=31).contains(&o[1]) {
                    2
                } else {
                    3
                };
                v.push((rank, ip.to_string()));
            }
        }
    }
    v.sort();
    v.dedup();
    v.into_iter().map(|(_, s)| s).collect()
}

pub fn qr_svg(text: &str) -> Result<String, String> {
    use qrcode::render::svg;
    let code = qrcode::QrCode::new(text.as_bytes()).map_err(|e| e.to_string())?;
    Ok(code
        .render::<svg::Color>()
        .min_dimensions(220, 220)
        .quiet_zone(true)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .build())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mp_remote_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn start(name: &str) -> (Remote, PathBuf) {
        let inc = tmp(name).join("incoming");
        let r = Remote::start(Config { port: 0, token: TOKEN.into(), incoming: inc.clone() }).unwrap();
        (r, inc)
    }

    /// minimal HTTP client: returns (status, body)
    fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> (u16, Vec<u8>) {
        let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
        let mut head = format!("{} {} HTTP/1.1\r\nHost: x\r\nConnection: close\r\nContent-Length: {}\r\n", method, path, body.len());
        for (k, v) in headers {
            head.push_str(&format!("{}: {}\r\n", k, v));
        }
        head.push_str("\r\n");
        s.write_all(head.as_bytes()).unwrap();
        s.write_all(body).unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let i = out.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4).unwrap_or(out.len());
        let status = String::from_utf8_lossy(&out[..out.len().min(15)]).split(' ').nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        (status, out[i..].to_vec())
    }

    fn url(p: &str) -> String {
        format!("/r/{}{}", TOKEN, p)
    }

    #[test]
    fn helpers() {
        assert!(ct_eq("abc", "abc") && !ct_eq("abc", "abd") && !ct_eq("abc", "ab"));
        assert_eq!(percent_decode("a%20b%D8%B3%D9%84%D8%A7%D9%85.mp3"), "a bسلام.mp3");
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
        assert_eq!(clean_name("../../etc/pass:wd?.mp3"), "pass_wd_.mp3");
        assert_eq!(clean_name("C:\\x\\y\\song.mp3"), "song.mp3");
        assert_eq!(clean_name("..."), "");
        assert!(has_audio_ext("a.MP3") && !has_audio_ext("a.exe") && !has_audio_ext("mp3"));
        assert_eq!(file_name_from_headers("https://x/y/Song%20One.mp3?token=1", None, None), "Song One.mp3");
        assert_eq!(file_name_from_headers("https://x/dl?id=3", Some("attachment; filename=\"a b.flac\""), None), "a b.flac");
        assert_eq!(file_name_from_headers("https://x/dl?id=3", Some("attachment; filename*=UTF-8''%D8%B3.mp3"), None), "س.mp3");
        assert_eq!(file_name_from_headers("https://x/stream", None, Some("audio/mpeg")), "stream.mp3");
        assert_eq!(file_name_from_headers("https://x/page.html", None, Some("text/html")), "");
    }

    #[test]
    fn token_and_routes() {
        let (r, _) = start("routes");
        let p = r.port();
        assert_eq!(http(p, "GET", "/", &[], b"").0, 404);
        assert_eq!(http(p, "GET", "/r/wrongtoken/", &[], b"").0, 404);
        assert_eq!(http(p, "GET", &format!("/r/{}x/", TOKEN), &[], b"").0, 404);
        assert_eq!(http(p, "GET", "/r/wrongtoken/api/state", &[], b"").0, 404);
        assert_eq!(http(p, "POST", "/r/wrongtoken/api/cmd", &[], b"{\"op\":\"next\"}").0, 404);
        let (st, body) = http(p, "GET", &url("/"), &[], b"");
        assert_eq!(st, 200);
        assert!(String::from_utf8_lossy(&body).contains("<html"));
        assert_eq!(http(p, "GET", &url("/nope"), &[], b"").0, 404);
        r.stop();
    }

    #[test]
    fn commands_reach_poll() {
        let (r, _) = start("cmd");
        let p = r.port();
        assert_eq!(http(p, "POST", &url("/api/cmd"), &[], b"{\"op\":\"next\"}").0, 200);
        assert_eq!(http(p, "POST", &url("/api/cmd"), &[], b"not json").0, 400);
        assert_eq!(http(p, "POST", &url("/api/cmd"), &[], b"[1,2]").0, 400);
        // a phone cannot fake the internal "upload" command
        assert_eq!(http(p, "POST", &url("/api/cmd"), &[], b"{\"op\":\"upload\",\"path\":\"C:/x\",\"name\":\"a.mp3\"}").0, 400);
        assert_eq!(http(p, "POST", &url("/api/cmd"), &[], &vec![b' '; 5000]).0, 400);
        let got = r.poll(Duration::from_secs(2));
        assert_eq!(got, vec!["{\"op\":\"next\"}".to_string()]);
        // blocked poll wakes up when a command arrives
        let t0 = Instant::now();
        let h = thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            http(p, "POST", &url("/api/cmd"), &[], b"{\"op\":\"prev\"}");
        });
        let got = r.poll(Duration::from_secs(10));
        assert_eq!(got.len(), 1);
        assert!(t0.elapsed() < Duration::from_secs(5));
        h.join().unwrap();
        // timeout returns empty
        assert!(r.poll(Duration::from_millis(200)).is_empty());
        r.stop();
    }

    #[test]
    fn state_long_poll_library_cover() {
        let (r, _) = start("state");
        let p = r.port();
        let (st, body) = http(p, "GET", &url("/api/state?v=0"), &[], b"");
        assert_eq!(st, 200);
        let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let v = j["v"].as_u64().unwrap();
        assert!(j["s"].is_object());
        // waits until something changes
        let t0 = Instant::now();
        let h = thread::spawn(move || http(p, "GET", &url(&format!("/api/state?v={}", v)), &[], b""));
        thread::sleep(Duration::from_millis(300));
        r.push_state("{\"playing\":true,\"t\":5}".into()).unwrap();
        let (_, body) = h.join().unwrap();
        assert!(t0.elapsed() < Duration::from_secs(5));
        let j: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(j["s"]["t"], 5);
        assert!(j["v"].as_u64().unwrap() > v);
        // same state again: no new version
        let v2 = j["v"].as_u64().unwrap();
        r.push_state("{\"playing\":true,\"t\":5}".into()).unwrap();
        let j: serde_json::Value = serde_json::from_slice(&http(p, "GET", &url("/api/state?v=0"), &[], b"").1).unwrap();
        assert_eq!(j["v"].as_u64().unwrap(), v2);
        assert!(r.push_state("{bad".into()).is_err());
        // notices
        r.push_notice("hello".into());
        let j: serde_json::Value = serde_json::from_slice(&http(p, "GET", &url("/api/state?v=0"), &[], b"").1).unwrap();
        assert_eq!(j["n"][0]["m"], "hello");
        // library
        r.push_library("{\"tracks\":[{\"id\":1}],\"playlists\":[]}".into()).unwrap();
        let (_, body) = http(p, "GET", &url("/api/library"), &[], b"");
        assert!(String::from_utf8_lossy(&body).contains("\"id\":1"));
        // cover
        assert_eq!(http(p, "GET", &url("/api/cover"), &[], b"").0, 204);
        r.push_cover(vec![0xFF, 0xD8, 0xFF, 1, 2, 3]);
        let (st, body) = http(p, "GET", &url("/api/cover"), &[], b"");
        assert_eq!((st, body), (200, vec![0xFF, 0xD8, 0xFF, 1, 2, 3]));
        r.stop();
    }

    #[test]
    fn upload_flow() {
        let (r, inc) = start("upload");
        let p = r.port();
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        let (st, _) = http(p, "POST", &url("/api/upload"), &[("X-Filename", "%D8%B3%D9%84%D8%A7%D9%85%20x.mp3")], &data);
        assert_eq!(st, 200);
        let cmds = r.poll(Duration::from_secs(2));
        assert_eq!(cmds.len(), 1);
        let c: serde_json::Value = serde_json::from_str(&cmds[0]).unwrap();
        assert_eq!(c["op"], "upload");
        assert_eq!(c["name"], "سلام x.mp3");
        let saved = fs::read(c["path"].as_str().unwrap()).unwrap();
        assert_eq!(saved, data);
        assert!(PathBuf::from(c["path"].as_str().unwrap()).starts_with(&inc));
        // bad extension, no name, empty body, path tricks
        assert_eq!(http(p, "POST", &url("/api/upload"), &[("X-Filename", "virus.exe")], b"abc").0, 400);
        assert_eq!(http(p, "POST", &url("/api/upload"), &[], b"abc").0, 400);
        assert_eq!(http(p, "POST", &url("/api/upload"), &[("X-Filename", "a.mp3")], b"").0, 400);
        let (st, _) = http(p, "POST", &url("/api/upload"), &[("X-Filename", "..%2F..%2Fevil.mp3")], b"abc");
        assert_eq!(st, 200);
        let c: serde_json::Value = serde_json::from_str(&r.poll(Duration::from_secs(2))[0]).unwrap();
        assert_eq!(c["name"], "evil.mp3");
        assert!(PathBuf::from(c["path"].as_str().unwrap()).starts_with(&inc), "staging path must stay in incoming/");
        // wrong token: nothing written
        let before = fs::read_dir(&inc).unwrap().count();
        assert_eq!(http(p, "POST", "/r/nopenopenopenopenope/api/upload", &[("X-Filename", "a.mp3")], b"abc").0, 404);
        assert_eq!(fs::read_dir(&inc).unwrap().count(), before);
        r.stop();
    }

    #[test]
    fn stop_unblocks_and_restart_cleans_incoming() {
        let (r, inc) = start("stop");
        fs::write(inc.join("old.part"), b"x").unwrap();
        let r = Arc::new(r);
        let r2 = r.clone();
        let t0 = Instant::now();
        let h = thread::spawn(move || r2.poll(Duration::from_secs(30)));
        thread::sleep(Duration::from_millis(200));
        r.stop();
        assert!(h.join().unwrap().is_empty());
        assert!(t0.elapsed() < Duration::from_secs(5));
        // a new server on the same folder removes leftovers
        let r3 = Remote::start(Config { port: 0, token: TOKEN.into(), incoming: inc.clone() }).unwrap();
        assert!(!inc.join("old.part").exists());
        r3.stop();
    }

    #[test]
    fn rejects_weak_token_and_busy_port_moves_on() {
        let inc = tmp("weak").join("i");
        assert!(Remote::start(Config { port: 0, token: "short".into(), incoming: inc.clone() }).is_err());
        assert!(Remote::start(Config { port: 0, token: "../../etc/passwd/aaaaaaaa".into(), incoming: inc.clone() }).is_err());
        let a = Remote::start(Config { port: 0, token: TOKEN.into(), incoming: inc.clone() }).unwrap();
        let b = Remote::start(Config { port: a.port(), token: TOKEN.into(), incoming: tmp("weak2").join("i") }).unwrap();
        assert_ne!(a.port(), b.port());
        a.stop();
        b.stop();
    }

    /// small file server for the download-by-link tests
    fn file_server() -> u16 {
        let srv = Server::http(("127.0.0.1", 0)).unwrap();
        let port = srv.server_addr().to_ip().unwrap().port();
        thread::spawn(move || {
            for req in srv.incoming_requests() {
                let u = req.url().to_string();
                let r = if u.starts_with("/song.mp3") {
                    Response::from_data(vec![7u8; 5000]).with_header(header("Content-Type", "audio/mpeg")).boxed()
                } else if u.starts_with("/redir") {
                    Response::empty(302).with_header(header("Location", "/song.mp3")).boxed()
                } else if u.starts_with("/dl") {
                    Response::from_data(vec![9u8; 100])
                        .with_header(header("Content-Disposition", "attachment; filename=\"from header.flac\""))
                        .boxed()
                } else if u.starts_with("/page") {
                    Response::from_data(b"<html></html>".to_vec()).with_header(header("Content-Type", "text/html")).boxed()
                } else {
                    Response::empty(404).boxed()
                };
                let _ = req.respond(r);
            }
        });
        port
    }

    fn notices(p: u16) -> Vec<String> {
        let j: serde_json::Value = serde_json::from_slice(&http(p, "GET", &url("/api/state?v=0"), &[], b"").1).unwrap();
        j["n"].as_array().unwrap().iter().map(|x| x["m"].as_str().unwrap().to_string()).collect()
    }

    #[test]
    fn fetch_by_link() {
        let (r, inc) = start("fetch");
        let p = r.port();
        let fp = file_server();
        let body = |u: &str| format!("{{\"url\":\"{}\"}}", u);
        // direct link
        assert_eq!(http(p, "POST", &url("/api/fetch"), &[], body(&format!("http://127.0.0.1:{}/song.mp3", fp)).as_bytes()).0, 200);
        let c: serde_json::Value = serde_json::from_str(&r.poll(Duration::from_secs(10))[0]).unwrap();
        assert_eq!((c["op"].as_str().unwrap(), c["name"].as_str().unwrap()), ("upload", "song.mp3"));
        assert_eq!(fs::read(c["path"].as_str().unwrap()).unwrap().len(), 5000);
        assert!(PathBuf::from(c["path"].as_str().unwrap()).starts_with(&inc));
        // redirect, and file name from Content-Disposition
        http(p, "POST", &url("/api/fetch"), &[], body(&format!("http://127.0.0.1:{}/redir", fp)).as_bytes());
        let got = r.poll(Duration::from_secs(10));
        assert!(!got.is_empty(), "redirect download gave nothing; notices: {:?}", notices(p));
        let c: serde_json::Value = serde_json::from_str(&got[0]).unwrap();
        assert_eq!(c["name"], "song.mp3");
        http(p, "POST", &url("/api/fetch"), &[], body(&format!("http://127.0.0.1:{}/dl?id=1", fp)).as_bytes());
        let c: serde_json::Value = serde_json::from_str(&r.poll(Duration::from_secs(10))[0]).unwrap();
        assert_eq!(c["name"], "from header.flac");
        // not audio / 404 / unreachable / bad scheme / not json
        http(p, "POST", &url("/api/fetch"), &[], body(&format!("http://127.0.0.1:{}/page", fp)).as_bytes());
        http(p, "POST", &url("/api/fetch"), &[], body(&format!("http://127.0.0.1:{}/missing.mp3", fp)).as_bytes());
        http(p, "POST", &url("/api/fetch"), &[], body("http://127.0.0.1:1/x.mp3").as_bytes());
        assert_eq!(http(p, "POST", &url("/api/fetch"), &[], body("file:///etc/passwd").as_bytes()).0, 400);
        assert_eq!(http(p, "POST", &url("/api/fetch"), &[], b"nope").0, 400);
        let t0 = Instant::now();
        while notices(p).len() < 3 && t0.elapsed() < Duration::from_secs(15) {
            thread::sleep(Duration::from_millis(100));
        }
        let n = notices(p).join(" | ");
        assert!(n.contains("not a direct link"), "{}", n);
        assert!(n.contains("answered 404"), "{}", n);
        assert!(n.contains("could not connect"), "{}", n);
        assert!(r.poll(Duration::from_millis(300)).is_empty(), "failed downloads must not queue imports");
        assert_eq!(fs::read_dir(&inc).unwrap().filter(|e| e.as_ref().unwrap().path().extension().map(|x| x == "part").unwrap_or(false)).count(), 3);
        r.stop();
    }

    #[test]
    fn stop_frees_the_port_at_once() {
        let inc = tmp("port").join("i");
        let a = Remote::start(Config { port: 0, token: TOKEN.into(), incoming: inc.clone() }).unwrap();
        let port = a.port();
        a.stop();
        let b = Remote::start(Config { port, token: TOKEN.into(), incoming: inc }).unwrap();
        assert_eq!(b.port(), port, "restart must get the same port back");
        assert_eq!(http(port, "GET", &url("/"), &[], b"").0, 200);
        b.stop();
    }

    #[test]
    fn qr_and_addresses() {
        let svg = qr_svg("http://192.168.1.5:47890/r/0123456789abcdef0123456789abcdef/").unwrap();
        assert!(svg.contains("<svg"));
        for a in local_addrs() {
            assert!(a.parse::<std::net::Ipv4Addr>().is_ok());
        }
    }
}
