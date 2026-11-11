//! Where the music folder is. Nothing here depends on Tauri, so it can be tested on its own.
//!
//! The choice is kept in `settings.json` in the app data folder: `{"musicDir": "D:\\Music"}`.
//! No setting = the default folder (`<app data>/Music`). The file can be edited by hand too; it is read at start-up.

use std::fs;
use std::path::{Path, PathBuf};

const SETTINGS: &str = "settings.json";
const AUDIO_EXT: [&str; 10] = ["mp3", "m4a", "flac", "wav", "ogg", "oga", "opus", "aac", "webm", "wma"];

/// `\\?\C:\x` -> `C:\x` (canonicalize gives the long form on Windows, which the rest of the app does not use).
pub fn plain(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if !rest.starts_with("UNC\\") {
            return PathBuf::from(rest);
        }
    }
    p
}

fn canon(p: &Path) -> PathBuf {
    plain(p.canonicalize().unwrap_or_else(|_| p.to_path_buf()))
}

/// Same folder? (ignores case/links/`\\?\\` when both exist, otherwise compares as written)
pub fn same_dir(a: &Path, b: &Path) -> bool {
    canon(a) == canon(b)
}

/// The folder saved in settings.json, if any (an empty or broken file counts as "none").
pub fn read_setting(appdata: &Path) -> Option<PathBuf> {
    let txt = fs::read_to_string(appdata.join(SETTINGS)).ok()?;
    let v: serde_json::Value = serde_json::from_str(txt.trim_start_matches('\u{feff}')).ok()?;
    let s = v.get("musicDir")?.as_str()?.trim();
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

/// Saves the folder (None = back to the default). Other keys in the file are kept. Written to a temp file first.
pub fn write_setting(appdata: &Path, dir: Option<&Path>) -> Result<(), String> {
    fs::create_dir_all(appdata).map_err(|e| e.to_string())?;
    let path = appdata.join(SETTINGS);
    let mut v: serde_json::Value = fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(t.trim_start_matches('\u{feff}')).ok())
        .filter(|v: &serde_json::Value| v.is_object())
        .unwrap_or_else(|| serde_json::json!({}));
    let obj = v.as_object_mut().unwrap();
    match dir {
        Some(d) => {
            obj.insert("musicDir".into(), serde_json::Value::String(d.to_string_lossy().into_owned()));
        }
        None => {
            obj.remove("musicDir");
        }
    }
    let tmp = appdata.join("settings.json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&v).unwrap()).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })
}

/// System folders that must never be used as a music folder (taken from the environment on Windows).
pub fn system_dirs() -> Vec<PathBuf> {
    ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)", "ProgramData"]
        .iter()
        .filter_map(|k| std::env::var_os(k))
        .map(PathBuf::from)
        .collect()
}

#[derive(Debug)]
pub struct Checked {
    pub path: PathBuf,
    /// audio files found in the folder (up to 3 levels deep, counting stops at 50 000)
    pub audio_files: usize,
    /// is it the folder in use already?
    pub same: bool,
}

fn count_audio(dir: &Path, depth: u32, n: &mut usize) {
    if depth > 3 || *n >= 50_000 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        match e.file_type() {
            Ok(t) if t.is_dir() => count_audio(&p, depth + 1, n),
            Ok(t) if t.is_file() => {
                if let Some(x) = p.extension().and_then(|x| x.to_str()) {
                    if AUDIO_EXT.contains(&x.to_ascii_lowercase().as_str()) {
                        *n += 1;
                    }
                }
            }
            _ => {}
        }
    }
}

fn inside(child: &Path, parent: &Path) -> bool {
    child.starts_with(parent)
}

