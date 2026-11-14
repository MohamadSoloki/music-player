//! Windows taskbar thumbnail buttons: Previous / Play-Pause / Next under the preview that shows
//! when the mouse hovers over the app's taskbar button.
//!
//! The module does not depend on Tauri: `install` gets the window handle (as an integer) and a
//! callback that receives "prev", "toggle" or "next". Everything that touches the shell runs on the
//! window's own thread (inside a window subclass), so no COM object is shared between threads.
//! The buttons are added again every time Windows creates the taskbar button of the window
//! (the window was hidden to the tray and shown again, or Explorer restarted).

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::core::{w, IUnknown};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
use windows::Win32::UI::Shell::{
    DefSubclassProc, ITaskbarList3, SetWindowSubclass, TaskbarList, THBF_ENABLED, THB_FLAGS, THB_ICON, THB_TOOLTIP,
    THUMBBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateIconIndirect, PostMessageW, RegisterWindowMessageW, HICON, ICONINFO, WM_APP, WM_COMMAND,
};

const ID_PREV: u32 = 0;
const ID_TOGGLE: u32 = 1;
const ID_NEXT: u32 = 2;
/// "a thumbnail button was clicked": the high word of `wParam` of `WM_COMMAND`
const THBN_CLICKED: usize = 0x1800;
/// our own message: add the buttons / refresh the Play-Pause icon (runs on the window thread)
const WM_THUMB_SYNC: u32 = WM_APP + 0x31;
const SUBCLASS_ID: usize = 0x4d50_5442;

