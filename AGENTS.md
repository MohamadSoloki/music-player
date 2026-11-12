# AGENTS.md

Instructions for AI coding agents working on this repository. Humans: see `README.md` (written in Persian).

## What this is

A personal offline music player for Windows, built with Tauri 2. The whole UI and app logic live in one file, `src/index.html` (HTML + CSS + JS, no framework, no bundler). The Rust side (`src-tauri/`) is small: plugin setup plus one custom command.

Owner: Disrespect (runs an internet café). Single user, personal use. Prefer simple, robust solutions over generality.

## Language rules

- **Talk to the owner in Persian.** Keep replies short and plain.
- **All text inside the app is English** (buttons, toasts, tooltips, dialogs, errors). The UI is LTR. Do not add Persian strings to the UI.
- `README.md` is Persian. Keep it in sync when behaviour changes. Code comments, commit messages and this file are English.
- Song titles, artists and lyrics can be Persian: use `dir="auto"` on elements that show user text.

## Layout

| Path | Purpose |
| --- | --- |
| `src/index.html` | The entire front end (CSS, markup, JS). |
| `src-tauri/src/lib.rs` | Tauri builder, plugins (single-instance, updater, process, fs, opener, dialog), commands `move_to_trash`, `music_dir_info`, `pick_music_folder`, `check_music_dir`, `save_music_dir` and `remote_*`. |
| `src-tauri/src/musicdir.rs` | Music folder location: `settings.json` read/write and folder validation. No Tauri dependency, has unit tests. |
| `src-tauri/src/remote.rs` | Phone remote: tiny HTTP server (tiny_http), independent of Tauri so it can be tested alone. Serves `remote.html`, state/library/cover, commands, uploads, download-by-link. |
| `src-tauri/src/remote.html` | The phone web page (embedded into the binary with `include_str!`). English UI. |
| `src-tauri/tauri.conf.json` | App config, updater public key and endpoint, asset protocol scope. |
| `src-tauri/tauri.dev.conf.json` | Overlay used by `npm run dev` (separate identifier so dev data stays apart from the installed app). |
| `src-tauri/capabilities/default.json` | Permissions for the webview (fs scopes, updater, opener, ...). |
| `.github/workflows/release.yml` | Builds the Windows installer and publishes a GitHub Release. |

## Commands

```
npm install          # once
npm run dev          # run locally with live window (Windows, needs Rust + MSVC build tools)
```

Releases are built by GitHub Actions (Actions tab, Release, Run workflow, enter a version higher than the last one). The workflow patches the version and the updater endpoint into `tauri.conf.json` at build time; do not hand-edit them for a release.

The Linux sandbox that agents usually run in cannot compile the Tauri app (no GTK/WebKit). Do not claim the Windows build was tested unless it really was; say what was verified and how.

## Architecture notes for `src/index.html`

