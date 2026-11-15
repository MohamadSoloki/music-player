//! Song details from a link: an Apple Music / iTunes song link (iTunes Lookup API) or a Spotify song link
//! (the public page's `og:` tags). Used by "Fix info from link": the owner pastes the link, so nothing is guessed.
//! No Tauri dependency. Only the id inside the link is sent out: the address the owner pasted is never fetched itself.

use std::time::Duration;

use serde_json::{json, Value};

#[derive(Debug, PartialEq)]
enum Link {
    /// storefront (two letters) and the song's id
    Apple(String, String),
    /// the 22-character track id
    Spotify(String),
}

const NOT_SONG: &str = "This link is not a song. Open the song itself and copy its link.";
const UNKNOWN: &str = "This is not an Apple Music, iTunes or Spotify song link.";

/// Splits "https://host/a/b?x=1#f" into (host, path segments, query pairs).
fn split_url(u: &str) -> Option<(String, Vec<String>, Vec<(String, String)>)> {
    let rest = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://"))?;
    let rest = rest.split('#').next()?;
    let (head, query) = rest.split_once('?').unwrap_or((rest, ""));
    let (host, path) = head.split_once('/').unwrap_or((head, ""));
    if host.is_empty() || host.contains('@') || host.contains(':') {
        return None;
    }
    let segs = path.split('/').filter(|s| !s.is_empty()).map(String::from).collect();
    let q = query.split('&').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();
    Some((host.to_ascii_lowercase(), segs, q))
}

fn spotify_id(s: &str) -> Option<String> {
    (s.len() == 22 && s.chars().all(|c| c.is_ascii_alphanumeric())).then(|| s.to_string())
}

fn parse_link(input: &str) -> Result<Link, String> {
    let u = input.trim();
    if let Some(id) = u.strip_prefix("spotify:track:") {
        return spotify_id(id).map(Link::Spotify).ok_or_else(|| UNKNOWN.to_string());
    }
    let Some((host, segs, query)) = split_url(u) else { return Err(UNKNOWN.into()) };
    match host.as_str() {
        "open.spotify.com" | "play.spotify.com" => {
            // optional "intl-xx" and "embed" in front: /intl-de/track/ID, /embed/track/ID
            let mut it = segs.iter().skip_while(|s| s.starts_with("intl-") || *s == "embed");
            match (it.next().map(String::as_str), it.next()) {
                (Some("track"), Some(id)) => spotify_id(id).map(Link::Spotify).ok_or_else(|| UNKNOWN.to_string()),
                (Some("album" | "playlist" | "artist" | "show" | "episode"), _) => Err(NOT_SONG.into()),
                _ => Err(UNKNOWN.into()),
            }
        }
        "music.apple.com" | "itunes.apple.com" | "geo.music.apple.com" | "embed.music.apple.com" => {
            let cc = segs.first().filter(|s| s.len() == 2 && s.chars().all(|c| c.is_ascii_alphabetic())).map(|s| s.to_ascii_lowercase()).unwrap_or_else(|| "us".into());
            let digits = |s: &str| {
                let s = s.strip_prefix("id").unwrap_or(s);
                (!s.is_empty() && s.len() <= 20 && s.chars().all(|c| c.is_ascii_digit())).then(|| s.to_string())
            };
            // an album link that points at one song: ...?i=<song id>
            if let Some(id) = query.iter().find(|(k, _)| k == "i").and_then(|(_, v)| digits(v)) {
                return Ok(Link::Apple(cc, id));
            }
            if segs.iter().any(|s| s == "song") {
                if let Some(id) = segs.last().and_then(|s| digits(s)) {
                    return Ok(Link::Apple(cc, id));
                }
            }
            if segs.iter().any(|s| s == "album" || s == "playlist" || s == "artist") {
                return Err(NOT_SONG.into());
            }
            Err(UNKNOWN.into())
        }
        _ => Err(UNKNOWN.into()),
    }
}

// ---------- Apple ----------