/// Checks that `candidate` can be the music folder. `default_dir` is `<app data>/Music`, `current` the folder in use now.
pub fn validate(candidate: &Path, appdata: &Path, default_dir: &Path, current: &Path, system: &[PathBuf]) -> Result<Checked, String> {
    let meta = fs::metadata(candidate).map_err(|_| "That folder does not exist.".to_string())?;
    if !meta.is_dir() {
        return Err("That is not a folder.".into());
    }
    let cand = canon(candidate);
    if cand.parent().is_none() {
        return Err("Choose a folder, not a whole drive.".into());
    }
    for s in system {
        let s = canon(s);
        if inside(&cand, &s) || inside(&s, &cand) {
            return Err("That is a system folder. Choose a folder of your own, for example one named Music.".into());
        }
    }
    let app = canon(appdata);
    let def = canon(default_dir);
    if cand != def && (inside(&cand, &app) || inside(&app, &cand)) {
        return Err("That folder is used by the app for its own data. Choose another one.".into());
    }
    let cur = canon(current);
    let same = cand == cur;
    if !same && cur.exists() && (inside(&cand, &cur) || inside(&cur, &cand)) {
        return Err("The new folder can't be inside the current music folder, and it can't contain it.".into());
    }
    // can we write there?
    let probe = cand.join(format!(".mp-write-test-{}", std::process::id()));
    match fs::write(&probe, b"x") {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
        }
        Err(_) => return Err("The app can't write in that folder (it may be read-only, or you may not have permission).".into()),
    }
    let mut n = 0;
    count_audio(&cand, 0, &mut n);
    Ok(Checked { path: plain(candidate.to_path_buf()), audio_files: n, same })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mp_md_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn setting_roundtrip_keeps_other_keys() {
        let d = tmp("set");
        assert_eq!(read_setting(&d), None);
        fs::write(d.join(SETTINGS), r#"{"other": 5}"#).unwrap();
        write_setting(&d, Some(Path::new("D:\\Music Ü"))).unwrap();
        assert_eq!(read_setting(&d), Some(PathBuf::from("D:\\Music Ü")));
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(d.join(SETTINGS)).unwrap()).unwrap();
        assert_eq!(v["other"], 5);
        write_setting(&d, None).unwrap();
        assert_eq!(read_setting(&d), None);
        let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(d.join(SETTINGS)).unwrap()).unwrap();
        assert_eq!(v["other"], 5);
        assert!(!d.join("settings.json.tmp").exists());
    }

    #[test]
    fn setting_tolerates_garbage_bom_and_blank() {
        let d = tmp("bad");
        fs::write(d.join(SETTINGS), "not json").unwrap();
        assert_eq!(read_setting(&d), None);
        fs::write(d.join(SETTINGS), "\u{feff}{\"musicDir\": \"  \"}").unwrap();
        assert_eq!(read_setting(&d), None);
        fs::write(d.join(SETTINGS), "\u{feff}{\"musicDir\": \" E:\\\\Songs \"}").unwrap();
        assert_eq!(read_setting(&d), Some(PathBuf::from("E:\\Songs")));
        // a broken file is replaced, not appended to
        fs::write(d.join(SETTINGS), "[1,2]").unwrap();
        write_setting(&d, Some(Path::new("/x"))).unwrap();
        assert_eq!(read_setting(&d), Some(PathBuf::from("/x")));
    }

    #[test]
    fn plain_strips_verbatim_prefix_only() {
        assert_eq!(plain(PathBuf::from(r"\\?\C:\Music")), PathBuf::from(r"C:\Music"));
        assert_eq!(plain(PathBuf::from(r"\\?\UNC\srv\share")), PathBuf::from(r"\\?\UNC\srv\share"));
        assert_eq!(plain(PathBuf::from("/home/x")), PathBuf::from("/home/x"));
    }

    #[test]
    fn validate_rules() {
        let root = tmp("val");
        let app = root.join("app");
        let def = app.join("Music");
        let cur = root.join("cur");
        let new = root.join("new");
        let sys = root.join("sys");
        for d in [&def, &cur, &new, &sys, &app.join("data")] {
            fs::create_dir_all(d).unwrap();
        }
        fs::write(new.join("a.MP3"), b"x").unwrap();
        fs::create_dir_all(new.join("sub")).unwrap();
        fs::write(new.join("sub").join("b.flac"), b"x").unwrap();
        fs::write(new.join("notes.txt"), b"x").unwrap();
        let systems = vec![sys.clone()];
        // ok + counts songs, no probe file left
        let c = validate(&new, &app, &def, &cur, &systems).unwrap();
        assert_eq!(c.audio_files, 2);
        assert!(!c.same);
        assert_eq!(fs::read_dir(&new).unwrap().count(), 3);
        // same as the current one
        assert!(validate(&cur, &app, &def, &cur, &systems).unwrap().same);
        // missing / a file
        assert!(validate(&root.join("nope"), &app, &def, &cur, &systems).is_err());
        assert!(validate(&new.join("notes.txt"), &app, &def, &cur, &systems).unwrap_err().contains("not a folder"));
        // system folder, and a folder inside it
        assert!(validate(&sys, &app, &def, &cur, &systems).unwrap_err().contains("system"));
        fs::create_dir_all(sys.join("x")).unwrap();
        assert!(validate(&sys.join("x"), &app, &def, &cur, &systems).is_err());
        // app data: only the default music folder is allowed
        assert!(validate(&app, &app, &def, &cur, &systems).is_err());
        assert!(validate(&app.join("data"), &app, &def, &cur, &systems).is_err());
        assert!(validate(&def, &app, &def, &cur, &systems).is_ok());
        // nested with the current folder
        fs::create_dir_all(cur.join("inner")).unwrap();
        assert!(validate(&cur.join("inner"), &app, &def, &cur, &systems).unwrap_err().contains("inside the current"));
        assert!(validate(&root, &app, &def, &cur, &systems).is_err());
        // current folder gone (unplugged drive): nesting rule does not apply
        let gone = root.join("gone");
        assert!(validate(&new, &app, &def, &gone, &systems).is_ok());
        // a drive root
        assert!(validate(Path::new("/"), &app, &def, &cur, &[]).unwrap_err().contains("whole drive"));
    }

    #[cfg(unix)]
    #[test]
    fn read_only_folder_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = tmp("ro");
        let ro = root.join("ro");
        fs::create_dir_all(&ro).unwrap();
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o555)).unwrap();
        // root can write anywhere: only assert when the probe really fails
        let can_write = fs::write(ro.join("t"), b"x").is_ok();
        let _ = fs::remove_file(ro.join("t"));
        let r = validate(&ro, &root.join("app"), &root.join("app").join("Music"), &root.join("cur"), &[]);
        if can_write { assert!(r.is_ok()); } else { assert!(r.unwrap_err().contains("can't write")); }
        fs::set_permissions(&ro, fs::Permissions::from_mode(0o755)).unwrap();
    }
}
