#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // آپدیت خودکار: بررسی نسخهٔ جدید، دانلود، نصب
        .plugin(tauri_plugin_updater::Builder::new().build())
        // راه‌اندازی دوبارهٔ برنامه بعد از آپدیت
        .plugin(tauri_plugin_process::init())
        // خواندن/نوشتن پوشهٔ آهنگ‌ها
        .plugin(tauri_plugin_fs::init())
        // باز کردن پوشهٔ آهنگ‌ها در Explorer
        .plugin(tauri_plugin_opener::init())
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