/// Cover addresses are accepted only from the services' own picture servers.
fn cover_ok(url: &str, host: &str) -> bool {
    url.starts_with("https://") && url.split('/').nth(2).is_some_and(|h| h == host || (host.starts_with('.') && h.ends_with(host)))
}

fn apple_hit(body: &Value) -> Option<Value> {
    let r = body["results"].as_array()?.iter().find(|r| r["kind"] == "song" || r["wrapperType"] == "track")?;
    let album = r["collectionName"].as_str().unwrap_or("");
    let album = album.strip_suffix(" - Single").unwrap_or(album);
    let art = r["artworkUrl100"].as_str().unwrap_or("").replace("100x100bb", "512x512bb");
    Some(json!({
        "title": r["trackName"].as_str()?, "artist": r["artistName"].as_str()?, "album": album,
        "cover": if cover_ok(&art, ".mzstatic.com") { art } else { String::new() }, "source": "Apple Music",
    }))
}

// ---------- Spotify ----------

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(end) = rest.find(';').filter(|e| *e <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..end];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Value of attribute `name` inside one tag ("<meta property=\"og:title\" content=\"x\">").
fn attr(tag: &str, name: &str) -> Option<String> {
    let mut from = 0;
    while let Some(i) = tag[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before_ok = tag[..at].chars().next_back().is_some_and(char::is_whitespace);
        let after = tag[from..].trim_start();
        if !before_ok || !after.starts_with('=') {
            continue;
        }
        let v = after[1..].trim_start();
        let q = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let v = &v[1..];
        return v.find(q).map(|e| unescape(&v[..e]));
    }
    None
}

/// The `content` of the first `<meta>` whose `property` or `name` is `key`.
fn meta(html: &str, key: &str) -> Option<String> {
    let mut rest = html;
    while let Some(i) = rest.find("<meta") {
        rest = &rest[i + 5..];
        let tag = &rest[..rest.find('>').unwrap_or(rest.len())];
        if attr(tag, "property").or_else(|| attr(tag, "name")).as_deref() == Some(key) {
            return attr(tag, "content");
        }
    }
    None
}

/// og:description is "Artist, Artist · Album · Song · 2024": artists first, kind and year last.
fn spotify_hit(html: &str) -> Option<Value> {
    let title = meta(html, "og:title")?.trim().to_string();
    let desc = meta(html, "og:description").unwrap_or_default();
    let parts: Vec<&str> = desc.split(" · ").map(str::trim).collect();
    let artist = parts.first().copied().unwrap_or("");
    let album = if parts.len() >= 4 { parts[1..parts.len() - 2].join(" · ") } else { String::new() };
    let img = meta(html, "og:image").unwrap_or_default();
    if title.is_empty() || artist.is_empty() {
        return None;
    }
    Some(json!({
        "title": title, "artist": artist, "album": album,
        "cover": if cover_ok(&img, "i.scdn.co") { img } else { String::new() }, "source": "Spotify",
    }))
}

// ---------- network ----------

fn get(url: &str, who: &str) -> Result<Vec<u8>, String> {
    // reqwest is built without a bundled crypto provider; install `ring` once (Err = already installed)
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(8))
        .user_agent("MyMusicPlayer/1.0")
        .build()
        .map_err(|e| e.to_string())?;
    let fail = |e: reqwest::Error| format!("Could not reach {}. Check the internet connection and try again. ({})", who, e.without_url());
    let resp = client.get(url).header("Accept-Language", "en").send().map_err(fail)?;
    if !resp.status().is_success() {
        return Err(format!("{} answered {}. Check the link and try again.", who, resp.status().as_u16()));
    }
    resp.bytes().map(|b| b.to_vec()).map_err(fail)
}

/// Details of the song behind a link: {title, artist, album, cover, source}. Err: a message for the owner.
pub fn info(link: &str) -> Result<Value, String> {
    match parse_link(link)? {
        Link::Apple(cc, id) => {
            let bytes = get(&format!("https://itunes.apple.com/lookup?id={}&country={}", id, cc), "Apple")?;
            let body: Value = serde_json::from_slice(&bytes).map_err(|_| "Apple gave an answer that cannot be read.".to_string())?;
            apple_hit(&body).ok_or_else(|| "Apple has no song for this link.".to_string())
        }
        Link::Spotify(id) => {
            let bytes = get(&format!("https://open.spotify.com/track/{}", id), "Spotify")?;
            spotify_hit(&String::from_utf8_lossy(&bytes)).ok_or_else(|| "Spotify did not give the song's details. Try again later, or use an Apple Music link.".to_string())
        }
    }
}

