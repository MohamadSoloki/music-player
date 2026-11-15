//! Finds a song on iTunes Search (free, no key): its official cover for the Discord presence, and its official
//! title / artist / album for "Fix info online". Discord only shows pictures that have an address on the internet,
//! so the cover inside the audio file cannot be used.
//! No Tauri dependency. Only the song title and artist leave the computer, and only when the owner turned the
//! "Look up covers online" switch on or pressed "Fix info online".

use std::time::Duration;

use serde_json::{json, Value};

/// What iTunes knows about one song. `cover` is empty when the picture is not on an Apple server.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub cover: String,
}

// ---------- text helpers ----------

/// One letter in a comparable form: lower case, no accents, Persian/Arabic variants unified, digits as 0-9.
fn fold_char(c: char, out: &mut String) {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => out.push('a'),
        'ç' | 'ć' | 'č' => out.push('c'),
        'ď' | 'đ' => out.push('d'),
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' | 'ě' => out.push('e'),
        'ğ' => out.push('g'),
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'ı' => out.push('i'),
        'ł' => out.push('l'),
        'ñ' | 'ń' | 'ň' => out.push('n'),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ő' => out.push('o'),
        'ř' => out.push('r'),
        'ś' | 'š' | 'ş' => out.push('s'),
        'ť' | 'ţ' => out.push('t'),
        'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' | 'ű' => out.push('u'),
        'ý' | 'ÿ' => out.push('y'),
        'ź' | 'ž' | 'ż' => out.push('z'),
        'ß' => out.push_str("ss"),
        'æ' => out.push_str("ae"),
        'œ' => out.push_str("oe"),
        'ي' | 'ى' => out.push('ی'),
        'ك' => out.push('ک'),
        'أ' | 'إ' | 'آ' => out.push('ا'),
        'ة' => out.push('ه'),
        'ۀ' => out.push('ه'),
        '۰'..='۹' => out.push((b'0' + (c as u32 - '۰' as u32) as u8) as char),
        '٠'..='٩' => out.push((b'0' + (c as u32 - '٠' as u32) as u8) as char),
        // Arabic marks (fatha, kasra, ...) and tatweel carry no meaning for matching
        '\u{064B}'..='\u{065F}' | '\u{0670}' | '\u{0640}' => {}
        _ => out.push(c),
    }
}

/// Letters and digits only, folded: "Beyoncé & Jay-Z" and "beyonce and jay z" are told apart only by the "and".
fn norm(s: &str) -> String {
    let mut out = String::new();
    for c in s.replace('&', " and ").chars().flat_map(|c| c.to_lowercase()) {
        if c.is_alphanumeric() || ('\u{064B}'..='\u{065F}').contains(&c) || c == '\u{0640}' || c == '\u{0670}' {
            fold_char(c, &mut out);
        }
    }
    out
}

fn lev(a: &[char], b: &[char]) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![i];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur.push((prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost));
        }
        prev = cur;
    }
    prev[b.len()]
}

/// 3 same, 2 one contains the other, 1 a small typo (about one wrong letter in six), 0 different.
fn closeness(a: &str, b: &str) -> u8 {
    let (a, b) = (norm(a), norm(b));
    let (la, lb) = (a.chars().count(), b.chars().count());
    if la < 2 || lb < 2 {
        return 0;
    }
    if a == b {
        return 3;
    }
    if la.min(lb) >= 3 && (a.contains(&b) || b.contains(&a)) {
        return 2;
    }
    let (ca, cb): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if la.min(lb) >= 4 && lev(&ca, &cb) <= (la.min(lb) / 6).clamp(1, 3) {
        return 1;
    }
    0
}

/// Text outside (...) and [...], plus the pieces inside them.
fn split_brackets(s: &str) -> (String, Vec<String>) {
    let (mut out, mut inner, mut parts) = (String::new(), String::new(), Vec::new());
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' | '[' => {
                if depth == 0 {
                    inner.clear();
                }
                depth += 1;
            }
            ')' | ']' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    parts.push(inner.trim().to_string());
                }
            }
            _ if depth == 0 => out.push(c),
            _ => inner.push(c),
        }
    }
    (out.split_whitespace().collect::<Vec<_>>().join(" "), parts)
}

/// "feat. A, B" -> ["A", "B"]; None when the text does not start with a featuring word.
fn featuring(s: &str) -> Option<Vec<String>> {
    let s = s.trim();
    let low = s.to_ascii_lowercase(); // same byte length as `s`
    for w in ["featuring ", "feat. ", "feat ", "ft. ", "ft ", "with "] {
        if low.starts_with(w) {
            return s.get(w.len()..).map(split_names);
        }
    }
    None
}

