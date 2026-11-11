use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;
use tauri_plugin_dialog::DialogExt;
use tauri_plugin_fs::FsExt;

mod musicdir;
mod remote;

/// Is `path` really inside the `base` folder? (resolves `..` and links)
fn is_inside(base: &Path, path: &Path) -> Result<bool, String> {
    let base = base.canonicalize().map_err(|e| e.to_string())?;
    let real = path.canonicalize().map_err(|e| e.to_string())?;
    Ok(real != base && real.starts_with(&base))
}

// ---------- music folder (see musicdir.rs) ----------
/// The folder the songs live in right now (default: `<app data>/Music`, or the one chosen in the app).
struct MusicDir {
    cur: Mutex<PathBuf>,
    /// the folder used before the last change: its files may still be moved to the Recycle Bin (after "Move my songs")
    prev: Mutex<Option<PathBuf>>,
}

fn default_music_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app.path().app_data_dir().map_err(|e| e.to_string())?.join("Music"))
}

fn current_music_dir(app: &tauri::AppHandle) -> Result<PathBuf, String> {
    Ok(app.state::<MusicDir>().cur.lock().unwrap().clone())
}

/// Lets the webview read/write the folder and show its files (the static scope only covers the default folder).
fn allow_music_dir(app: &tauri::AppHandle, dir: &Path) -> Result<(), String> {
    app.fs_scope().allow_directory(dir, true).map_err(|e| e.to_string())?;
    app.asset_protocol_scope().allow_directory(dir, true).map_err(|e| e.to_string())
}

