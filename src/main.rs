//! CERBERUS - Phase 6
//! Tauri desktop app that mounts disk and disc images as Windows drives using
//! only built-in Windows components:
//!   * ISO / VHD / VHDX                -> Windows Virtual Disk service
//!   * everything else                 -> in-process iSCSI loopback target
//!       raw IMG/IMA/FLP/RAW, VDI, VMDK, OVA, CHD  (disks)
//!       BIN/CUE, MDS/MDF, CCD, CDI, NRG, CD CHD   (virtual CD/DVD drive)
//!   * ZIP / 7Z / RAR / TAR / TAR.GZ / GZ / CAB (also split: .7z.001, .part1.rar)
//!       -> Windows Projected File System folder with its own drive letter
//! Plus Initialize & Format, Format, write overlays and "Open in WSL".
//!
//! Phase 6 - Windows integration:
//!   * Explorer right-click "Mount with CERBERUS" and optional double-click
//!     associations (`cerberus.exe --mount "<file>"`)
//!   * Start with Windows (`cerberus.exe --tray`), re-mount last session
//!   * Tray menu listing every mounted drive (Open / Unmount)
//!   * Settings dialog
//!
//! The window runs non-elevated (so drag & drop from Explorer works) and lives in
//! the system tray: closing the window only hides it; "Quit CERBERUS" in the tray
//! menu unmounts everything and exits. Administrator work is done by an elevated
//! helper copy of cerberus.exe (broker.rs), started on first need.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(dead_code)]

#[macro_use]
mod log;
mod archive;
mod archmount;
mod backend;
mod broker;
mod chdimg;
mod conn;
mod detect;
mod disk;
mod formats;
mod login;
mod mmc;
mod mounts;
mod optical;
mod ova;
mod overlay;
mod pdu;
mod projfs;
mod ps;
mod registry;
mod scsi;
mod settings;
mod target;
mod vdi;
mod vmdk;

use std::io::{Read, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::json;
use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Emitter, Manager};

use mounts::{AppState, MountInfo};
use settings::Settings;

const PORT: u16 = 3260;
const TRAY_ID: &str = "cerberus-tray";

/// The "still running in the tray" hint is logged only once per session.
static TRAY_HINT: AtomicBool = AtomicBool::new(false);

/// Loopback port used to detect an already-running CERBERUS and hand it work
/// (show the window, mount files from Explorer) - single instance.
const INSTANCE_PORT: u16 = 47260;

#[derive(Serialize)]
struct Status {
    version: &'static str,
    admin: bool,
    port: u16,
    target_ok: bool,
    target_error: Option<String>,
    busy: Option<String>,
    ready: bool,
    closing: bool,
    mounts: Vec<MountInfo>,
}

#[derive(Serialize)]
struct SettingsView {
    settings: Settings,
    extensions: Vec<&'static str>,
}

fn shared(app: &AppHandle) -> Arc<AppState> {
    app.state::<Arc<AppState>>().inner().clone()
}

fn toast(app: &AppHandle, msg: &str, ok: bool) {
    let _ = app.emit("toast", json!({ "msg": msg, "kind": if ok { "ok" } else { "err" } }));
}

// ------------------------------------------------------------------ commands

#[tauri::command]
fn get_status(app: AppHandle) -> Status {
    let st = shared(&app);
    let target_error = st.target_error.lock().ok().and_then(|g| g.clone());
    Status {
        version: env!("CARGO_PKG_VERSION"),
        admin: st.has_admin(),
        port: st.port,
        target_ok: target_error.is_none(),
        target_error,
        busy: st.busy.lock().ok().and_then(|b| b.clone()),
        ready: st.ready.load(Ordering::SeqCst),
        closing: st.closing.load(Ordering::SeqCst),
        mounts: st.list(),
    }
}