/// "A, B & C" -> ["A", "B", "C"]
fn split_names(s: &str) -> Vec<String> {
    let mut t = s.replace('_', " ");
    for sep in [" feat. ", " feat ", " ft. ", " ft ", " featuring ", " with ", " and ", " x ", " X ", ",", "،", "&", ";", "/", "+"] {
        t = t.replace(sep, "\u{1}");
    }
    t.split('\u{1}').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect()
}

/// "www.site.ir", "site.com": web addresses that download sites put into tags
fn is_site(tok: &str) -> bool {
    let t = tok.to_lowercase();
    let t = t.trim_matches(|c: char| !c.is_alphanumeric());
    t.starts_with("www.") || [".com", ".ir", ".net", ".org", ".info", ".me", ".co", ".biz"].iter().any(|d| t.ends_with(d) && t.len() > d.len() + 1)
}

/// Words of a "- Remastered 2011" style ending (only cut after a " - ", so real subtitles with other words stay)
const NOISE: [&str; 29] = [
    "remastered", "remaster", "version", "single", "edit", "radio", "mono", "stereo", "explicit", "clean", "official",
    "lyrics", "lyric", "audio", "video", "music", "hd", "hq", "visualizer", "4k", "original", "album", "deluxe", "bonus",
    "track", "mp3", "new", "full", "kbps",
];
/// Words cut from the very end of a title without a dash: only ones that are never part of a real title
const TAIL: [&str; 10] = ["official", "video", "audio", "lyrics", "lyric", "visualizer", "hd", "hq", "4k", "mp3"];
const TAIL_PHRASES: [&str; 6] = ["official music video", "official lyric video", "official visualizer", "music video", "lyric video", "lyrics video"];

fn kbps(w: &str) -> bool {
    w.strip_suffix("kbps").is_some_and(|d| !d.is_empty() && d.chars().all(|c| c.is_ascii_digit()))
}

fn all_noise(s: &str) -> bool {
    let words: Vec<String> = s.to_lowercase().split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).map(String::from).collect();
    !words.is_empty() && words.iter().all(|w| NOISE.contains(&w.as_str()) || kbps(w) || w.chars().all(|c| c.is_ascii_digit()))
}

/// The title without what makes a search fail, and the featured artists that were written into it.
/// "01 - Prayer (feat. X) [Official Video] - Remastered 2011 www.site.ir" -> ("Prayer", ["X"])
fn tidy_title(s: &str) -> (String, Vec<String>) {
    let (mut t, parts) = split_brackets(&s.replace('_', " "));
    let mut feats: Vec<String> = parts.iter().filter_map(|p| featuring(p)).flatten().collect();
    t = t.split_whitespace().filter(|w| !is_site(w)).collect::<Vec<_>>().join(" ");
    // track number in front: "01 - ", "01. ", "1) "
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=3).contains(&digits) {
        let rest = t[digits..].trim_start();
        if let Some(r) = rest.strip_prefix(['.', '-', ')']) {
            if r.starts_with(' ') {
                t = r.trim().to_string();
            }
        }
    }
    // "Title ft. X" / "Title feat. X"
    let low = t.to_ascii_lowercase(); // same byte length as `t`
    for m in [" featuring ", " feat. ", " feat ", " ft. ", " ft "] {
        if let Some(i) = low.find(m) {
            feats.extend(split_names(&t[i + m.len()..]));
            t = t[..i].trim().to_string();
            break;
        }
    }
    // "Title - Remastered 2011", "Title - Official Video": cut every trailing "- noise" part
    while let Some(i) = t.rfind(" - ") {
        if all_noise(&t[i + 3..]) {
            t = t[..i].trim().to_string();
        } else {
            break;
        }
    }
    // "Title Official Music Video", "Title Lyrics", "Title 320kbps": trailing noise (never the whole title)
    loop {
        let low = t.to_ascii_lowercase();
        if let Some(ph) = TAIL_PHRASES.iter().find(|p| low.ends_with(*p) && low.len() > p.len()) {
            t = t[..t.len() - ph.len()].trim().to_string();
            continue;
        }
        let words: Vec<&str> = t.split_whitespace().collect();
        let last = words.last().map(|w| w.to_ascii_lowercase()).unwrap_or_default();
        if words.len() < 2 || !(TAIL.contains(&last.as_str()) || kbps(&last)) {
            break;
        }
        let rest = words[..words.len() - 1].join(" ");
        if all_noise(&rest) {
            break; // the title is made of such words only
        }
        t = rest;
    }
    (t.trim().to_string(), feats)
}

