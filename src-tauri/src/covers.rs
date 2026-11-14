//! Finds the official cover of a song for the Discord presence (iTunes Search: free, no key).
//! Discord only shows pictures that have an address on the internet, so the cover inside the audio file cannot be used.
//! No Tauri dependency. Only the song title and artist leave the computer, and only when the owner turned the
//! "Look up covers online" switch on.

use std::time::Duration;

use serde_json::Value;

/// Keeps letters and digits (any language) in lower case: "Prayer (Official Video)" and "prayer official video" compare equal.
fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Drops "(feat. ...)", "[Explicit]" and similar notes that make a search fail.
fn clean(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn similar(a: &str, b: &str) -> bool {
    let (a, b) = (norm(a), norm(b));
    a.chars().count() >= 2 && b.chars().count() >= 2 && (a.contains(&b) || b.contains(&a))
}

/// Picks the cover of the first result that really is this song (title and artist both match), as a 512 px https address.
/// Returns None when nothing fits: a wrong cover is worse than the default picture.
pub fn pick_cover(body: &Value, title: &str, artist: &str) -> Option<String> {
    for r in body["results"].as_array()? {
        let (Some(track), Some(by), Some(art)) = (r["trackName"].as_str(), r["artistName"].as_str(), r["artworkUrl100"].as_str()) else { continue };
        if !similar(track, title) || !similar(by, artist) {
            continue;
        }
        // only Apple's own picture servers; the size is part of the file name
        let url = art.replace("100x100bb", "512x512bb");
        if url.starts_with("https://") && url.split('/').nth(2).is_some_and(|h| h.ends_with(".mzstatic.com")) {
            return Some(url);
        }
    }
    None
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

/// Ok(None): searched fine, no cover found. Err: no internet or the service failed (worth trying again later).
pub fn lookup(title: &str, artist: &str) -> Result<Option<String>, String> {
    let (title, artist) = (clean(title), clean(artist));
    if title.is_empty() || artist.is_empty() {
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
    let url = format!("https://itunes.apple.com/search?media=music&entity=song&limit=8&term={}", encode(&format!("{} {}", title, artist)));
    let resp = client.get(url).send().map_err(|e| e.without_url().to_string())?;
    if !resp.status().is_success() {
        return Err(format!("iTunes answered {}", resp.status().as_u16()));
    }
    let bytes = resp.bytes().map_err(|e| e.without_url().to_string())?;
    let body: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(pick_cover(&body, &title, &artist))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn result(track: &str, artist: &str, art: &str) -> Value {
        json!({"trackName": track, "artistName": artist, "artworkUrl100": art})
    }
    const ART: &str = "https://is1-ssl.mzstatic.com/image/thumb/Music/abc/100x100bb.jpg";

    #[test]
    fn picks_the_matching_result_in_big_size() {
        let body = json!({"results": [result("Prayer", "Someone Else", ART), result("Prayer", "Kendrick Lamar", ART)]});
        assert_eq!(
            pick_cover(&body, "prayer", "kendrick lamar").as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/Music/abc/512x512bb.jpg")
        );
    }

    #[test]
    fn a_wrong_cover_is_never_returned() {
        let body = json!({"results": [result("Other Song", "Kendrick Lamar", ART), result("Prayer", "Other Band", ART)]});
        assert_eq!(pick_cover(&body, "Prayer", "Kendrick Lamar"), None);
        assert_eq!(pick_cover(&json!({"results": []}), "Prayer", "Kendrick Lamar"), None);
        assert_eq!(pick_cover(&json!({}), "Prayer", "Kendrick Lamar"), None);
        assert_eq!(pick_cover(&json!({"results": [{"trackName": "Prayer"}]}), "Prayer", "K"), None);
    }

    #[test]
    fn titles_with_notes_and_other_languages_match() {
        let body = json!({"results": [result("Prayer (Explicit)", "Kendrick Lamar", ART)]});
        assert!(pick_cover(&body, "Prayer", "Kendrick Lamar").is_some());
        let fa = json!({"results": [result("گل یخ", "محسن چاوشی", ART)]});
        assert!(pick_cover(&fa, "گل یخ", "محسن چاوشی").is_some());
        let feat = json!({"results": [result("Prayer", "Kendrick Lamar & Friends", ART)]});
        assert!(pick_cover(&feat, "Prayer", "Kendrick Lamar").is_some());
    }

    #[test]
    fn only_apple_picture_servers_are_accepted() {
        for bad in ["http://is1-ssl.mzstatic.com/a/100x100bb.jpg", "https://evil.example/a/100x100bb.jpg", "https://mzstatic.com.evil.example/a/100x100bb.jpg", "javascript:alert(1)"] {
            let body = json!({"results": [result("Prayer", "Kendrick Lamar", bad)]});
            assert_eq!(pick_cover(&body, "Prayer", "Kendrick Lamar"), None, "{}", bad);
        }
    }

    #[test]
    fn helpers() {
        assert_eq!(clean("Prayer (feat. X) [Explicit]  "), "Prayer");
        assert_eq!(clean("a (b (c) d) e"), "a e");
        assert_eq!(encode("a b&c/é"), "a%20b%26c%2F%C3%A9");
        assert!(!similar("a", "a"), "one character is not enough to call two titles alike");
        // an empty artist is not searched at all (no network call is made)
        assert_eq!(lookup("Prayer", "  "), Ok(None));
    }
}
