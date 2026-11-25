//! Mount manager: routes each image to the right engine, tracks active mounts,
//! persists them (data\mounts.json) for crash recovery, and cleans up.
//!
//!   .iso / .vhd / .vhdx / .img(ISO9660)  -> Windows native (Mount-DiskImage)
//!   everything else                        -> CERBERUS iSCSI loopback target:
//!       raw IMG/IMA/FLP/RAW                   disk, direct read-write
//!       VDI / VMDK / OVA / hard-disk CHD      disk, writes go to an overlay
//!       BIN/CUE, MDS/MDF, CCD, CDI, NRG, CD/DVD CHD, raw-CD IMG -> CD/DVD drive
//!
//!   ZIP / 7Z / RAR / TAR / TAR.GZ / TGZ / GZ / CAB   -> archmount.rs (Windows
//!       Projected File System folder + its own drive letter, read-only)
//!
//! Also: Initialize & Format (blank disks) and Format (existing volumes).
//! The window runs non-elevated; administrator work goes through broker.rs.
//! Phase 6: split archives, "Open in WSL" for Linux partitions, remembered
//! sessions (data\session.json) and settings (settings.rs).
//! Partitions Windows can't read (Linux ext4/swap/LVM, ...) get no drive letter
//! and are listed on the card instead.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::archive;
use crate::archmount;
use crate::broker;
use crate::detect;
use crate::disk::{fnv1a, Disk};
use crate::formats;
use crate::overlay::{Overlay, CHUNK};
use crate::ps;
use crate::settings::{self, Settings};
use crate::target::Registry;

pub const IQN_PREFIX: &str = "iqn.2026-11.local.cerberus:";

const NOTE_BLANK: &str = "Blank / uninitialised disk - click \"Initialize & Format\" to make it usable.";
const NOTE_NO_LETTER: &str = "Mounted, but Windows assigned no drive letter (no usable partition?). Use \"Initialize & Format\" or check Disk Management.";
const NOTE_NO_CD_LETTER: &str = "The virtual CD/DVD drive was attached but no drive letter appeared - check Device Manager > DVD/CD-ROM drives and the log.";

pub const SUPPORTED: &[&str] = &[
    "iso", "vhd", "vhdx", "img", "ima", "flp", "raw", "vdi", "vmdk", "ova", "chd", "cue", "bin", "mds", "mdf", "ccd",
    "cdi", "nrg", "zip", "7z", "rar", "tar", "tgz", "gz", "cab", "001",
];

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct MountInfo {
    pub id: String,
    pub path: String,
    pub name: String,
    pub kind: String,
    pub route: String,
    #[serde(default)]
    pub storage: String,
    #[serde(default)]
    pub contents: String,
    pub readonly: bool,
    #[serde(default)]
    pub removable: bool,
    #[serde(default)]
    pub iso: bool,
    #[serde(default)]
    pub optical: bool,
    /// Writes go to a changes file instead of the image.
    #[serde(default)]
    pub overlay: bool,
    /// Bytes of saved changes that were re-applied at mount time.
    #[serde(default)]
    pub saved_changes: u64,
    /// Archive projected with Windows Projected File System (Phase 5).
    #[serde(default)]
    pub archive: bool,
    /// Folder that backs the drive letter (archives).
    #[serde(default)]
    pub folder: Option<String>,
    pub size: u64,
    #[serde(default)]
    pub letters: Vec<String>,
    /// Partitions skipped because Windows can't read their file system.
    #[serde(default)]
    pub skipped: Vec<String>,
    /// Explorer paths of Linux partitions attached to WSL (Open in WSL).
    #[serde(default)]
    pub wsl: Vec<String>,
    #[serde(default)]
    pub disk_number: Option<i64>,
    #[serde(default)]
    pub style: String,
    #[serde(default)]
    pub iqn: Option<String>,
    #[serde(default)]
    pub mounted_at: u64,
    #[serde(default)]
    pub note: Option<String>,
}

pub struct AppState {
    pub base: PathBuf,
    pub port: u16,
    /// This process itself is elevated (started with "Run as administrator").
    pub admin: bool,
    pub registry: Arc<Registry>,
    pub mounts: Mutex<Vec<MountInfo>>,
    pub op_lock: Mutex<()>,
    pub busy: Mutex<Option<String>>,
    pub target_error: Mutex<Option<String>>,
    pub stale: Mutex<Vec<MountInfo>>,
    pub ready: AtomicBool,
    pub closing: AtomicBool,
    pub exiting: AtomicBool,
    pub portal_used: AtomicBool,
    pub settings: Mutex<Settings>,
    /// Bumped whenever the mount list changes (tray menu refresh).
    pub generation: AtomicU64,
    next_id: AtomicU64,
}