/// The artists written in the artist field: main artist first. Drops "- Topic" (YouTube) and web addresses.
fn tidy_artists(s: &str) -> Vec<String> {
    let (t, _) = split_brackets(s);
    let t = t.split_whitespace().filter(|w| !is_site(w)).collect::<Vec<_>>().join(" ");
    let t = t.strip_suffix(" - Topic").unwrap_or(&t).to_string();
    split_names(&t)
}

// ---------- search ----------

/// One way to look for the song: the words to search, and what the answer must be called.
#[derive(Debug, PartialEq)]
struct Plan {
    term: String,
    title: String,
    artists: Vec<String>,
}

/// Several readings of the same tags, most likely first, without repeats.
fn plans(title: &str, artist: &str) -> Vec<Plan> {
    let (t, feats) = tidy_title(title);
    let mut artists = tidy_artists(artist);
    let mut out: Vec<Plan> = Vec::new();
    let mut add = |title: &str, artists: &[String], term_artist: bool| {
        if title.is_empty() {
            return;
        }
        let term = if term_artist && !artists.is_empty() { format!("{} {}", title, artists[0]) } else { title.to_string() };
        if !out.iter().any(|p| p.term == term) {
            out.push(Plan { term, title: title.to_string(), artists: artists.to_vec() });
        }
    };
    // "Kendrick Lamar - Prayer" typed into the title field: the artist is in front of a dash
    let dash = t.split_once(" - ").map(|(l, r)| (l.trim().to_string(), r.trim().to_string()));
    let mut main = t.clone();
    if let Some((l, r)) = &dash {
        if artists.is_empty() || artists.iter().any(|a| closeness(a, l) > 0) {
            main = r.clone();
            if artists.is_empty() {
                artists = split_names(l);
            }
        }
    }
    artists.extend(feats);
    add(&main, &artists, true);
    if !artists.is_empty() {
        add(&main, &artists, false); // the artist may be spelled differently on iTunes
    }
    if let Some((l, r)) = &dash {
        add(r, &split_names(l), true);
        add(l, &artists, true); // "Title - Artist"
    }
    out
}

fn pick_plan(body: &Value, plan: &Plan, blank_ok: bool) -> Option<Hit> {
    let mut best: Option<(u8, Hit)> = None;
    for r in body["results"].as_array()? {
        let (Some(track), Some(by)) = (r["trackName"].as_str(), r["artistName"].as_str()) else { continue };
        let ts = closeness(track, &plan.title);
        let a_all: Vec<&str> = by.split(['&', ',']).collect();
        let a_score = plan.artists.iter().map(|w| closeness(w, by).max(a_all.iter().map(|x| closeness(w, x)).max().unwrap_or(0))).max().unwrap_or(0);
        let score = if plan.artists.is_empty() {
            // no artist known: only an identical title is trusted (the owner confirms before anything changes)
            if blank_ok && ts == 3 { 3 } else { 0 }
        } else if ts > 0 && a_score > 0 {
            ts + a_score
        } else {
            0
        };
        if score == 0 || best.as_ref().is_some_and(|(s, _)| *s >= score) {
            continue;
        }
        // only Apple's own picture servers; the size is part of the file name
        let url = r["artworkUrl100"].as_str().unwrap_or("").replace("100x100bb", "512x512bb");
        let cover = if url.starts_with("https://") && url.split('/').nth(2).is_some_and(|h| h.ends_with(".mzstatic.com")) { url } else { String::new() };
        let album = r["collectionName"].as_str().unwrap_or("");
        let album = album.strip_suffix(" - Single").unwrap_or(album).to_string();
        best = Some((score, Hit { title: track.to_string(), artist: by.to_string(), album, cover }));
    }
    best.map(|(_, h)| h)
}

fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// Ok(None): searched fine, nothing fits (a wrong song is worse than none). Err: no internet or the service failed.
/// `blank_ok`: also search when the artist is unknown and accept an identical title (for "Fix info online").
pub fn find(title: &str, artist: &str, blank_ok: bool) -> Result<Option<Hit>, String> {
    let plans = plans(title, artist);
    if plans.is_empty() || (!blank_ok && plans[0].artists.is_empty()) {
        return Ok(None); // without an artist the match would be a guess
    }
    // reqwest is built without a bundled crypto provider; install `ring` once (Err = already installed)
    let _ = rustls::crypto::ring::default_provider().install_default();
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(4))
        .timeout(Duration::from_secs(6))
        .user_agent("MyMusicPlayer/1.0")
        .build()
        .map_err(|e| e.to_string())?;
    let (mut answered, mut last_err) = (false, String::new());
    for plan in plans.iter().take(4) {
        let url = format!("https://itunes.apple.com/search?media=music&entity=song&limit=15&term={}", encode(&plan.term));
        let body: Result<Value, String> = client
            .get(url)
            .send()
            .map_err(|e| e.without_url().to_string())
            .and_then(|r| if r.status().is_success() { r.bytes().map_err(|e| e.without_url().to_string()) } else { Err(format!("iTunes answered {}", r.status().as_u16())) })
            .and_then(|b| serde_json::from_slice(&b).map_err(|e| e.to_string()));
        match body {
            Ok(b) => {
                answered = true;
                if let Some(hit) = pick_plan(&b, plan, blank_ok) {
                    return Ok(Some(hit));
                }
            }
            Err(e) => last_err = e,
        }
    }
    if answered { Ok(None) } else { Err(last_err) }
}

/// The cover address for the Discord presence (Ok(None): nothing found).
pub fn lookup(title: &str, artist: &str) -> Result<Option<String>, String> {
    Ok(find(title, artist, false)?.map(|h| h.cover).filter(|c| !c.is_empty()))
}