static HWND_RAW: AtomicIsize = AtomicIsize::new(0);
static PLAYING: AtomicBool = AtomicBool::new(false);
static CREATED_MSG: AtomicU32 = AtomicU32::new(0);
static CALLBACK: OnceLock<Box<dyn Fn(&'static str) + Send + Sync>> = OnceLock::new();
/// (prev, play, pause, next) icons, built once on the window thread
static ICONS: Mutex<Option<[isize; 4]>> = Mutex::new(None);

/// Draws a 32x32 glyph: white with a thin dark outline so it shows on light and dark taskbars.
/// kind: 0 previous, 1 play, 2 pause, 3 next. Returns premultiplied BGRA pixels.
fn glyph(kind: u8) -> Vec<u8> {
    const N: i32 = 32;
    let inside = |x: i32, y: i32| -> bool {
        if x < 0 || y < 0 || x >= N || y >= N {
            return false;
        }
        let (xf, yf) = (x as f32 + 0.5, y as f32 + 0.5);
        let tri = |x0: f32, x1: f32, half: f32, right: bool| -> bool {
            // a triangle between x0 and x1, pointing right (apex at x1) or left (apex at x0)
            if xf < x0 || xf > x1 {
                return false;
            }
            let t = if right { (x1 - xf) / (x1 - x0) } else { (xf - x0) / (x1 - x0) };
            (yf - 16.0).abs() <= half * t
        };
        match kind {
            0 => (xf >= 7.0 && xf < 11.0 && yf >= 7.0 && yf < 25.0) || tri(11.0, 25.0, 9.0, false),
            1 => tri(9.0, 26.0, 10.0, true),
            2 => ((xf >= 8.0 && xf < 13.0) || (xf >= 19.0 && xf < 24.0)) && yf >= 7.0 && yf < 25.0,
            _ => (xf >= 21.0 && xf < 25.0 && yf >= 7.0 && yf < 25.0) || tri(7.0, 21.0, 9.0, true),
        }
    };
    let mut px = vec![0u8; (N * N * 4) as usize];
    for y in 0..N {
        for x in 0..N {
            let i = ((y * N + x) * 4) as usize;
            if inside(x, y) {
                px[i..i + 4].copy_from_slice(&[255, 255, 255, 255]);
            } else if inside(x - 1, y) || inside(x + 1, y) || inside(x, y - 1) || inside(x, y + 1) {
                // dark outline, premultiplied: 0x20 * 200 / 255
                px[i..i + 4].copy_from_slice(&[25, 25, 25, 200]);
            }
        }
    }
    px
}

fn make_icon(kind: u8) -> Option<HICON> {
    let px = glyph(kind);
    let mask = vec![0u8; 32 * 4]; // 1 bit per pixel, all zero: the alpha channel decides
    unsafe {
        let color = CreateBitmap(32, 32, 1, 32, Some(px.as_ptr() as *const _));
        let mono = CreateBitmap(32, 32, 1, 1, Some(mask.as_ptr() as *const _));
        let info = ICONINFO { fIcon: true.into(), xHotspot: 0, yHotspot: 0, hbmMask: mono, hbmColor: color };
        let icon = CreateIconIndirect(&info).ok();
        let _ = DeleteObject(color.into());
        let _ = DeleteObject(mono.into());
        icon
    }
}

fn icons() -> Option<[HICON; 4]> {
    let mut guard = ICONS.lock().ok()?;
    if guard.is_none() {
        let mut raw = [0isize; 4];
        for (i, slot) in raw.iter_mut().enumerate() {
            *slot = make_icon(i as u8)?.0 as isize;
        }
        *guard = Some(raw);
    }
    let raw = guard.as_ref()?;
    Some([0, 1, 2, 3].map(|i| HICON(raw[i] as *mut _)))
}

fn tip(text: &str) -> [u16; 260] {
    let mut out = [0u16; 260];
    for (i, c) in text.encode_utf16().take(259).enumerate() {
        out[i] = c;
    }
    out
}

fn buttons(playing: bool) -> Option<[THUMBBUTTON; 3]> {
    let [prev, play, pause, next] = icons()?;
    let mk = |id: u32, icon: HICON, text: &str| THUMBBUTTON {
        dwMask: THB_ICON | THB_TOOLTIP | THB_FLAGS,
        iId: id,
        iBitmap: 0,
        hIcon: icon,
        szTip: tip(text),
        dwFlags: THBF_ENABLED,
    };
    Some([
        mk(ID_PREV, prev, "Previous"),
        mk(ID_TOGGLE, if playing { pause } else { play }, if playing { "Pause" } else { "Play" }),
        mk(ID_NEXT, next, "Next"),
    ])
}

/// Runs on the window thread. `add`: first time for this taskbar button, otherwise only refresh the icon.
fn sync(hwnd: HWND, add: bool) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let Ok(list) = CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None::<&IUnknown>, CLSCTX_INPROC_SERVER) else { return };
        if list.HrInit().is_err() {
            return;
        }
        let Some(btns) = buttons(PLAYING.load(Ordering::Relaxed)) else { return };
        // errors are ignored on purpose: before the taskbar button exists the call fails, and the
        // "TaskbarButtonCreated" message brings us back here
        if add {
            let _ = list.ThumbBarAddButtons(hwnd, &btns);
        } else {
            let _ = list.ThumbBarUpdateButtons(hwnd, &btns);
        }
    }
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg != 0 && msg == CREATED_MSG.load(Ordering::Relaxed) {
        sync(hwnd, true);
    } else if msg == WM_THUMB_SYNC {
        // lparam 1: first try to add the buttons (they may already exist: then the update does the job)
        if lparam.0 == 1 {
            sync(hwnd, true);
        }
        sync(hwnd, false);
        return LRESULT(0);
    } else if msg == WM_COMMAND && ((wparam.0 >> 16) & 0xffff) == THBN_CLICKED {
        let cmd = match (wparam.0 & 0xffff) as u32 {
            ID_PREV => Some("prev"),
            ID_TOGGLE => Some("toggle"),
            ID_NEXT => Some("next"),
            _ => None,
        };
        if let (Some(cmd), Some(cb)) = (cmd, CALLBACK.get()) {
            cb(cmd);
            return LRESULT(0);
        }
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

/// Call once, on the thread that owns the window (Tauri's `setup` hook).
pub fn install(hwnd: isize, on_click: Box<dyn Fn(&'static str) + Send + Sync>) {
    if hwnd == 0 || CALLBACK.set(on_click).is_err() {
        return;
    }
    HWND_RAW.store(hwnd, Ordering::Relaxed);
    let h = HWND(hwnd as *mut _);
    unsafe {
        CREATED_MSG.store(RegisterWindowMessageW(w!("TaskbarButtonCreated")), Ordering::Relaxed);
        if !SetWindowSubclass(h, Some(proc), SUBCLASS_ID, 0).as_bool() {
            return;
        }
        // the taskbar button may already exist: try now (a failure is harmless, see `sync`)
        let _ = PostMessageW(Some(h), WM_THUMB_SYNC, WPARAM(0), LPARAM(1));
    }
}

/// The page tells whether music is playing: the middle button shows Pause or Play.
/// Safe to call from any thread: the work is posted to the window thread.
pub fn set_playing(playing: bool) {
    if PLAYING.swap(playing, Ordering::Relaxed) == playing {
        return;
    }
    let raw = HWND_RAW.load(Ordering::Relaxed);
    if raw != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(raw as *mut _)), WM_THUMB_SYNC, WPARAM(0), LPARAM(0));
        }
    }
}