impl AppState {
    pub fn new(base: PathBuf, port: u16) -> AppState {
        let _ = fs::create_dir_all(base.join("data"));
        let stale = load_state(&base);
        let cfg = settings::load(&base);
        AppState {
            admin: ps::is_elevated(),
            base,
            port,
            registry: Arc::new(Registry::new()),
            mounts: Mutex::new(Vec::new()),
            op_lock: Mutex::new(()),
            busy: Mutex::new(None),
            target_error: Mutex::new(None),
            stale: Mutex::new(stale),
            ready: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            exiting: AtomicBool::new(false),
            portal_used: AtomicBool::new(false),
            settings: Mutex::new(cfg),
            generation: AtomicU64::new(1),
            next_id: AtomicU64::new(1),
        }
    }

    pub fn list(&self) -> Vec<MountInfo> {
        self.mounts.lock().map(|m| m.clone()).unwrap_or_default()
    }

    fn state_file(&self) -> PathBuf {
        self.base.join("data").join("mounts.json")
    }

    fn overlay_dir(&self) -> PathBuf {
        self.base.join("data").join("overlays")
    }

    fn save(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        let list = self.list();
        if let Ok(s) = serde_json::to_string_pretty(&list) {
            let _ = fs::write(self.state_file(), s);
        }
    }

    fn set_busy(&self, s: Option<String>) {
        if let Ok(mut b) = self.busy.lock() {
            *b = s;
        }
    }

    fn new_id(&self) -> String {
        let n = self.next_id.fetch_add(1, Ordering::SeqCst);
        format!("m{:x}{:02x}", now() & 0xFF_FFFF, n & 0xFF)
    }

    fn remove(&self, id: &str) {
        if let Ok(mut m) = self.mounts.lock() {
            m.retain(|x| x.id != id);
        }
        self.save();
    }

    fn replace(&self, info: &MountInfo) {
        if let Ok(mut m) = self.mounts.lock() {
            if let Some(x) = m.iter_mut().find(|x| x.id == info.id) {
                *x = info.clone();
            }
        }
        self.save();
    }

    pub fn settings(&self) -> Settings {
        self.settings.lock().map(|s| s.clone()).unwrap_or_default()
    }

    /// Administrator rights are available (elevated process or connected helper).
    pub fn has_admin(&self) -> bool {
        self.admin || broker::connected()
    }

    fn find(&self, id: &str) -> Result<MountInfo, String> {
        self.list()
            .into_iter()
            .find(|m| m.id == id)
            .ok_or_else(|| "that image is no longer mounted".to_string())
    }
}

/// Sets the "busy" banner for the duration of an operation.
struct Busy<'a>(&'a AppState);
impl<'a> Busy<'a> {
    fn new(st: &'a AppState, msg: String) -> Busy<'a> {
        st.set_busy(Some(msg));
        Busy(st)
    }
}
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        self.0.set_busy(None);
    }
}