/// What the page shows in "Fix info online": the official tags and the cover, or null.
pub fn info(title: &str, artist: &str) -> Result<Value, String> {
    Ok(match find(title, artist, true)? {
        Some(h) => json!({"title": h.title, "artist": h.artist, "album": h.album, "cover": h.cover}),
        None => Value::Null,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(track: &str, artist: &str, album: &str) -> Value {
        json!({"trackName": track, "artistName": artist, "collectionName": album,
               "artworkUrl100": "https://is1-ssl.mzstatic.com/image/thumb/Music/abc/100x100bb.jpg"})
    }
    const BIG: &str = "https://is1-ssl.mzstatic.com/image/thumb/Music/abc/512x512bb.jpg";
    fn plan(t: &str, a: &[&str]) -> Plan {
        Plan { term: t.into(), title: t.into(), artists: a.iter().map(|s| s.to_string()).collect() }
    }

    #[test]
    fn picks_the_matching_result_in_big_size() {
        let body = json!({"results": [result("Prayer", "Someone Else", "X"), result("Prayer", "Kendrick Lamar", "Prayer - Single")]});
        let h = pick_plan(&body, &plan("prayer", &["kendrick lamar"]), false).unwrap();
        assert_eq!((h.title.as_str(), h.artist.as_str(), h.album.as_str(), h.cover.as_str()), ("Prayer", "Kendrick Lamar", "Prayer", BIG));
    }

    #[test]
    fn a_wrong_song_is_never_returned() {
        let body = json!({"results": [result("Other Song", "Kendrick Lamar", "A"), result("Prayer", "Other Band", "B")]});
        assert_eq!(pick_plan(&body, &plan("Prayer", &["Kendrick Lamar"]), false), None);
        assert_eq!(pick_plan(&json!({"results": []}), &plan("Prayer", &["K"]), false), None);
        assert_eq!(pick_plan(&json!({}), &plan("Prayer", &["Kendrick Lamar"]), false), None);
        // no artist: nothing is trusted unless the caller allows it, and then only an identical title
        let body = json!({"results": [result("Prayer", "Kendrick Lamar", "A")]});
        assert_eq!(pick_plan(&body, &plan("Prayer", &[]), false), None);
        assert!(pick_plan(&body, &plan("Prayer", &[]), true).is_some());
        assert_eq!(pick_plan(&body, &plan("Pray", &[]), true), None);
    }

    #[test]
    fn small_differences_still_match() {
        let body = json!({"results": [result("Beyoncé", "Beyoncé", "A"), result("Halo", "Beyoncé", "I Am... Sasha Fierce")]});
        // accents, a typo, "&" and extra artists
        assert_eq!(pick_plan(&body, &plan("halo", &["Beyonce"]), false).unwrap().title, "Halo");
        assert_eq!(pick_plan(&body, &plan("Hallo", &["Beyonce"]), false).unwrap().title, "Halo");
        let duo = json!({"results": [result("Run This Town", "JAY-Z, Rihanna & Kanye West", "A")]});
        assert!(pick_plan(&duo, &plan("Run This Town", &["Rihanna"]), false).is_some());
        let fa = json!({"results": [result("گل یخ", "محسن چاوشی", "A")]});
        assert!(pick_plan(&fa, &plan("گل‌یخ", &["محسن چاوشي"]), false).is_some(), "ZWNJ and Arabic yeh");
    }

    #[test]
    fn only_apple_picture_servers_give_a_cover() {
        for bad in ["http://is1-ssl.mzstatic.com/a/100x100bb.jpg", "https://evil.example/a/100x100bb.jpg", "https://mzstatic.com.evil.example/a/100x100bb.jpg", "javascript:alert(1)"] {
            let body = json!({"results": [{"trackName": "Prayer", "artistName": "Kendrick Lamar", "artworkUrl100": bad}]});
            assert_eq!(pick_plan(&body, &plan("Prayer", &["Kendrick Lamar"]), false).unwrap().cover, "", "{}", bad);
        }
    }

    #[test]
    fn closeness_levels() {
        assert_eq!(closeness("Prayer", "prayer!"), 3);
        assert_eq!(closeness("Prayer (Explicit)", "Prayer"), 2);
        assert_eq!(closeness("Kendrick Lamar", "Kendrick Lamar feat. SZA"), 2);
        assert_eq!(closeness("Kendrik Lamar", "Kendrick Lamar"), 1);
        assert_eq!(closeness("ab", "abc"), 0, "too short to compare loosely");
        assert_eq!(closeness("Prayer", "Player"), 1);
        assert_eq!(closeness("Prayer", "Pastry"), 0);
    }

    #[test]
    fn titles_are_tidied() {
        let t = |s: &str| tidy_title(s);
        assert_eq!(t("Prayer (feat. X) [Official Video]"), ("Prayer".into(), vec!["X".into()]));
        assert_eq!(t("01 - Prayer - Remastered 2011"), ("Prayer".into(), vec![]));
        assert_eq!(t("Prayer ft. A & B"), ("Prayer".into(), vec!["A".into(), "B".into()]));
        assert_eq!(t("Prayer_Official_Video"), ("Prayer".into(), vec![]));
        assert_eq!(t("Prayer www.site.ir"), ("Prayer".into(), vec![]));
        assert_eq!(t("Prayer 320kbps"), ("Prayer".into(), vec![]));
        assert_eq!(t("Prayer Official Music Video").0, "Prayer");
        assert_eq!(t("Make Some Music").0, "Make Some Music", "ordinary last words stay");
        assert_eq!(t("7 Rings").0, "7 Rings", "a number that is part of the title stays");
        assert_eq!(t("21 Guns").0, "21 Guns");
        assert_eq!(t("Official Video").0, "Official Video", "never empty the title");
        assert_eq!(t("Song - Live in Paris").0, "Song - Live in Paris", "a real subtitle stays");
    }

    #[test]
    fn artists_are_split_and_tidied() {
        assert_eq!(tidy_artists("Kendrick Lamar - Topic"), vec!["Kendrick Lamar"]);
        assert_eq!(tidy_artists("A, B & C"), vec!["A", "B", "C"]);
        assert_eq!(tidy_artists("محسن چاوشی، همایون شجریان"), vec!["محسن چاوشی", "همایون شجریان"]);
        assert_eq!(tidy_artists("Daft Punk feat. Pharrell"), vec!["Daft Punk", "Pharrell"]);
        assert_eq!(tidy_artists("  "), Vec::<String>::new());
    }

    #[test]
    fn plans_cover_the_usual_tag_mistakes() {
        // clean tags: one search with the artist, one with the title alone
        let p = plans("Prayer (Official Video)", "Kendrick Lamar");
        assert_eq!((p[0].term.as_str(), p[0].title.as_str()), ("Prayer Kendrick Lamar", "Prayer"));
        assert_eq!(p[1].term, "Prayer");
        // artist typed in the title, artist field empty
        let p = plans("Kendrick Lamar - Prayer", "");
        assert_eq!((p[0].term.as_str(), p[0].artists.clone()), ("Prayer Kendrick Lamar", vec!["Kendrick Lamar".to_string()]));
        // "Title - Artist" order
        let p = plans("Prayer - Kendrick Lamar", "Kendrick Lamar");
        assert!(p.iter().any(|x| x.title == "Prayer" && x.artists.contains(&"Kendrick Lamar".to_string())), "{:?}", p);
        // nothing to search
        assert!(plans("", "x").is_empty());
        // an empty artist stays empty, so the Discord lookup (blank_ok = false) skips the song
        assert!(plans("Prayer", "  ")[0].artists.is_empty());
        assert_eq!(find("Prayer", "  ", false), Ok(None));
    }
}
