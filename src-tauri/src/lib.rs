use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tauri::Manager;

mod remote;

/// Is `path` really inside the `base` folder? (resolves `..` and links)
fn is_inside(base: &Path, path: &Path) -> Result<bool, String> {
    let base = base.canonicalize().map_err(|e| e.to_string())?;
    let real = path.canonicalize().map_err(|e| e.to_string())?;
    Ok(real != base && real.starts_with(&base))
}

/// Moves a file to the Windows Recycle Bin. Only files inside the music folder are allowed.
#[tauri::command]
async fn move_to_trash(app: tauri::AppHandle, path: String) -> Result<(), String> {
    let music = app
        .path()
        .app_data_dir()
        .map_err(|e| e.to_string())?
        .join("Music");
    tauri::async_runtime::spawn_blocking(move || {
        let p = PathBuf::from(&path);
        if !is_inside(&music, &p)? {
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
        // Phone remote server state
        .manage(RemoteState::default())
        .invoke_handler(tauri::generate_handler![
            move_to_trash,
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