/// The picture behind a cover address from `info` (JPEG, PNG or WebP, at most 5 MB). Only the services' own
/// picture servers are contacted.
pub fn image(url: &str) -> Result<Vec<u8>, String> {
    if !(cover_ok(url, ".mzstatic.com") || cover_ok(url, "i.scdn.co")) {
        return Err("This picture address is not accepted.".into());
    }
    let b = get(url, if url.contains("scdn.co") { "Spotify" } else { "Apple" })?;
    if b.len() > 5_000_000 || !looks_like_image(&b) {
        return Err("The cover is not a usable picture.".into());
    }
    Ok(b)
}

fn looks_like_image(b: &[u8]) -> bool {
    b.starts_with(&[0xFF, 0xD8]) || b.starts_with(&[0x89, b'P', b'N', b'G']) || (b.len() > 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP")
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "45J4avUb9Ni0bnETYaYFVJ";

    #[test]
    fn spotify_links() {
        for u in [
            format!("https://open.spotify.com/track/{ID}"),
            format!("https://open.spotify.com/track/{ID}?si=abc123"),
            format!("  https://open.spotify.com/intl-de/track/{ID}?si=x#frag "),
            format!("https://open.spotify.com/embed/track/{ID}"),
            format!("spotify:track:{ID}"),
        ] {
            assert_eq!(parse_link(&u), Ok(Link::Spotify(ID.into())), "{}", u);
        }
        assert_eq!(parse_link("https://open.spotify.com/album/0hvT3yIEysuuvkK73vgdcW"), Err(NOT_SONG.into()));
        assert_eq!(parse_link("https://open.spotify.com/playlist/37i9dQZF1DZ06evO1IPOOk"), Err(NOT_SONG.into()));
        assert_eq!(parse_link("https://open.spotify.com/track/short"), Err(UNKNOWN.into()));
    }

    #[test]
    fn apple_links() {
        let a = |cc: &str, id: &str| Ok(Link::Apple(cc.into(), id.into()));
        assert_eq!(parse_link("https://music.apple.com/us/album/prayer/1234567?i=7654321"), a("us", "7654321"));
        assert_eq!(parse_link("https://music.apple.com/de/song/prayer/7654321"), a("de", "7654321"));
        assert_eq!(parse_link("https://itunes.apple.com/gb/album/prayer/id1234567?i=7654321&uo=4"), a("gb", "7654321"));
        assert_eq!(parse_link("https://music.apple.com/album/prayer/1234567?i=7654321"), a("us", "7654321"), "no storefront: US");
        assert_eq!(parse_link("https://music.apple.com/us/album/prayer/1234567"), Err(NOT_SONG.into()), "an album, not a song");
        assert_eq!(parse_link("https://music.apple.com/us/song/prayer/abc"), Err(UNKNOWN.into()));
    }

    #[test]
    fn other_addresses_are_refused() {
        for u in [
            "", "prayer kendrick lamar", "ftp://open.spotify.com/track/45J4avUb9Ni0bnETYaYFVJ",
            "https://open.spotify.com.evil.example/track/45J4avUb9Ni0bnETYaYFVJ",
            "https://evil.example/open.spotify.com/track/45J4avUb9Ni0bnETYaYFVJ",
            "https://user@open.spotify.com/track/45J4avUb9Ni0bnETYaYFVJ",
            "https://localhost:8080/us/song/x/1", "https://youtube.com/watch?v=abc",
        ] {
            assert!(parse_link(u).is_err(), "{}", u);
        }
    }

    #[test]
    fn spotify_page_details() {
        let html = r#"<html><head><title>x</title>
            <meta property="og:title" content="luther (with sza)"/>
            <meta property="og:description" content="Kendrick Lamar, SZA &middot; GNX &middot; Song &middot; 2024"/>
            <meta name='x' content='no'><meta property="og:image" content="https://i.scdn.co/image/ab67616d0000b273e2a0a166493a2bde5480a420">"#;
        // the real page writes a plain "·"; `&middot;` is not decoded on purpose, so that case must fail cleanly below
        let real = html.replace("&middot;", "·");
        let h = spotify_hit(&real).unwrap();
        assert_eq!((h["title"].as_str(), h["artist"].as_str(), h["album"].as_str()), (Some("luther (with sza)"), Some("Kendrick Lamar, SZA"), Some("GNX")));
        assert_eq!(h["cover"], "https://i.scdn.co/image/ab67616d0000b273e2a0a166493a2bde5480a420");
        assert_eq!(h["source"], "Spotify");
        // entities, an album name that holds the separator, a foreign picture server
        let h = spotify_hit(r#"<meta property="og:title" content="Rock &amp; Roll &#39;n&#x27; Co"><meta property="og:description" content="A &amp; B · Best · Of · Song · 2001"><meta property="og:image" content="https://evil.example/a.jpg">"#).unwrap();
        assert_eq!((h["title"].as_str(), h["artist"].as_str(), h["album"].as_str()), (Some("Rock & Roll 'n' Co"), Some("A & B"), Some("Best · Of")));
        assert_eq!(h["cover"], "");
        // no details: not a song page
        assert_eq!(spotify_hit("<html>captcha</html>"), None);
        assert_eq!(spotify_hit(r#"<meta property="og:title" content="X">"#), None, "no artist");
    }

    #[test]
    fn apple_lookup_answer() {
        let body = json!({"resultCount": 1, "results": [{"wrapperType": "track", "kind": "song", "trackName": "Prayer", "artistName": "Kendrick Lamar",
            "collectionName": "Prayer - Single", "artworkUrl100": "https://is1-ssl.mzstatic.com/image/thumb/Music/abc/100x100bb.jpg"}]});
        let h = apple_hit(&body).unwrap();
        assert_eq!((h["title"].as_str(), h["artist"].as_str(), h["album"].as_str()), (Some("Prayer"), Some("Kendrick Lamar"), Some("Prayer")));
        assert_eq!(h["cover"], "https://is1-ssl.mzstatic.com/image/thumb/Music/abc/512x512bb.jpg");
        assert_eq!(apple_hit(&json!({"resultCount": 0, "results": []})), None);
        // an album answer is not a song
        assert_eq!(apple_hit(&json!({"results": [{"wrapperType": "collection", "collectionName": "A", "artistName": "B"}]})), None);
        for bad in ["http://is1-ssl.mzstatic.com/a.jpg", "https://evil.example/a.jpg", "https://mzstatic.com.evil.example/a.jpg", "javascript:alert(1)"] {
            let b = json!({"results": [{"kind": "song", "trackName": "P", "artistName": "K", "artworkUrl100": bad}]});
            assert_eq!(apple_hit(&b).unwrap()["cover"], "", "{}", bad);
        }
    }

    #[test]
    fn only_the_services_picture_servers_are_downloaded() {
        for u in ["http://i.scdn.co/image/a", "https://evil.example/a.jpg", "https://i.scdn.co.evil.example/a", "file:///etc/passwd", "https://localhost/a.png"] {
            assert!(image(u).is_err(), "{}", u);
        }
        assert!(looks_like_image(&[0xFF, 0xD8, 0xFF, 0xE0]) && looks_like_image(b"\x89PNG\r\n") && looks_like_image(b"RIFF1234WEBPVP8 "));
        assert!(!looks_like_image(b"<html>") && !looks_like_image(b""));
    }

    #[test]
    fn a_wrong_link_never_reaches_the_network() {
        assert_eq!(info("https://example.com/x"), Err(UNKNOWN.into()));
        assert_eq!(info("https://music.apple.com/us/album/x/1"), Err(NOT_SONG.into()));
    }
}
