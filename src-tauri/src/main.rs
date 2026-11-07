// در حالت release پنجرهٔ کنسول اضافه باز نشود
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    music_player_lib::run()
}