- **Storage.** In the Windows app ("FOLDER mode") songs are real files in `%APPDATA%\com.disrespect.musicplayer\Music`; each track record stores its relative path in `t.file`. Metadata, playlists, covers and play counts live in IndexedDB (stores `tracks`, `playlists`, `audio`, `covers`) and are mirrored to `data\library.json` (+ `covers\`, `library.prev.json`) as a backup that is restored automatically if IndexedDB is empty. Anything that changes library data must go through the existing save/backup path (`dbPut` / `dbDel` followed by `renderAll()`), not around it.
- **Performance is a feature.** The target machine has low RAM/CPU. The song list is virtualized (`ROW_H`, `renderRows`), cover object URLs are kept in an LRU (`COVER_CAP`), and the Web Audio graph (EQ, pitch shifter) is created lazily and only when needed. Do not render whole libraries into the DOM and do not keep large blobs in memory.
- **Safety rules already in place, keep them:** deleting a song moves it to the Recycle Bin via `move_to_trash` (validated to stay inside the Music folder), never a hard delete; duplicate songs are rejected by size + byte comparison; ID3 tag writing is opt-in, writes to a temp file, verifies, then renames, and refuses ID3v2.2.
- **Music folder location.** Default is `<app data>/Music`; the user can pick another folder (sidebar footer, gear button, "Music folder" dialog). The choice is stored in `<app data>/settings.json` (`{"musicDir": "..."}`; no key = default) and read at start by Rust (`MusicDir` state, `music_dir_info`). Rust also adds the chosen folder to the fs and asset-protocol scopes at runtime (the static scope only covers the default one), so no capability change is needed. `check_music_dir` validates a folder (exists, writable, not a drive root or system folder, not inside app data except the default one, not nested with the current folder); `save_music_dir` re-validates and persists. In the page: `MUSIC_DIR` (current), `APP_DIR` (app data, used for `incoming`), `MUSIC` flags (`custom`, `missing`, `busy`). Choosing a folder offers "Move my songs" (copy songs and their `.lrc` with verified sizes, switch, then old files go to the Recycle Bin; `move_to_trash` also accepts the previous folder after a switch; a failure or cancel removes the copies and changes nothing) or "Use the folder as is" (songs not found there leave the library on the following scan, files are never deleted). The page notices manual changes: on window focus (throttled to 20 s) it checks the folder and rescans; a missing folder shows the red `#folderBar` (library untouched, adding songs refused) with Try again / Choose folder / Use default folder. `scanFolder` never removes many songs by itself: if 5 or more and at least half of the library vanished, only a manual refresh asks and removes. If app data was wiped while the custom folder is missing, the backup is kept (`PENDING_LIB`) and applied after the folder is found. Editing `settings.json` by hand works but only takes effect after a restart.
- **Tray.** `lib.rs` (`setup_tray`, needs the `tray-icon` feature of `tauri`) puts an icon in the Windows tray. The window's close button only hides the window (`on_window_event`), so playback and the phone remote keep running; the only real exit is Quit in the tray menu (or an update restart). Left click on the icon shows the window. Menu items Play/Pause, Previous, Next emit the event `tray-cmd` that the page handles at the end of `index.html`; Quit makes the page save the resume position and backup, then call `quit_app` (Rust exits by itself after 4 s if the page does not answer).
- **Phone remote.** `remote.rs` is a small server on the local network; every URL starts with `/r/<token>/` and anything else is an empty 404. The webview stays the player: it publishes state/library/cover with `remote_push` / `remote_cover` and takes phone commands by long polling `remote_poll` (`remLoop`, `remExec` in `index.html`). Uploads and downloads land in `<app data>/incoming` and the app imports them through `addFilesToFolder` (same duplicate check as normal adding). The phone can never send the internal `upload` command, and `remImport` only touches files inside `incoming`. tiny_http cannot close kept-alive connections, so a stopped server leaves phone requests hanging until their own timeouts: keep the client-side timeouts in `remote.html`. The phone page has four tabs (Now, Songs, Playlists, Add), a mini player above the tabs on every tab except Now, swipe on the cover for next/previous, Play all / Shuffle in Songs and in a playlist, a two-column landscape layout, a collapsible Speed and pitch panel (command `fx`: `s`, `p`, `l` or `preset`; values are clamped by `remNum` and the state carries `fx: {s, p, l}`), an opt-in "Show in phone notifications" switch (Media Session API: title, artist, cover, play/pause/next/previous/seek from the notification bar and lock screen; phones only show it for a page that plays sound, so the page loops a 10 s silent WAV while it is on, which takes audio focus and is why it is off by default), and it follows the desktop theme. Tests rely on the ids and `data-` attributes in `remote.html` (`#bPlay`, `#songs .item`, `[data-q]`, `[data-next]`, `[data-open]`, `[data-plplay]`, `#upnext`, `#dot`, `#lost`, `#toast`): keep them when restyling.
- **PROTOTYPE: "On this phone" (play on the phone itself).** The phone page has a switch at the top of Now: "On the computer" (default, the phone is a remote) or "On this phone". In phone mode the page plays songs with its own `<audio>` that streams `GET /r/<token>/api/audio?id=<song id>` from the PC (the PC must be on, same Wi-Fi). `remote.rs`: `resolve_audio` (the phone only sends an id; the server maps it to a file through the id-to-relative-path map pushed by the page with `remote_push` kind `files`, requires an audio extension and that the canonical path is inside the current music folder, which `set_audio_root` asks for at each request), `serve_audio` (Range/206/416, tests `audio_stream_and_ranges`, `range_parser`). `index.html`: `remFiles()` (never sent to the phone), `remCover(want)` and the commands `cover` and `favId`. `remote.html`: the block between "PROTOTYPE: play on this phone" and "end of PROTOTYPE" (`lpExec` mirrors `remExec`: toggle, play, pause, next, prev, seek, skip, vol, mute, shuffle, repeat, fav, playTrack, playPl, queue, dequeue, fx) and `cmd()` routes to it while `PM` is true; the Media Session works with the real audio, so the silent loop is off in this mode. Limits: no pitch in semitones (speed works; "link" = nightcore), phone speakers/background behaviour is up to the phone's browser, playback stops if the PC sleeps or leaves the network. **To remove the prototype:** `git revert` the commit titled "Phone: play on this phone (prototype)" (it touches only `remote.rs`, `lib.rs`, `index.html`, `remote.html` and this file).
- **Select and delete several songs (with Undo).** "Select" in the toolbar (or "Select songs" in a song's menu) turns on `SEL` mode: clicking rows toggles them, Shift-click selects a range, Ctrl+A selects all, Escape leaves. Deleting uses `bulkDelete`: the songs leave `S.tracks` at once but their records and files stay untouched in `PEND` for 10 seconds (`#undoBar`); Undo just puts them back, and only `commitPending` moves the files (and `.lrc`) to the Recycle Bin and removes the records. So closing the app inside the window loses nothing; quitting from the tray commits first, and `scanFolder` ignores the files of pending songs (`pendingRels`). The single-song Delete in the menu still asks and deletes at once.
- **Most played and stats.** `t.plays` / `t.lastPlayed` are counted in the player (half of a song or its first 10 seconds). The sidebar view `top` ("Most played", top 100) and the "Listening stats" dialog (`openStats`, `topBy` merges artist spellings case-insensitively) read them.
- **Lyrics** come from a `.lrc` file with the same base name next to the song (`parseLrc`, `lyrLoad`). Encodings: UTF-8/UTF-16/windows-1256.
- **Theme** is stored in `localStorage` key `mp_theme` and applied as CSS variables on `:root`.
- **Look (rounded, soft glow, floating player).** The last CSS block in `index.html` ("look: rounder shapes…") overrides earlier rules; all colors still come from the theme variables, so presets and custom colors keep working. Keep `.row` height equal to `ROW_H` (56) and give rows no vertical margin: the virtual list computes positions from `ROW_H`. The seek and volume sliders are drawn by CSS from the custom property `--p`, which `fillRange(el)` sets (on input, on `timeupdate`, and when the value is set from code); call it after any code that sets `#seek` or `#vol` `.value`.
- Playback state keys in `localStorage` start with `mp_` and are listed in `BK_KEYS` when they must survive in the backup.
- Beware of name clashes: everything is one global script. Before adding a top-level function or constant, grep that the name is not already used (a duplicate function name silently overrides the earlier one).

## Testing

There is no test suite in the repo. Verify UI work with Playwright against `src/index.html` opened from `file://`, with the Tauri APIs (`window.__TAURI__`: `fs`, `path`, `core.invoke`, `opener`, `updater`, `process`) mocked by a small harness that serves a temp "AppData" folder. Check at least: the feature itself, a reload (persistence), and that the main flows still work (play, queue, playlists, delete, tag write). For tag writing, compare audio checksums with `ffmpeg -f md5` before and after. Rust helpers (`is_inside`) have unit tests: `cargo test` in `src-tauri` on a machine that can build it. `musicdir.rs` is pure Rust too (it only needs `serde_json`): in a scratch crate, `#[path]`-include it and run `cargo test`. `remote.rs` does not depend on Tauri: in a scratch crate, `#[path]`-include it with its dependencies (tiny_http, serde_json, reqwest, rustls, if-addrs, qrcode) and run `cargo test`. For end-to-end checks, a small bin can expose its functions over a local admin port so Playwright drives the real server together with the simulated desktop page and a mobile-sized phone page.

Always run `node --check` on the extracted script after large edits.

## Git and secrets

- Branch: `main`. Commit small, with a clear message. Commit and push only when the owner asks or has clearly authorised it.
- **Never commit** the updater private signing key, `.env`, or any `*.key` file (they are in `.gitignore`). The public key in `tauri.conf.json` is fine. The private key lives in the GitHub Secret `TAURI_SIGNING_PRIVATE_KEY` (single-line value, the key file's content) and in the owner's local `.env` for local builds only.
- Do not rewrite published history.

## Definition of done

1. Feature works in the simulator and a reload keeps its data.
2. No regression in the flows listed under Testing.
3. UI text is English, user text uses `dir="auto"`.
4. `README.md` updated when users would notice the change.
5. Reply to the owner in Persian: what changed, what was and was not tested, and that a new Release with a higher version number is needed to receive it.
