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
| `src-tauri/src/lib.rs` | Tauri builder, plugins (single-instance, updater, process, fs, opener), command `move_to_trash`. |
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
- **Lyrics** come from a `.lrc` file with the same base name next to the song (`parseLrc`, `lyrLoad`). Encodings: UTF-8/UTF-16/windows-1256.
- **Theme** is stored in `localStorage` key `mp_theme` and applied as CSS variables on `:root`.
- Playback state keys in `localStorage` start with `mp_` and are listed in `BK_KEYS` when they must survive in the backup.
- Beware of name clashes: everything is one global script. Before adding a top-level function or constant, grep that the name is not already used (a duplicate function name silently overrides the earlier one).

## Testing

There is no test suite in the repo. Verify UI work with Playwright against `src/index.html` opened from `file://`, with the Tauri APIs (`window.__TAURI__`: `fs`, `path`, `core.invoke`, `opener`, `updater`, `process`) mocked by a small harness that serves a temp "AppData" folder. Check at least: the feature itself, a reload (persistence), and that the main flows still work (play, queue, playlists, delete, tag write). For tag writing, compare audio checksums with `ffmpeg -f md5` before and after. Rust helpers (`is_inside`) have unit tests: `cargo test` in `src-tauri` on a machine that can build it.

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