#[tauri::command]
async fn request_admin(app: AppHandle) -> Result<(), String> {
    let st = shared(&app);
    tauri::async_runtime::spawn_blocking(move || mounts::ensure_admin(&st))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn get_log(since: u64) -> Vec<log::LogLine> {
    log::since(since)
}

#[tauri::command]
fn get_settings(app: AppHandle) -> SettingsView {
    SettingsView { settings: shared(&app).settings(), extensions: settings::EXTENSIONS.to_vec() }
}

/// Apply settings that live inside CERBERUS (not in the registry).
fn apply_runtime(s: &Settings) {
    archmount::set_letter_pref(s.letter());
    if std::env::var("CERBERUS_DEBUG").is_err() {
        log::set_level(s.log_level);
    }
}

#[tauri::command]
fn set_settings(app: AppHandle, settings: Settings) -> Result<Settings, String> {
    let st = shared(&app);
    let new = settings.normalized();
    let old = st.settings();
    settings::apply(&old, &new, false)?;
    settings::save(&st.base, &new)?;
    apply_runtime(&new);
    if let Ok(mut g) = st.settings.lock() {
        *g = new.clone();
    }
    info!(
        "Settings saved (start with Windows: {}, re-mount at startup: {}, Explorer menu: {}, double-click: {})",
        new.start_with_windows,
        new.restore_mounts,
        new.context_menu,
        if new.associations.is_empty() { "none".to_string() } else { new.associations.join(", ") }
    );
    Ok(new)
}

#[tauri::command]
fn set_readonly_default(app: AppHandle, readonly: bool) -> Result<(), String> {
    let st = shared(&app);
    let mut s = st.settings();
    s.readonly_default = readonly;
    settings::save(&st.base, &s)?;
    if let Ok(mut g) = st.settings.lock() {
        *g = s;
    }
    Ok(())
}

#[tauri::command]
fn open_default_apps() -> Result<(), String> {
    Command::new("explorer.exe")
        .arg("ms-settings:defaultapps")
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
async fn pick_files() -> Result<Vec<String>, String> {
    tauri::async_runtime::spawn_blocking(|| {
        rfd::FileDialog::new()
            .set_title("Mount image(s)")
            .add_filter("All supported images", mounts::SUPPORTED)
            .add_filter("Disk images (ISO, VHD, VHDX, IMG, VDI, VMDK, OVA, CHD)", &[
                "iso", "vhd", "vhdx", "img", "ima", "flp", "raw", "vdi", "vmdk", "ova", "chd",
            ])
            .add_filter("CD/DVD images (CUE, BIN, MDS, MDF, CCD, CDI, NRG)", &[
                "cue", "bin", "mds", "mdf", "ccd", "cdi", "nrg",
            ])
            .add_filter("Archives (ZIP, 7Z, RAR, TAR, TAR.GZ, TGZ, GZ, CAB, split .001)", &[
                "zip", "7z", "rar", "tar", "tgz", "gz", "cab", "001",
            ])
            .add_filter("All files", &["*"])
            .pick_files()
            .map(|v| v.into_iter().map(|p| p.to_string_lossy().to_string()).collect::<Vec<String>>())
            .unwrap_or_default()
    })
    .await
    .map_err(|e| e.to_string())
}

#[tauri::command]
async fn mount_image(app: AppHandle, path: String, readonly: bool) -> Result<MountInfo, String> {
    let st = shared(&app);
    tauri::async_runtime::spawn_blocking(move || mounts::mount(&st, &path, readonly))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn unmount_image(app: AppHandle, id: String, keep: Option<bool>) -> Result<(), String> {
    let st = shared(&app);
    let keep = keep.unwrap_or(true);
    tauri::async_runtime::spawn_blocking(move || mounts::unmount(&st, &id, keep))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn unmount_all(app: AppHandle) -> Result<Vec<String>, String> {
    let st = shared(&app);
    tauri::async_runtime::spawn_blocking(move || mounts::unmount_all(&st))
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn discard_saved_changes(app: AppHandle, path: String) -> Result<bool, String> {
    let st = shared(&app);
    mounts::discard_saved(&st, &path)
}

#[tauri::command]
fn list_saved_changes(app: AppHandle) -> Vec<mounts::SavedChange> {
    mounts::list_saved(&shared(&app))
}

#[tauri::command]
fn delete_saved_changes(app: AppHandle, key: String) -> Result<(), String> {
    mounts::delete_saved(&shared(&app), &key)
}

#[tauri::command]
async fn initialize_disk(
    app: AppHandle,
    id: String,
    style: String,
    fs: String,
    label: String,
    full: bool,
) -> Result<MountInfo, String> {
    let st = shared(&app);
    tauri::async_runtime::spawn_blocking(move || mounts::initialize_and_format(&st, &id, &style, &fs, &label, full))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn format_drive(
    app: AppHandle,
    id: String,
    letter: String,
    fs: String,
    label: String,
    full: bool,
) -> Result<MountInfo, String> {
    let st = shared(&app);
    tauri::async_runtime::spawn_blocking(move || mounts::format_drive(&st, &id, &letter, &fs, &label, full))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn open_in_wsl(app: AppHandle, id: String) -> Result<MountInfo, String> {
    let st = shared(&app);
    let m = tauri::async_runtime::spawn_blocking(move || mounts::open_in_wsl(&st, &id))
        .await
        .map_err(|e| e.to_string())??;
    if let Some(p) = m.wsl.first() {
        let _ = open_folder(p.clone());
    }
    Ok(m)
}

#[tauri::command]
fn open_drive(letter: String) -> Result<(), String> {
    let l: String = letter.chars().filter(|c| c.is_ascii_alphabetic()).take(1).collect();
    if l.is_empty() {
        return Err("no drive letter".into());
    }
    Command::new("explorer.exe")
        .arg(format!("{}:\\", l.to_uppercase()))
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn open_folder(path: String) -> Result<(), String> {
    Command::new("explorer.exe")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn reveal_image(path: String) -> Result<(), String> {
    explorer_select(&path).map_err(|e| e.to_string())
}

#[cfg(windows)]
fn explorer_select(path: &str) -> std::io::Result<()> {
    use std::os::windows::process::CommandExt;
    Command::new("explorer.exe")
        .raw_arg(format!("/select,\"{}\"", path))
        .spawn()
        .map(|_| ())
}

#[cfg(not(windows))]
fn explorer_select(_path: &str) -> std::io::Result<()> {
    Ok(())
}

// ------------------------------------------------------------------ window & tray

/// Bring the main window back from the tray.
fn show_main(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Open what a mount produced: its first drive letter, else its WSL folder.
fn open_mount(m: &MountInfo) {
    if let Some(l) = m.letters.first() {
        let _ = open_drive(l.clone());
    } else if let Some(p) = m.wsl.first() {
        let _ = open_folder(p.clone());
    }
}

/// Files handed over by Explorer (right-click "Mount with CERBERUS", double-click).
fn external_mount(app: &AppHandle, paths: Vec<String>) {
    let st = shared(app);
    let app = app.clone();
    std::thread::spawn(move || {
        let cfg = st.settings();
        for p in paths {
            if let Some(m) = mounts::find_by_path(&st, &p) {
                // Already mounted - just open it.
                if cfg.open_after_external_mount {
                    open_mount(&m);
                }
                continue;
            }
            match mounts::mount(&st, &p, cfg.readonly_default) {
                Ok(m) => {
                    let where_ = if m.letters.is_empty() {
                        "(no drive letter)".to_string()
                    } else {
                        m.letters.iter().map(|l| format!("{}:", l)).collect::<Vec<_>>().join(", ")
                    };
                    toast(&app, &format!("{} mounted as {}", m.name, where_), true);
                    if cfg.open_after_external_mount {
                        open_mount(&m);
                    }
                }
                Err(e) => {
                    let name = std::path::Path::new(&p)
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or(p.clone());
                    warn!("Mount failed for {}: {}", name, e);
                    show_main(&app);
                    toast(&app, &format!("{}: {}", name, e), false);
                }
            }
        }
    });
}

/// Tray "Quit CERBERUS": unmount everything, then exit.
fn quit(app: &AppHandle) {
    let st = shared(app);
    if st.closing.swap(true, Ordering::SeqCst) {
        return;
    }
    show_main(app);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.emit("closing", true);
    }
    let app2 = app.clone();
    std::thread::spawn(move || {
        mounts::shutdown(&st);
        st.exiting.store(true, Ordering::SeqCst);
        app2.exit(0);
    });
}

fn short(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max - 1).collect();
        format!("{}…", t)
    }
}

/// Tray menu: one submenu per mounted image (Open X: / Unmount), then actions.
fn tray_menu(app: &AppHandle, list: &[MountInfo]) -> tauri::Result<Menu<tauri::Wry>> {
    let menu = Menu::new(app)?;
    if list.is_empty() {
        menu.append(&MenuItem::with_id(app, "none", "No drives mounted", false, None::<&str>)?)?;
    } else {
        for m in list {
            let letters = if m.letters.is_empty() {
                "—".to_string()
            } else {
                m.letters.iter().map(|l| format!("{}:", l)).collect::<Vec<_>>().join(" ")
            };
            let sub = Submenu::new(app, format!("{}   {}", letters, short(&m.name, 42)), true)?;
            for l in &m.letters {
                sub.append(&MenuItem::with_id(app, format!("open:{}", l), format!("Open {}:", l), true, None::<&str>)?)?;
            }
            for (i, _) in m.wsl.iter().enumerate() {
                sub.append(&MenuItem::with_id(app, format!("wsl:{}:{}", m.id, i), "Open in WSL", true, None::<&str>)?)?;
            }
            sub.append(&MenuItem::with_id(app, format!("um:{}", m.id), "Unmount", true, None::<&str>)?)?;
            menu.append(&sub)?;
        }
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "show", "Show CERBERUS", true, None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "unmount_all", "Unmount all", !list.is_empty(), None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app, "settings", "Settings…", true, None::<&str>)?)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app, "quit", "Quit CERBERUS", true, None::<&str>)?)?;
    Ok(menu)
}

fn on_menu(app: &AppHandle, id: &str) {
    let st = shared(app);
    match id {
        "show" => show_main(app),
        "settings" => {
            show_main(app);
            let _ = app.emit("open-settings", true);
        }
        "unmount_all" => {
            let app = app.clone();
            std::thread::spawn(move || {
                for e in mounts::unmount_all(&st) {
                    warn!("{}", e);
                    toast(&app, &e, false);
                }
            });
        }
        "quit" => quit(app),
        _ => {
            if let Some(l) = id.strip_prefix("open:") {
                let _ = open_drive(l.to_string());
            } else if let Some(rest) = id.strip_prefix("wsl:") {
                let mut it = rest.rsplitn(2, ':');
                let idx: usize = it.next().and_then(|v| v.parse().ok()).unwrap_or(0);
                let mid = it.next().unwrap_or("");
                if let Some(p) = st.list().into_iter().find(|m| m.id == mid).and_then(|m| m.wsl.get(idx).cloned()) {
                    let _ = open_folder(p);
                }
            } else if let Some(mid) = id.strip_prefix("um:") {
                let mid = mid.to_string();
                let app = app.clone();
                std::thread::spawn(move || {
                    if let Err(e) = mounts::unmount(&st, &mid, true) {
                        toast(&app, &e, false);
                    }
                });
            }
        }
    }
}

fn build_tray(app: &tauri::App) -> Result<(), Box<dyn std::error::Error>> {
    let menu = tray_menu(app.handle(), &[])?;
    let icon = app.default_window_icon().cloned().ok_or("no application icon")?;
    TrayIconBuilder::with_id(TRAY_ID)
        .icon(icon)
        .tooltip("CERBERUS - virtual drive mounter")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_tray_icon_event(|tray, event| match event {
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
            | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => show_main(tray.app_handle()),
            _ => {}
        })
        .build(app)?;
    Ok(())
}

/// Rebuild the tray menu whenever the list of mounts changes.
fn tray_watch(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last = 0u64;
        loop {
            std::thread::sleep(Duration::from_millis(600));
            let st = shared(&app);
            if st.exiting.load(Ordering::SeqCst) {
                break;
            }
            let g = st.generation.load(Ordering::SeqCst);
            if g == last {
                continue;
            }
            last = g;
            let list = st.list();
            if let (Ok(menu), Some(tray)) = (tray_menu(&app, &list), app.tray_by_id(TRAY_ID)) {
                let _ = tray.set_menu(Some(menu));
                let tip = match list.len() {
                    0 => "CERBERUS - no drives mounted".to_string(),
                    1 => "CERBERUS - 1 drive mounted".to_string(),
                    n => format!("CERBERUS - {} drives mounted", n),
                };
                let _ = tray.set_tooltip(Some(tip));
            }
        }
    });
}

// ------------------------------------------------------------------ single instance

/// Messages: "SHOW" or "MOUNT\t<path>", one per line.
fn instance_listener(app: AppHandle, listener: std::net::TcpListener) {
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut c = conn;
            let _ = c.set_read_timeout(Some(Duration::from_secs(3)));
            let mut buf = Vec::new();
            let _ = Read::by_ref(&mut c).take(256 * 1024).read_to_end(&mut buf);
            let text = String::from_utf8_lossy(&buf).to_string();
            let mut paths = Vec::new();
            for line in text.lines() {
                if line == "SHOW" {
                    show_main(&app);
                } else if let Some(p) = line.strip_prefix("MOUNT\t") {
                    if !p.trim().is_empty() {
                        paths.push(p.trim().to_string());
                    }
                }
            }
            if !paths.is_empty() {
                external_mount(&app, paths);
            }
        }
    });
}

