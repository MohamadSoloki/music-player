use std::path::{Path, PathBuf};
use tauri::Manager;

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
        // Move a song file to the Recycle Bin
        .invoke_handler(tauri::generate_handler![move_to_trash])
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