pub fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Make sure administrator rights are available, starting the elevated helper
/// (one UAC prompt) when needed.
pub fn ensure_admin(st: &AppState) -> Result<(), String> {
    if st.has_admin() {
        return Ok(());
    }
    let _busy = Busy::new(st, "Waiting for administrator permission (UAC)…".into());
    broker::ensure()
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn load_state(base: &Path) -> Vec<MountInfo> {
    fs::read_to_string(base.join("data").join("mounts.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Canonical path without the \\?\ prefix (Mount-DiskImage does not accept it).
fn clean_path(p: &Path) -> String {
    let c = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = c.to_string_lossy().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{}", rest)
    } else if let Some(rest) = s.strip_prefix(r"\\?\") {
        rest.to_string()
    } else {
        s
    }
}

fn wait_ready(st: &AppState) {
    for _ in 0..900 {
        if st.ready.load(Ordering::SeqCst) {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1048576.0)
}

fn letters_text(l: &[String]) -> String {
    if l.is_empty() {
        "no drive letter".to_string()
    } else {
        l.iter().map(|x| format!("{}:", x)).collect::<Vec<_>>().join(", ")
    }
}

/// Card note for partitions Windows can't read.
fn foreign_note(skipped: &[String], has_letters: bool) -> String {
    let lead = if has_letters {
        format!(
            "{} other partition(s) use a file system Windows can't read and were not given a drive letter",
            skipped.len()
        )
    } else {
        "None of this disk's partitions use a file system Windows can read, so no drive letter was assigned".to_string()
    };
    format!(
        "{} ({}). Windows has no built-in driver for Linux ext4 / swap / LVM.",
        lead,
        skipped.join("; ")
    )
}

/// Apply the result of a disk-type mount/initialize script to a MountInfo.
fn apply_disk_result(info: &mut MountInfo, v: &serde_json::Value, iso: bool) {
    info.letters = ps::letters(v);
    info.skipped = ps::skipped(v);
    if let Some(d) = v.get("disk").and_then(|d| d.as_i64()) {
        info.disk_number = Some(d);
    }
    if let Some(s) = v.get("style").and_then(|s| s.as_str()) {
        info.style = s.to_string();
    }
    info.note = None;
    if !info.skipped.is_empty() {
        warn!("{}: skipped {} partition(s) Windows can't read: {}", info.name, info.skipped.len(), info.skipped.join("; "));
        info.note = Some(foreign_note(&info.skipped, !info.letters.is_empty()));
    } else if !iso && info.style == "RAW" && info.letters.is_empty() {
        info.note = Some(NOTE_BLANK.into());
    }
}

pub fn mount(st: &AppState, path: &str, readonly: bool) -> Result<MountInfo, String> {
    wait_ready(st);
    if st.closing.load(Ordering::SeqCst) {
        return Err("CERBERUS is shutting down".into());
    }
    let _op = st.op_lock.lock().map_err(|_| "internal lock error".to_string())?;

    let mut p = PathBuf::from(path);
    if let Some(first) = archive::first_volume(&p) {
        info!("{} is part of a split archive - mounting it from {}", path, first.display());
        p = first;
    }
    let path = p.to_string_lossy().to_string();
    let path = path.as_str();
    let is_archive = {
        let e = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        archive::kind_for(&p, &e).is_some()
    };
    if !is_archive {
        ensure_admin(st)?;
    }
    let meta = fs::metadata(&p).map_err(|e| format!("cannot open {}: {}", path, e))?;
    if !meta.is_file() {
        return Err(format!("{} is not a file", path));
    }
    let canon = clean_path(&p);
    let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| canon.clone());
    if st.list().iter().any(|m| m.path.eq_ignore_ascii_case(&canon)) {
        return Err(format!("{} is already mounted", name));
    }
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let arch = archive::kind_for(&p, &ext);
    if ext == "001" && arch.is_none() {
        return Err(format!("{}: only split .7z / .zip / .tar archives (x.7z.001) are supported", name));
    }
    if !ext.is_empty() && !SUPPORTED.contains(&ext.as_str()) {
        return Err(format!(".{} is not supported (supported: {})", ext, SUPPORTED.join(", ")));
    }
    let det = if matches!(ext.as_str(), "img" | "ima" | "flp" | "raw") { detect::detect(&p).ok() } else { None };
    let contents = det.as_ref().map(|d| d.description.clone()).unwrap_or_default();

    // Windows-native route.
    let native: Option<(&str, bool, &str)> = match ext.as_str() {
        "iso" => Some(("", true, "ISO disc image")),
        "vhd" => Some(("", false, "VHD virtual disk")),
        "vhdx" => Some(("", false, "VHDX virtual disk")),
        "img" | "ima" | "flp" | "raw" if det.as_ref().map(|d| d.kind) == Some(detect::Kind::Iso9660) => {
            Some(("ISO", true, "ISO disc image (.img)"))
        }
        _ => None,
    };

    let _busy = Busy::new(st, format!("Mounting {}…", name));
    let mut info = MountInfo {
        id: st.new_id(),
        path: canon.clone(),
        name: name.clone(),
        kind: String::new(),
        route: String::new(),
        storage: String::new(),
        contents,
        readonly,
        removable: false,
        iso: false,
        optical: false,
        overlay: false,
        saved_changes: 0,
        archive: false,
        folder: None,
        size: if ext == "001" { archive::volume_set_size(&p) } else { meta.len() },
        letters: Vec::new(),
        skipped: Vec::new(),
        wsl: Vec::new(),
        disk_number: None,
        style: String::new(),
        iqn: None,
        mounted_at: now(),
        note: None,
    };

    if let Some(kind) = arch {
        info!("Mounting {} ({}) via Windows Projected File System", name, kind.label());
        let r = archmount::mount(&st.base, &info.id, &p, kind).map_err(|e| {
            warn!("Mount failed for {}: {}", name, e);
            e
        })?;
        info.archive = true;
        info.readonly = true;
        info.kind = r.kind;
        info.route = "Archive (ProjFS)".into();
        info.contents = r.detail.clone();
        info.letters = vec![r.letter];
        info.folder = Some(r.folder);
        info.note = r.note;
        info!("{}: {}", name, r.detail);
    } else if let Some((storage, iso, kind)) = native {
        let ro = readonly || iso;
        info.readonly = ro;
        info.iso = iso;
        info.kind = kind.into();
        info.route = "Windows native".into();
        info.storage = storage.to_string();
        info!("Mounting {} ({}, {}) via Windows Virtual Disk", name, kind, if ro { "read-only" } else { "read-write" });
        let v = ps::run(&ps::native_mount(&canon, ro, storage, iso), 90)?;
        apply_disk_result(&mut info, &v, iso);
    } else {
        if let Some(e) = st.target_error.lock().ok().and_then(|g| g.clone()) {
            return Err(format!("the iSCSI engine is not running: {}", e));
        }
        let opened = formats::open(&p, &ext, readonly, det.as_ref())?;
        let eff_ro = readonly || opened.optical;
        info.readonly = eff_ro;
        info.optical = opened.optical;
        info.removable = opened.removable;
        info.kind = opened.kind.clone();
        if !opened.detail.is_empty() {
            info.contents = opened.detail.clone();
        }
        info!("{}: {}", name, opened.detail);

        // Overlay for container formats.
        let mut overlay = None;
        let mut fresh_overlay = false;
        if opened.container {
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let key = fnv1a(canon.to_lowercase().as_bytes());
            let r = Overlay::open(&st.overlay_dir(), key, meta.len(), mtime, !eff_ro)
                .map_err(|e| format!("cannot open the changes file: {}", e))?;
            if r.discarded_stale {
                warn!("{}: saved changes were discarded because the image file changed since they were made", name);
            }
            if r.overlay.is_some() {
                // Remember which image this changes file belongs to (Settings > Saved changes).
                let _ = fs::write(st.overlay_dir().join(format!("{:016x}.path", key)), &canon);
            }
            if let Some(o) = r.overlay {
                info.saved_changes = o.saved_chunks as u64 * CHUNK;
                fresh_overlay = o.saved_chunks == 0;
                if o.saved_chunks > 0 {
                    info!("{}: re-applying {} of saved changes", name, mb(info.saved_changes));
                }
                overlay = Some(o);
            }
        }
        info.overlay = overlay.is_some();

        let disk = Disk::new(opened.backend, opened.block_size, eff_ro, opened.removable, opened.optical, &canon, overlay)
            .map_err(|e| format!("cannot open image: {}", e))?;
        if disk.tail_bytes > 0 {
            warn!("{}: last {} bytes are not a whole sector and are ignored", name, disk.tail_bytes);
        }
        let iqn = format!("{}{}", IQN_PREFIX, info.id);
        st.registry.add(&iqn, Arc::new(disk));
        st.portal_used.store(true, Ordering::SeqCst);
        info.route = if opened.optical { "iSCSI loopback (CD/DVD)".into() } else { "iSCSI loopback".into() };
        info.iqn = Some(iqn.clone());
        info!(
            "Mounting {} ({}, {}{}) via iSCSI loopback",
            name,
            info.kind,
            if eff_ro { "read-only" } else { "read-write" },
            if info.overlay && !eff_ro { ", changes go to an overlay" } else { "" }
        );
        match ps::run(&ps::iscsi_connect(&iqn, st.port, opened.optical), 120) {
            Ok(v) => {
                if opened.optical {
                    info.letters = ps::letters(&v);
                    if info.letters.is_empty() {
                        info.note = Some(NOTE_NO_CD_LETTER.into());
                    }
                } else {
                    apply_disk_result(&mut info, &v, false);
                }
            }
            Err(e) => {
                let _ = ps::run(&ps::iscsi_disconnect(&iqn, &[]), 60);
                if let Some(d) = st.registry.remove(&iqn) {
                    d.set_keep_changes(!fresh_overlay);
                }
                warn!("Mount failed for {}: {}", name, e);
                return Err(e);
            }
        }
    }

    if info.letters.is_empty() && info.note.is_none() {
        info.note = Some(NOTE_NO_LETTER.into());
    }
    info!("Mounted {} -> {}", name, letters_text(&info.letters));
    if let Ok(mut m) = st.mounts.lock() {
        m.push(info.clone());
    }
    st.save();
    Ok(info)
}

fn unmount_one(st: &AppState, m: &MountInfo, keep_changes: bool) -> Result<(), String> {
    if m.archive {
        archmount::unmount(&m.id)?;
        info!("Unmounted {}", m.name);
        return Ok(());
    }
    if !m.wsl.is_empty() {
        if let Some(d) = m.disk_number {
            match ps::run(&ps::wsl_unmount(d), 90) {
                Ok(_) => info!("{}: detached from WSL", m.name),
                Err(e) => warn!("{}: could not detach from WSL ({})", m.name, e),
            }
        }
    }
    match &m.iqn {
        Some(iqn) => {
            ps::run(&ps::iscsi_disconnect(iqn, &m.letters), 60)?;
            if let Some(d) = st.registry.remove(iqn) {
                if d.has_overlay() && !m.readonly {
                    let bytes = d.overlay_bytes();
                    if keep_changes {
                        info!("{}: keeping {} of changes for next time", m.name, mb(bytes));
                    } else {
                        info!("{}: discarding {} of changes", m.name, mb(bytes));
                    }
                }
                d.set_keep_changes(keep_changes);
            }
        }
        None => {
            ps::run(&ps::native_dismount(&m.path, &m.storage, &m.letters), 60)?;
        }
    }
    info!("Unmounted {}", m.name);
    Ok(())
}

pub fn unmount(st: &AppState, id: &str, keep_changes: bool) -> Result<(), String> {
    let _op = st.op_lock.lock().map_err(|_| "internal lock error".to_string())?;
    let m = st.find(id)?;
    let _busy = Busy::new(st, format!("Unmounting {}…", m.name));
    match unmount_one(st, &m, keep_changes) {
        Ok(()) => {
            st.remove(&m.id);
            Ok(())
        }
        Err(e) => {
            warn!("Unmount failed for {}: {}", m.name, e);
            Err(e)
        }
    }
}

/// Returns the list of failures (empty = everything unmounted). Changes are kept.
pub fn unmount_all(st: &AppState) -> Vec<String> {
    let _op = match st.op_lock.lock() {
        Ok(g) => g,
        Err(_) => return vec!["internal lock error".into()],
    };
    let list = st.list();
    if list.is_empty() {
        return Vec::new();
    }
    let _busy = Busy::new(st, format!("Unmounting {} image(s)…", list.len()));
    let mut errs = Vec::new();
    for m in list {
        let mut r = unmount_one(st, &m, true);
        if r.is_err() {
            thread::sleep(Duration::from_millis(1500));
            r = unmount_one(st, &m, true);
        }
        match r {
            Ok(()) => st.remove(&m.id),
            Err(e) => {
                warn!("Unmount failed for {}: {}", m.name, e);
                errs.push(format!("{}: {}", m.name, e));
            }
        }
    }
    errs
}

/// Delete saved changes for an image that is not currently mounted.
pub fn discard_saved(st: &AppState, path: &str) -> Result<bool, String> {
    let canon = clean_path(Path::new(path));
    if st.list().iter().any(|m| m.path.eq_ignore_ascii_case(&canon)) {
        return Err("unmount the image first".into());
    }
    let key = fnv1a(canon.to_lowercase().as_bytes());
    let (d, i) = Overlay::paths(&st.overlay_dir(), key);
    let existed = d.exists() || i.exists();
    let _ = fs::remove_file(d);
    let _ = fs::remove_file(i);
    let _ = fs::remove_file(st.overlay_dir().join(format!("{:016x}.path", key)));
    Ok(existed)
}

/// One kept changes file (data\overlays\<key>.dat/.idx/.path).
#[derive(Serialize, Clone, Debug)]
pub struct SavedChange {
    pub key: String,
    /// Image the changes belong to (empty if unknown - made before v0.6.1).
    pub path: String,
    pub name: String,
    pub size: u64,
    /// The image file still exists.
    pub exists: bool,
    /// The image is mounted right now (cannot be deleted).
    pub mounted: bool,
    /// Seconds since 1970 (last write of the changes file).
    pub modified: u64,
}

fn is_key(s: &str) -> bool {
    s.len() == 16 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// All kept changes files, newest first. Orphaned .path files are cleaned up.
pub fn list_saved(st: &AppState) -> Vec<SavedChange> {
    let dir = st.overlay_dir();
    let mounted_keys: Vec<String> =
        st.list().iter().map(|m| format!("{:016x}", fnv1a(m.path.to_lowercase().as_bytes()))).collect();
    let mut out = Vec::new();
    let rd = match fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return out,
    };
    for e in rd.flatten() {
        let p = e.path();
        let stem = p.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
        let ext = p.extension().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        if !is_key(&stem) {
            continue;
        }
        if ext == "path" {
            if !dir.join(format!("{}.dat", stem)).exists() {
                let _ = fs::remove_file(&p);
            }
            continue;
        }
        if ext != "dat" {
            continue;
        }
        let meta = match fs::metadata(&p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let path = fs::read_to_string(dir.join(format!("{}.path", stem))).unwrap_or_default().trim().to_string();
        let name = if path.is_empty() {
            format!("Unknown image ({})", stem)
        } else {
            Path::new(&path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.clone())
        };
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        out.push(SavedChange {
            mounted: mounted_keys.contains(&stem),
            key: stem,
            exists: !path.is_empty() && Path::new(&path).exists(),
            name,
            path,
            size: meta.len(),
            modified,
        });
    }
    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out
}

/// Delete one kept changes file by its key (from `list_saved`).
pub fn delete_saved(st: &AppState, key: &str) -> Result<(), String> {
    let key = key.trim().to_lowercase();
    if !is_key(&key) {
        return Err("invalid changes-file id".into());
    }
    if let Some(s) = list_saved(st).into_iter().find(|s| s.key == key) {
        if s.mounted {
            return Err(format!("{} is mounted - unmount it first", s.name));
        }
        info!("Deleted saved changes for {} ({})", s.name, mb(s.size));
    }
    let dir = st.overlay_dir();
    for ext in ["dat", "idx", "path"] {
        let f = dir.join(format!("{}.{}", key, ext));
        if f.exists() {
            fs::remove_file(&f).map_err(|e| format!("cannot delete {}: {}", f.display(), e))?;
        }
    }
    Ok(())
}

// ------------------------------------------------------ initialize & format

fn check_fs(fs_name: &str) -> Result<&'static str, String> {
    match fs_name.to_uppercase().as_str() {
        "NTFS" => Ok("NTFS"),
        "EXFAT" => Ok("exFAT"),
        "FAT32" => Ok("FAT32"),
        "FAT" => Ok("FAT"),
        other => Err(format!("unsupported file system '{}'", other)),
    }
}

fn check_label(label: &str, fs_name: &str) -> String {
    let max = match fs_name {
        "NTFS" => 32,
        "exFAT" => 15,
        _ => 11, // FAT / FAT32
    };
    let bad = ['"', '*', '/', ':', '<', '>', '?', '\\', '|', '+', ',', '.', ';', '=', '[', ']'];
    let mut s: String = label
        .trim()
        .chars()
        .filter(|c| !c.is_control() && (fs_name == "NTFS" || !bad.contains(c)))
        .take(max)
        .collect();
    if s.trim().is_empty() {
        s = "CERBERUS".into();
    }
    if fs_name == "FAT32" || fs_name == "FAT" {
        s = s.to_uppercase();
    }
    s
}

fn check_writable(m: &MountInfo) -> Result<(), String> {
    if m.iso || m.optical {
        return Err("CD/DVD images are read-only and cannot be formatted.".into());
    }
    if m.archive {
        return Err("Archives are mounted read-only and cannot be formatted.".into());
    }
    if m.readonly {
        return Err("This image is mounted read-only. Unmount it and mount it again with \"Mount read-only\" unticked.".into());
    }
    Ok(())
}

fn full_timeout(m: &MountInfo, full: bool) -> u64 {
    if full {
        3600 + m.size / (1 << 30) * 60
    } else {
        300
    }
}

/// Blank / uninitialised disk: initialize (GPT/MBR), create one partition, format it.
pub fn initialize_and_format(st: &AppState, id: &str, style: &str, fs_name: &str, label: &str, full: bool) -> Result<MountInfo, String> {
    let _op = st.op_lock.lock().map_err(|_| "internal lock error".to_string())?;
    let mut m = st.find(id)?;
    check_writable(&m)?;
    if !m.skipped.is_empty() {
        return Err("This disk already holds partitions (e.g. Linux ext4) - CERBERUS will not initialize it.".into());
    }
    let disk = m.disk_number.ok_or("Windows did not report a disk number for this image - cannot initialize it.")?;
    let style = match style.to_uppercase().as_str() {
        "GPT" => "GPT",
        "MBR" => "MBR",
        other => return Err(format!("unsupported partition style '{}'", other)),
    };
    let fs_name = check_fs(fs_name)?;
    let label = check_label(label, fs_name);

    let _busy = Busy::new(st, format!("Initializing & formatting {} ({} / {})…", m.name, style, fs_name));
    info!(
        "Initialize & format {} (Disk {}): {} partition table, {} \"{}\"{}",
        m.name,
        disk,
        style,
        fs_name,
        label,
        if full { ", full format" } else { ", quick format" }
    );
    let v = ps::run(&ps::init_and_format(disk, style, fs_name, &label, full), full_timeout(&m, full)).map_err(|e| {
        warn!("Initialize & format failed for {}: {}", m.name, e);
        e
    })?;
    apply_disk_result(&mut m, &v, false);
    if m.letters.is_empty() && m.note.is_none() {
        m.note = Some(NOTE_NO_LETTER.into());
    }
    info!("{} is ready -> {}", m.name, letters_text(&m.letters));
    st.replace(&m);
    Ok(m)
}

/// Reformat one existing volume of a mounted image. Erases everything on that volume.
pub fn format_drive(st: &AppState, id: &str, letter: &str, fs_name: &str, label: &str, full: bool) -> Result<MountInfo, String> {
    let _op = st.op_lock.lock().map_err(|_| "internal lock error".to_string())?;
    let mut m = st.find(id)?;
    check_writable(&m)?;
    let letter = letter.trim().trim_end_matches(':').to_uppercase();
    if !m.letters.iter().any(|l| l.eq_ignore_ascii_case(&letter)) {
        return Err(format!("{}: does not belong to {} - refusing to format it.", letter, m.name));
    }
    let fs_name = check_fs(fs_name)?;
    let label = check_label(label, fs_name);

    let _busy = Busy::new(st, format!("Formatting {}: ({})…", letter, fs_name));
    info!(
        "Format {}: of {} as {} \"{}\"{}",
        letter,
        m.name,
        fs_name,
        label,
        if full { " (full format)" } else { " (quick format)" }
    );
    ps::run(&ps::format_volume(&letter, fs_name, &label, full), full_timeout(&m, full)).map_err(|e| {
        warn!("Format failed for {}: {}", letter, e);
        e
    })?;
    if m.skipped.is_empty() {
        m.note = None;
    }
    info!("Formatted {}: as {}", letter, fs_name);
    st.replace(&m);
    Ok(m)
}

/// Called on window close: unmount everything (keeping changes), then remove the portal.
pub fn shutdown(st: &AppState) {
    write_session(st, &st.list());
    info!("Shutting down - unmounting all images");
    let errs = unmount_all(st);
    if !errs.is_empty() {
        warn!("{} image(s) could not be unmounted cleanly", errs.len());
    }
    if st.portal_used.load(Ordering::SeqCst) {
        let _ = ps::run(&ps::remove_portal(st.port), 30);
    }
    info!("Shutdown complete");
}

/// Runs once at startup (background thread): removes leftovers from a crash.
pub fn startup_cleanup(st: &AppState) {
    {
        let _op = st.op_lock.lock();
        let _busy = Busy::new(st, "Checking for leftover mounts…".into());
        let stale = st.stale.lock().map(|s| s.clone()).unwrap_or_default();
        let native: Vec<String> = stale
            .iter()
            .filter(|m| m.iqn.is_none() && !m.archive)
            .map(|m| format!("{}|{}", m.path, m.storage))
            .collect();
        if !stale.is_empty() && !session_file(st).exists() {
            write_session(st, &stale);
        }
        // Administrator rights are only needed here to clean up disk/disc mounts
        // left behind by a crash - otherwise they are requested on first use.
        let needs_admin = stale.iter().any(|m| !m.archive);
        let admin = if needs_admin { ensure_admin(st) } else { Err(String::new()) };
        if let Err(e) = &admin {
            if needs_admin {
                warn!("{}", e);
            }
        }
        let stale_archives: Vec<(String, String)> = stale
            .iter()
            .filter(|m| m.archive)
            .filter_map(|m| Some((m.letters.first()?.clone(), m.folder.clone()?)))
            .collect();
        let purged = archmount::cleanup_stale(&st.base, &stale_archives);
        if purged > 0 {
            info!("Cleaned up {} leftover archive folder(s) from a previous run", purged);
        }
        if admin.is_ok() {
            match ps::run(&ps::startup_cleanup(IQN_PREFIX, &native), 90) {
                Ok(v) => {
                    let s = v.get("sessions").and_then(|x| x.as_i64()).unwrap_or(0);
                    let i = v.get("images").and_then(|x| x.as_i64()).unwrap_or(0);
                    if s > 0 || i > 0 {
                        info!("Cleaned up leftovers from a previous run: {} iSCSI session(s), {} image(s)", s, i);
                    }
                }
                Err(e) => warn!("Startup cleanup: {}", e),
            }
        }
        if let Ok(mut g) = st.stale.lock() {
            g.clear();
        }
        st.save();
    }
    st.ready.store(true, Ordering::SeqCst);
    info!("Ready");
}

// ------------------------------------------------------ helpers for Phase 6

/// Canonical path of an image (after resolving split-archive volumes).
pub fn canonical(path: &str) -> String {
    let p = PathBuf::from(path);
    let p = archive::first_volume(&p).unwrap_or(p);
    clean_path(&p)
}

/// The mount that uses this image file, if any.
pub fn find_by_path(st: &AppState, path: &str) -> Option<MountInfo> {
    let c = canonical(path);
    st.list().into_iter().find(|m| m.path.eq_ignore_ascii_case(&c))
}

#[derive(Serialize, Deserialize)]
struct SessionItem {
    path: String,
    readonly: bool,
}

fn session_file(st: &AppState) -> PathBuf {
    st.base.join("data").join("session.json")
}

fn write_session(st: &AppState, list: &[MountInfo]) {
    let items: Vec<SessionItem> = list.iter().map(|m| SessionItem { path: m.path.clone(), readonly: m.readonly }).collect();
    if let Ok(s) = serde_json::to_string_pretty(&items) {
        let _ = fs::write(session_file(st), s);
    }
}

/// Re-mount last session's images (Settings: "Re-mount at startup"). The
/// session file is consumed either way.
pub fn restore_session(st: &AppState) -> usize {
    let f = session_file(st);
    let items: Vec<SessionItem> = fs::read_to_string(&f)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let _ = fs::remove_file(&f);
    if !st.settings().restore_mounts || items.is_empty() {
        return 0;
    }
    info!("Re-mounting {} image(s) from the last session", items.len());
    let mut n = 0;
    for it in items {
        if !Path::new(&it.path).exists() {
            warn!("Not re-mounting {} - the file no longer exists", it.path);
            continue;
        }
        match mount(st, &it.path, it.readonly) {
            Ok(_) => n += 1,
            Err(e) => warn!("Could not re-mount {}: {}", it.path, e),
        }
    }
    n
}

/// Partition numbers of Linux ext2/3/4 partitions listed in `skipped`.
pub fn linux_partitions(m: &MountInfo) -> Vec<u32> {
    m.skipped
        .iter()
        .filter(|s| s.contains("Linux ext"))
        .filter_map(|s| s.strip_prefix("partition ")?.split(':').next()?.trim().parse().ok())
        .collect()
}

/// Attach the disk's Linux partitions to WSL 2 so Explorer can browse them.
pub fn open_in_wsl(st: &AppState, id: &str) -> Result<MountInfo, String> {
    let _op = st.op_lock.lock().map_err(|_| "internal lock error".to_string())?;
    let mut m = st.find(id)?;
    if !m.wsl.is_empty() {
        return Ok(m);
    }
    let disk = m.disk_number.ok_or("Windows did not report a disk number for this image.")?;
    let parts = linux_partitions(&m);
    if parts.is_empty() {
        return Err("This disk has no Linux ext2/3/4 partition that WSL can mount (LVM and swap are not supported).".into());
    }
    if !st.has_admin() {
        let _busy = Busy::new(st, "Waiting for administrator permission (UAC)…".into());
        broker::ensure()?;
    }
    let _busy = Busy::new(st, format!("Attaching {} to WSL…", m.name));
    info!("Attaching {} (Disk {}, partition(s) {:?}) to WSL", m.name, disk, parts);
    let v = ps::run(&ps::wsl_mount(disk, &parts, m.readonly), 240).map_err(|e| {
        warn!("Open in WSL failed for {}: {}", m.name, e);
        e
    })?;
    m.wsl = ps::paths(&v);
    info!("{}: available in Explorer at {}", m.name, m.wsl.join(", "));
    st.replace(&m);
    Ok(m)
}