fn path_str(p: &Path) -> String {
    musicdir::plain(p.to_path_buf()).to_string_lossy().into_owned()
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MusicDirInfo {
    dir: String,
    default_dir: String,
    is_custom: bool,
    exists: bool,
}

#[tauri::command]
fn music_dir_info(app: tauri::AppHandle) -> Result<MusicDirInfo, String> {
    let def = default_music_dir(&app)?;
    let cur = current_music_dir(&app)?;
    Ok(MusicDirInfo {
        dir: path_str(&cur),
        default_dir: path_str(&def),
        is_custom: !musicdir::same_dir(&cur, &def),
        exists: cur.is_dir(),
    })
}

/// Opens the system "choose folder" window. None = the user cancelled.
#[tauri::command]
async fn pick_music_folder(app: tauri::AppHandle) -> Result<Option<String>, String> {
    tauri::async_runtime::spawn_blocking(move || {
        let mut d = app.dialog().file().set_title("Choose your music folder");
        if let Some(w) = app.get_webview_window("main") {
            d = d.set_parent(&w);
        }
        match d.blocking_pick_folder() {
            None => Ok(None),
            Some(f) => f.into_path().map(|p| Some(path_str(&p))).map_err(|e| e.to_string()),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct FolderCheck {
    path: String,
    audio_files: usize,
    same: bool,
}

/// Is this folder usable as the music folder? Also lets the webview use it (needed to look inside before switching).
#[tauri::command]
async fn check_music_dir(app: tauri::AppHandle, path: String) -> Result<FolderCheck, String> {
    // (counting songs can take a moment on a big folder: keep it off the UI thread)
    tauri::async_runtime::spawn_blocking(move || check_music_dir_sync(&app, &path))
        .await
        .map_err(|e| e.to_string())?
}

fn check_music_dir_sync(app: &tauri::AppHandle, path: &str) -> Result<FolderCheck, String> {
    let appdata = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let def = default_music_dir(app)?;
    let cur = current_music_dir(app)?;
    let c = musicdir::validate(Path::new(path), &appdata, &def, &cur, &musicdir::system_dirs())?;
    allow_music_dir(app, &c.path)?;
    Ok(FolderCheck { path: path_str(&c.path), audio_files: c.audio_files, same: c.same })
}

/// Makes `path` the music folder (None = back to the default one) and remembers it for the next start.
#[tauri::command]
async fn save_music_dir(app: tauri::AppHandle, path: Option<String>) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || save_music_dir_sync(&app, path))
        .await
        .map_err(|e| e.to_string())?
}

fn save_music_dir_sync(app: &tauri::AppHandle, path: Option<String>) -> Result<String, String> {
    let appdata = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let def = default_music_dir(app)?;
    let state = app.state::<MusicDir>();
    let mut slot = state.cur.lock().unwrap();
    let (dir, custom) = match path {
        Some(p) => {
            let c = musicdir::validate(Path::new(&p), &appdata, &def, &slot, &musicdir::system_dirs())?;
            let is_def = musicdir::same_dir(&c.path, &def);
            (c.path, !is_def)
        }
        None => {
            std::fs::create_dir_all(&def).map_err(|e| e.to_string())?;
            (def.clone(), false)
        }
    };
    allow_music_dir(app, &dir)?;
    musicdir::write_setting(&appdata, if custom { Some(dir.as_path()) } else { None })?;
    if !musicdir::same_dir(&slot, &dir) {
        *state.prev.lock().unwrap() = Some(slot.clone());
    }
    *slot = dir.clone();
    Ok(path_str(&dir))
}

/// Moves a file to the Windows Recycle Bin. Only files inside the music folder are allowed.
#[tauri::command]
async fn move_to_trash(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let music = current_music_dir(&app)?;
    let prev = app.state::<MusicDir>().prev.lock().unwrap().clone();
    tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        let ok = is_inside(&music, &p)?
            || match &prev {
                Some(pv) => is_inside(pv, &p).unwrap_or(false),
                None => false,
            };
        if !ok {
            return Err("path is outside the music folder".to_string());
        }
        trash::delete(&p).map_err(|e| e.to_string())
    })
    .await
    .map_err(|e| e.to_string())?
}

// ---------- phone remote (see remote.rs) ----------
#[derive(Default)]
struct RemoteState(Mutex<Option<Arc<remote::Remote>>>);

#[derive(serde::Serialize)]
struct RemoteInfo {
    port: u16,
    addrs: Vec<String>,
}

fn current_remote(state: &RemoteState) -> Option<Arc<remote::Remote>> {
    state.0.lock().unwrap().clone()
}

/// Starts (or restarts) the phone-remote web server. `token` is the secret part of the link.
#[tauri::command]
fn remote_start(
    app: tauri::AppHandle,
    state: tauri::State<'_, RemoteState>,
    port: u16,
    token: String,
) -> Result<RemoteInfo, String> {
    let incoming = app.path().app_data_dir().map_err(|e| e.to_string())?.join("incoming");
    let mut slot = state.0.lock().unwrap();
    if let Some(old) = slot.take() {
        old.stop();
    }
    let r = Arc::new(remote::Remote::start(remote::Config { port, token, incoming })?);
    let info = RemoteInfo { port: r.port(), addrs: remote::local_addrs() };
    *slot = Some(r);
    Ok(info)
}

#[tauri::command]
fn remote_stop(state: tauri::State<'_, RemoteState>) {
    if let Some(r) = state.0.lock().unwrap().take() {
        r.stop();
    }
}

/// Waits for commands from the phone (up to `timeout_ms`). Empty list = nothing yet.
#[tauri::command]
async fn remote_poll(state: tauri::State<'_, RemoteState>, timeout_ms: u64) -> Result<Vec<String>, String> {
    let Some(r) = current_remote(&state) else {
        return Ok(Vec::new());
    };
    tauri::async_runtime::spawn_blocking(move || r.poll(Duration::from_millis(timeout_ms.min(30_000))))
        .await
        .map_err(|e| e.to_string())
}

/// kind: "state" | "library" | "notice"
#[tauri::command]
fn remote_push(state: tauri::State<'_, RemoteState>, kind: String, data: String) -> Result<(), String> {
    let Some(r) = current_remote(&state) else {
        return Ok(());
    };
    match kind.as_str() {
        "state" => r.push_state(data),
        "library" => r.push_library(data),
        "notice" => {
            r.push_notice(data);
            Ok(())
        }
        _ => Err("unknown kind".into()),
    }
}

#[tauri::command]
fn remote_cover(state: tauri::State<'_, RemoteState>, bytes: Vec<u8>) {
    if let Some(r) = current_remote(&state) {
        r.push_cover(bytes);
    }
}

#[tauri::command]
fn remote_qr(text: String) -> Result<String, String> {
    remote::qr_svg(&text)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let mut builder = tauri::Builder::default();
    // Single instance (must be the first plugin): launching the app again brings the
    // existing window to the front instead of opening a second one.
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.unminimize();
                let _ = w.show();
                let _ = w.set_focus();
            }
        }));
    }
    builder
        // Auto-update: check for a new version, download, install
        .plugin(tauri_plugin_updater::Builder::new().build())
        // Restart the app after an update
        .plugin(tauri_plugin_process::init())
        // Read/write the music folder
        .plugin(tauri_plugin_fs::init())
        // Open the music folder in Explorer
        .plugin(tauri_plugin_opener::init())
        // Choose the music folder
        .plugin(tauri_plugin_dialog::init())
        // Phone remote server state
        .manage(RemoteState::default())
        .setup(|app| {
            // Music folder: the one saved in settings.json, else the default one.
            let handle = app.handle().clone();
            let appdata = handle.path().app_data_dir()?;
            let def = appdata.join("Music");
            let dir = musicdir::read_setting(&appdata).unwrap_or_else(|| def.clone());
            if !musicdir::same_dir(&dir, &def) {
                // allowed even when the folder is missing right now (unplugged drive): it may come back
                let _ = allow_music_dir(&handle, &dir);
            }
            app.manage(MusicDir { cur: Mutex::new(dir), prev: Mutex::new(None) });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            move_to_trash,
            music_dir_info,
            pick_music_folder,
            check_music_dir,
            save_music_dir,
            remote_start,
            remote_stop,
            remote_poll,
            remote_push,
            remote_cover,
            remote_qr
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mp_trash_test_{}_{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn file_inside_is_allowed() {
        let d = tmpdir("in");
        let m = d.join("Music");
        fs::create_dir_all(m.join("sub")).unwrap();
        fs::write(m.join("a.mp3"), b"x").unwrap();
        fs::write(m.join("sub").join("b.mp3"), b"x").unwrap();
        assert!(is_inside(&m, &m.join("a.mp3")).unwrap());
        assert!(is_inside(&m, &m.join("sub").join("b.mp3")).unwrap());
    }

    #[test]
    fn outside_and_traversal_are_rejected() {
        let d = tmpdir("out");
        let m = d.join("Music");
        fs::create_dir_all(&m).unwrap();
        fs::write(d.join("secret.txt"), b"x").unwrap();
        fs::write(m.join("a.mp3"), b"x").unwrap();
        assert!(!is_inside(&m, &d.join("secret.txt")).unwrap());
        assert!(!is_inside(&m, &m.join("..").join("secret.txt")).unwrap());
        assert!(!is_inside(&m, &m).unwrap());
        // A folder with the same prefix (Music2) must not count as inside Music
        let m2 = d.join("Music2");
        fs::create_dir_all(&m2).unwrap();
        fs::write(m2.join("c.mp3"), b"x").unwrap();
        assert!(!is_inside(&m, &m2.join("c.mp3")).unwrap());
        assert!(is_inside(&m, &m.join("nope.mp3")).is_err());
    }
}
