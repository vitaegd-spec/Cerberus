//! Phase 5 - archives as drives.
//! Each archive is projected into data\archives\<id> with Windows' Projected File
//! System and that folder is given its own drive letter.
//!
//! Drive letters: normally the CERBERUS window runs non-elevated, so a plain
//! DefineDosDevice letter lands in the user's own session, exactly where Explorer
//! looks. If CERBERUS itself was started elevated, an elevated process has its own
//! private set of drive letters that Explorer can't see, so the letter is created:
//!   1. in the global namespace (visible to everyone), or
//!   2. in the signed-in user's normal session via a one-shot `subst` task
//!      (plus a private copy so CERBERUS itself can use it), or
//!   3. privately only (last resort - the Folder button still works).

use std::collections::HashMap;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use crate::archive::{self, Kind};
use crate::projfs::{self, Provider, Running};
use crate::ps;

#[link(name = "kernel32")]
extern "system" {
    fn GetLogicalDrives() -> u32;
    fn DefineDosDeviceW(flags: u32, device: *const u16, target: *const u16) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn SHChangeNotify(event: i32, flags: u32, item1: *const c_void, item2: *const c_void);
}

const DDD_RAW_TARGET_PATH: u32 = 0x1;
const DDD_REMOVE_DEFINITION: u32 = 0x2;
const DDD_EXACT_MATCH_ON_REMOVE: u32 = 0x4;
const SHCNE_DRIVEREMOVED: i32 = 0x80;
const SHCNE_DRIVEADD: i32 = 0x100;
const SHCNF_PATHW: u32 = 0x5;
const MAX_LETTER_TRIES: usize = 6;

/// Archive drive letters are taken from this letter downwards (Settings).
static PREF_LETTER: AtomicU8 = AtomicU8::new(b'Z');