struct Launch {
    tray: bool,
    mounts: Vec<String>,
}

fn parse_args(args: &[String]) -> Launch {
    let mut l = Launch { tray: false, mounts: Vec::new() };
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--tray" => l.tray = true,
            "--mount" => {
                if let Some(p) = args.get(i + 1) {
                    l.mounts.push(p.clone());
                    i += 1;
                }
            }
            other if !other.starts_with("--") => l.mounts.push(other.to_string()),
            _ => {}
        }
        i += 1;
    }
    l
}

fn main() {
    // Elevated helper mode: `cerberus.exe --broker <port> <token>` (no window).
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 4 && args[1] == "--broker" {
        if let Ok(port) = args[2].parse::<u16>() {
            broker::serve(port, &args[3]);
        }
        return;
    }
    let launch = parse_args(&args);

    // Single instance: if CERBERUS is already running (e.g. hidden in the tray),
    // hand it the files to mount (or ask it to show its window) and exit.
    let instance = match std::net::TcpListener::bind(("127.0.0.1", INSTANCE_PORT)) {
        Ok(l) => l,
        Err(_) => {
            if let Ok(mut s) = std::net::TcpStream::connect(("127.0.0.1", INSTANCE_PORT)) {
                let mut msg = String::new();
                if launch.mounts.is_empty() {
                    if !launch.tray {
                        msg.push_str("SHOW\n");
                    }
                } else {
                    for p in &launch.mounts {
                        msg.push_str(&format!("MOUNT\t{}\n", p));
                    }
                }
                let _ = s.write_all(msg.as_bytes());
                let _ = s.shutdown(std::net::Shutdown::Write);
            }
            return;
        }
    };

    let base = mounts::base_dir();
    let env_level = std::env::var("CERBERUS_DEBUG").ok().and_then(|v| v.parse::<u8>().ok());
    log::init(&base.join("logs"), env_level.unwrap_or(0));
    info!("CERBERUS {} starting ({})", env!("CARGO_PKG_VERSION"), base.display());

    let st = Arc::new(AppState::new(base, PORT));
    apply_runtime(&st.settings());
    if st.admin {
        warn!("CERBERUS was started as administrator - drag & drop from Explorer is blocked by Windows in that case. Start it with CERBERUS.bat instead.");
    }
    match target::start(st.registry.clone(), PORT) {
        Ok(()) => info!("iSCSI engine listening on 127.0.0.1:{}", PORT),
        Err(e) => {
            warn!("{}", e);
            if let Ok(mut g) = st.target_error.lock() {
                *g = Some(e);
            }
        }
    }
    {
        let st2 = st.clone();
        std::thread::spawn(move || {
            // Keep registry entries pointing at this exe (the folder may have moved).
            let cfg = st2.settings();
            if let Err(e) = settings::apply(&cfg, &cfg, true) {
                warn!("Could not refresh the Windows integration settings: {}", e);
            }
            mounts::startup_cleanup(&st2);
            let n = mounts::restore_session(&st2);
            if n > 0 {
                info!("Re-mounted {} image(s) from the last session", n);
            }
        });
    }

    let st_ev = st.clone();
    let show_window = !launch.tray && launch.mounts.is_empty();
    let initial_mounts = launch.mounts.clone();
    tauri::Builder::default()
        .manage(st.clone())
        .setup(move |app| {
            build_tray(app)?;
            let handle = app.handle().clone();
            tray_watch(handle.clone());
            instance_listener(handle.clone(), instance);
            if show_window {
                show_main(&handle);
            }
            if !initial_mounts.is_empty() {
                external_mount(&handle, initial_mounts.clone());
            }
            Ok(())
        })
        .on_menu_event(|app, event| on_menu(app, event.id.as_ref()))
        .invoke_handler(tauri::generate_handler![
            get_status,
            request_admin,
            get_log,
            get_settings,
            set_settings,
            set_readonly_default,
            open_default_apps,
            pick_files,
            mount_image,
            unmount_image,
            unmount_all,
            discard_saved_changes,
            list_saved_changes,
            delete_saved_changes,
            initialize_disk,
            format_drive,
            open_in_wsl,
            open_drive,
            open_folder,
            reveal_image
        ])
        .on_window_event(move |window, event| match event {
            tauri::WindowEvent::CloseRequested { api, .. } => {
                if st_ev.exiting.load(Ordering::SeqCst) {
                    return;
                }
                // Closing the window only hides it - CERBERUS keeps running in the tray.
                api.prevent_close();
                let _ = window.hide();
                if !TRAY_HINT.swap(true, Ordering::SeqCst) {
                    info!("CERBERUS is still running in the system tray - right-click the tray icon and choose \"Quit CERBERUS\" to unmount everything and exit");
                }
            }
            tauri::WindowEvent::DragDrop(drag) => match drag {
                tauri::DragDropEvent::Enter { .. } => {
                    let _ = window.emit("drag-state", true);
                }
                tauri::DragDropEvent::Drop { paths, .. } => {
                    let _ = window.emit("drag-state", false);
                    let list: Vec<String> = paths.iter().map(|p| p.to_string_lossy().to_string()).collect();
                    let _ = window.emit("files-dropped", list);
                }
                tauri::DragDropEvent::Leave => {
                    let _ = window.emit("drag-state", false);
                }
                _ => {}
            },
            _ => {}
        })
        .run(tauri::generate_context!())
        .expect("error while running CERBERUS");
}