pub fn set_letter_pref(c: char) {
    let c = c.to_ascii_uppercase();
    if ('D'..='Z').contains(&c) {
        PREF_LETTER.store(c as u8, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Scope {
    /// Non-elevated CERBERUS: the user's own session (Explorer sees it).
    Session,
    /// Global namespace - visible to every session.
    Global,
    /// `subst` in the signed-in user's session + a private copy for CERBERUS.
    User,
    /// CERBERUS's own (elevated) session only.
    Private,
}

struct Mounted {
    run: Running,
    letter: String,
    scope: Scope,
    target: String,
    root: PathBuf,
}

fn table() -> &'static Mutex<HashMap<String, Mounted>> {
    static T: OnceLock<Mutex<HashMap<String, Mounted>>> = OnceLock::new();
    T.get_or_init(|| Mutex::new(HashMap::new()))
}

pub struct ArchiveMount {
    pub letter: String,
    pub folder: String,
    pub kind: String,
    pub detail: String,
    /// Set when the drive letter is only visible inside CERBERUS.
    pub note: Option<String>,
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn archives_dir(base: &Path) -> PathBuf {
    base.join("data").join("archives")
}

fn raw_target(root: &Path) -> String {
    format!("\\??\\{}", root.display())
}

/// Free letters: the preferred letter downwards to D:, then the letters above it
/// (as seen by CERBERUS's own session).
fn free_letters() -> Vec<char> {
    let used = unsafe { GetLogicalDrives() };
    let pref = PREF_LETTER.load(Ordering::Relaxed).clamp(b'D', b'Z');
    (b'D'..=pref)
        .rev()
        .chain((pref + 1..=b'Z').rev())
        .map(|c| c as char)
        .filter(|c| used & (1u32 << (*c as u8 - b'A')) == 0)
        .collect()
}

fn define_global(letter: char, target: &str) -> Result<(), String> {
    let t = w(target);
    let ok = unsafe { DefineDosDeviceW(DDD_RAW_TARGET_PATH, w(&format!("Global\\{}:", letter)).as_ptr(), t.as_ptr()) };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn define_private(letter: char, target: &str) -> Result<(), String> {
    let t = w(target);
    let ok = unsafe { DefineDosDeviceW(DDD_RAW_TARGET_PATH, w(&format!("{}:", letter)).as_ptr(), t.as_ptr()) };
    if ok != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().to_string())
    }
}

fn undefine(name: &str, target: &str) {
    let n = w(name);
    let t = w(target);
    unsafe {
        let flags = DDD_RAW_TARGET_PATH | DDD_REMOVE_DEFINITION | DDD_EXACT_MATCH_ON_REMOVE;
        if DefineDosDeviceW(flags, n.as_ptr(), t.as_ptr()) == 0 {
            let _ = DefineDosDeviceW(DDD_RAW_TARGET_PATH | DDD_REMOVE_DEFINITION, n.as_ptr(), std::ptr::null());
        }
    }
}

fn notify(letter: &str, added: bool) {
    let p = w(&format!("{}:\\", letter));
    unsafe {
        SHChangeNotify(
            if added { SHCNE_DRIVEADD } else { SHCNE_DRIVEREMOVED },
            SHCNF_PATHW,
            p.as_ptr() as *const c_void,
            std::ptr::null(),
        );
    }
}

/// Outcome of trying one letter.
enum Attempt {
    Done(Scope),
    /// Letter is taken in the user's session (e.g. a mapped network drive) - try the next one.
    Busy,
    Failed(String),
}

fn try_letter(letter: char, target: &str, root: &Path) -> Attempt {
    if !ps::is_elevated() {
        return match define_private(letter, target) {
            Ok(()) => Attempt::Done(Scope::Session),
            Err(e) => Attempt::Failed(e),
        };
    }
    match define_global(letter, target) {
        Ok(()) => return Attempt::Done(Scope::Global),
        Err(e) => debug!("global drive letter {}: not allowed ({}) - using the signed-in user's session", letter, e),
    }
    let folder = root.display().to_string();
    match ps::run(&ps::user_subst(&letter.to_string(), &folder, false), 60) {
        Ok(v) => {
            let code = v.get("code").and_then(|c| c.as_i64()).unwrap_or(-1);
            if code == 0 {
                if let Err(e) = define_private(letter, target) {
                    debug!("private copy of {}: failed ({}) - harmless", letter, e);
                }
                Attempt::Done(Scope::User)
            } else {
                debug!("subst {}: returned {} - letter probably in use in the user's session", letter, code);
                Attempt::Busy
            }
        }
        Err(e) => Attempt::Failed(e),
    }
}

/// Create the drive letter, trying a few letters if the user already uses some.
fn assign_letter(target: &str, root: &Path) -> Result<(char, Scope), String> {
    let letters = free_letters();
    if letters.is_empty() {
        return Err("no free drive letter".into());
    }
    let mut last_err = String::new();
    for &l in letters.iter().take(MAX_LETTER_TRIES) {
        match try_letter(l, target, root) {
            Attempt::Done(scope) => return Ok((l, scope)),
            Attempt::Busy => continue,
            Attempt::Failed(e) => {
                last_err = e;
                break;
            }
        }
    }
    if !ps::is_elevated() {
        return Err(if last_err.is_empty() { "could not create a drive letter".into() } else { format!("could not create a drive letter ({})", last_err) });
    }
    // Last resort: a letter only CERBERUS can see (the Folder button still works).
    let l = letters[0];
    warn!(
        "Could not create a drive letter Explorer can see{} - using a private one",
        if last_err.is_empty() { String::new() } else { format!(" ({})", last_err) }
    );
    define_private(l, target).map_err(|e| format!("could not create drive {}: ({})", l, e))?;
    Ok((l, Scope::Private))
}

fn remove_letter(letter: &str, scope: Scope, target: &str, root: &Path) {
    match scope {
        Scope::Session => undefine(&format!("{}:", letter), target),
        Scope::Global => undefine(&format!("Global\\{}:", letter), target),
        Scope::User => {
            let folder = root.display().to_string();
            if let Err(e) = ps::run(&ps::user_subst(letter, &folder, true), 60) {
                warn!("could not remove drive {}: from your session ({}) - run `subst {}: /D` to remove it", letter, e, letter);
            }
            undefine(&format!("{}:", letter), target);
        }
        Scope::Private => undefine(&format!("{}:", letter), target),
    }
    notify(letter, false);
}

/// Make sure ProjFS is usable, enabling the Windows feature if needed.
fn ensure_projfs() -> Result<(), String> {
    if projfs::load().is_ok() {
        return Ok(());
    }
    info!("Windows Projected File System is not enabled - enabling the Client-ProjFS feature (one-time)…");
    let v = ps::run(&ps::enable_projfs(), 600).map_err(|e| format!("could not enable Windows Projected File System: {}", e))?;
    let restart = v.get("restart").and_then(|r| r.as_bool()).unwrap_or(false);
    match projfs::load() {
        Ok(_) => {
            info!("Windows Projected File System enabled");
            Ok(())
        }
        Err(_) => Err(if restart {
            "Windows Projected File System was just enabled - restart Windows, then mount the archive again.".into()
        } else {
            "Windows Projected File System is enabled but could not be loaded - restart Windows and try again.".into()
        }),
    }
}

fn human(n: u64) -> String {
    crate::conn::human(n)
}

pub fn mount(base: &Path, id: &str, path: &Path, kind: Kind) -> Result<ArchiveMount, String> {
    ensure_projfs()?;
    let raw = archive::list(path, kind)?;
    let prov = Provider::new(path, kind, raw);
    if prov.files == 0 && prov.dirs == 0 {
        return Err("the archive is empty".into());
    }
    let vols = archive::volume_count(path);
    let detail = format!(
        "{}{} - {} file(s), {} folder(s), {} uncompressed",
        kind.label(),
        if vols > 1 { format!(" ({} volumes)", vols) } else { String::new() },
        prov.files,
        prov.dirs,
        human(prov.bytes)
    );
    let root = archives_dir(base).join(id);
    if root.exists() {
        let _ = projfs::purge(&root);
    }
    std::fs::create_dir_all(&root).map_err(|e| format!("cannot create {}: {}", root.display(), e))?;
    let run = match projfs::start(&root, Arc::new(prov), true) {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&root);
            // The DLL can exist while the ProjFS driver is still switched off.
            if let Ok(v) = ps::run(&ps::enable_projfs(), 600) {
                if v.get("changed").and_then(|c| c.as_bool()).unwrap_or(false) {
                    return Err("Windows Projected File System was just enabled - restart Windows, then mount the archive again.".into());
                }
            }
            return Err(e);
        }
    };
    let target = raw_target(&root);
    let (letter, scope) = match assign_letter(&target, &root) {
        Ok(x) => x,
        Err(e) => {
            run.stop();
            let _ = projfs::purge(&root);
            return Err(e);
        }
    };
    let note = match scope {
        Scope::Session => {
            debug!("drive {}: created in your session", letter);
            None
        }
        Scope::Global => {
            debug!("drive {}: created in the global namespace", letter);
            None
        }
        Scope::User => {
            debug!("drive {}: created in the signed-in user's session", letter);
            None
        }
        Scope::Private => Some(format!(
            "Drive {}: is only visible inside CERBERUS - use the Folder button to browse the archive in Explorer.",
            letter
        )),
    };
    notify(&letter.to_string(), true);
    let letter_s = letter.to_string();
    if let Ok(mut t) = table().lock() {
        t.insert(
            id.to_string(),
            Mounted { run, letter: letter_s.clone(), scope, target, root: root.clone() },
        );
    }
    Ok(ArchiveMount {
        letter: letter_s,
        folder: root.display().to_string(),
        kind: kind.label().to_string(),
        detail,
        note,
    })
}

pub fn unmount(id: &str) -> Result<(), String> {
    let m = table().lock().map_err(|_| "internal lock error".to_string())?.remove(id);
    let m = match m {
        Some(m) => m,
        None => return Ok(()),
    };
    remove_letter(&m.letter, m.scope, &m.target, &m.root);
    m.run.stop();
    if let Err(e) = projfs::purge(&m.root) {
        warn!("{} (it will be removed next time CERBERUS starts)", e);
    }
    Ok(())
}

/// Startup: remove drive letters and projection folders left behind by a crash.
pub fn cleanup_stale(base: &Path, stale: &[(String, String)]) -> usize {
    for (letter, folder) in stale {
        let root = Path::new(folder);
        let target = raw_target(root);
        undefine(&format!("Global\\{}:", letter), &target);
        undefine(&format!("{}:", letter), &target);
        // Best effort: a leftover subst in the user's session (non-zero code = it wasn't there).
        if ps::is_elevated() {
            let _ = ps::run(&ps::user_subst(letter, folder, true), 60);
        }
        notify(letter, false);
    }
    let dir = archives_dir(base);
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                match projfs::purge(&p) {
                    Ok(()) => n += 1,
                    Err(err) => warn!("Startup cleanup: {}", err),
                }
            }
        }
    }
    n
}
