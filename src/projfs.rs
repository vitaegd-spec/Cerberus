//! Windows Projected File System (ProjFS) provider.
//! An archive's table of contents is "projected" into a folder: Explorer sees
//! every file and folder immediately, and a file's bytes are only read from the
//! archive when something actually opens it. ProjectedFSLib.dll is a built-in
//! Windows component (optional feature "Client-ProjFS"); it is loaded at run
//! time so CERBERUS still starts when the feature is not enabled yet.

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::archive::{self, Kind, RawEntry};

// ------------------------------------------------------------------ FFI types

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Guid {
    d1: u32,
    d2: u16,
    d3: u16,
    d4: [u8; 8],
}

#[repr(C)]
pub struct CallbackData {
    size: u32,
    flags: i32,
    ctx: *mut c_void,
    command_id: i32,
    file_id: Guid,
    stream_id: Guid,
    file_path: *const u16,
    version_info: *const c_void,
    pid: u32,
    image_name: *const u16,
    instance: *mut c_void,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct BasicInfo {
    is_dir: u8,
    size: i64,
    creation: i64,
    last_access: i64,
    last_write: i64,
    change: i64,
    attributes: u32,
}

#[repr(C)]
struct VersionInfo {
    provider_id: [u8; 128],
    content_id: [u8; 128],
}

#[repr(C)]
pub struct PlaceholderInfo {
    basic: BasicInfo,
    ea: [u32; 2],
    security: [u32; 2],
    streams: [u32; 2],
    version: VersionInfo,
    variable: [u8; 1],
}

type StartEnumFn = unsafe extern "system" fn(*const CallbackData, *const Guid) -> i32;
type EndEnumFn = unsafe extern "system" fn(*const CallbackData, *const Guid) -> i32;
type GetEnumFn = unsafe extern "system" fn(*const CallbackData, *const Guid, *const u16, *mut c_void) -> i32;
type PlaceholderFn = unsafe extern "system" fn(*const CallbackData) -> i32;
type FileDataFn = unsafe extern "system" fn(*const CallbackData, u64, u32) -> i32;

#[repr(C)]
pub struct Callbacks {
    start_enum: Option<StartEnumFn>,
    end_enum: Option<EndEnumFn>,
    get_enum: Option<GetEnumFn>,
    placeholder: Option<PlaceholderFn>,
    file_data: Option<FileDataFn>,
    query_name: usize,
    notification: usize,
    cancel: usize,
}

type StartVirt = unsafe extern "system" fn(*const u16, *const Callbacks, *const c_void, *const c_void, *mut *mut c_void) -> i32;
type StopVirt = unsafe extern "system" fn(*mut c_void);
type MarkRoot = unsafe extern "system" fn(*const u16, *const u16, *const c_void, *const Guid) -> i32;
type WritePh = unsafe extern "system" fn(*mut c_void, *const u16, *const PlaceholderInfo, u32) -> i32;
type FillDir = unsafe extern "system" fn(*const u16, *const BasicInfo, *mut c_void) -> i32;
type WriteData = unsafe extern "system" fn(*mut c_void, *const Guid, *mut c_void, u64, u32) -> i32;
type AllocBuf = unsafe extern "system" fn(*mut c_void, usize) -> *mut c_void;
type FreeBuf = unsafe extern "system" fn(*mut c_void);
type NameMatch = unsafe extern "system" fn(*const u16, *const u16) -> u8;
type NameCmp = unsafe extern "system" fn(*const u16, *const u16) -> i32;

pub struct Api {
    start: StartVirt,
    stop: StopVirt,
    mark: MarkRoot,
    write_ph: WritePh,
    fill: FillDir,
    write_data: WriteData,
    alloc: AllocBuf,
    free: FreeBuf,
    name_match: NameMatch,
    name_cmp: NameCmp,
}

static API: OnceLock<Api> = OnceLock::new();

const S_OK: i32 = 0;
const E_FILE_NOT_FOUND: i32 = 0x8007_0002u32 as i32;
const E_INVALIDARG: i32 = 0x8007_0057u32 as i32;
const E_FAIL: i32 = 0x8000_4005u32 as i32;
const E_OUTOFMEMORY: i32 = 0x8007_000Eu32 as i32;
const FLAG_RESTART_SCAN: i32 = 1;
const CHUNK: usize = 4 << 20;

static CALLBACKS: Callbacks = Callbacks {
    start_enum: Some(cb_start_enum as StartEnumFn),
    end_enum: Some(cb_end_enum as EndEnumFn),
    get_enum: Some(cb_get_enum as GetEnumFn),
    placeholder: Some(cb_placeholder as PlaceholderFn),
    file_data: Some(cb_file_data as FileDataFn),
    query_name: 0,
    notification: 0,
    cancel: 0,
};

/// Load ProjectedFSLib.dll (only succeeds once the Client-ProjFS feature is on).
pub fn load() -> Result<&'static Api, String> {
    if let Some(a) = API.get() {
        return Ok(a);
    }
    let lib = unsafe { libloading::Library::new("ProjectedFSLib.dll") }
        .map_err(|e| format!("Windows Projected File System is not available ({})", e))?;
    let api = unsafe {
        macro_rules! sym {
            ($n:literal, $t:ty) => {
                *lib.get::<$t>($n).map_err(|e| format!("ProjectedFSLib.dll: {}", e))?
            };
        }
        Api {
            start: sym!(b"PrjStartVirtualizing\0", StartVirt),
            stop: sym!(b"PrjStopVirtualizing\0", StopVirt),
            mark: sym!(b"PrjMarkDirectoryAsPlaceholder\0", MarkRoot),
            write_ph: sym!(b"PrjWritePlaceholderInfo\0", WritePh),
            fill: sym!(b"PrjFillDirEntryBuffer\0", FillDir),
            write_data: sym!(b"PrjWriteFileData\0", WriteData),
            alloc: sym!(b"PrjAllocateAlignedBuffer\0", AllocBuf),
            free: sym!(b"PrjFreeAlignedBuffer\0", FreeBuf),
            name_match: sym!(b"PrjFileNameMatch\0", NameMatch),
            name_cmp: sym!(b"PrjFileNameCompare\0", NameCmp),
        }
    };
    // Keep the DLL loaded for the life of the process.
    Box::leak(Box::new(lib));
    let _ = API.set(api);
    API.get().ok_or_else(|| "ProjFS initialisation failed".to_string())
}

// ------------------------------------------------------------------ provider

pub struct Item {
    name_w: Vec<u16>,
    path_w: Vec<u16>,
    pub size: u64,
    pub is_dir: bool,
    orig: String,
    idx: usize,
}

struct Enum {
    list: Vec<usize>,
    next: usize,
    pattern: Option<Vec<u16>>,
}

pub struct Provider {
    items: Vec<Item>,
    by_path: HashMap<String, usize>,
    children: HashMap<String, Vec<usize>>,
    enums: Mutex<HashMap<u128, Enum>>,
    archive: PathBuf,
    kind: Kind,
    filetime: i64,
    pub files: usize,
    pub dirs: usize,
    pub bytes: u64,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn wide_path(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
}

/// Split an archive path into safe Windows path components.
fn components(name: &str) -> Vec<String> {
    name.split(|c| c == '/' || c == '\\')
        .filter(|c| !c.is_empty() && *c != "." && *c != "..")
        .map(|c| {
            let s: String = c
                .chars()
                .map(|ch| if matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*') || (ch as u32) < 32 { '_' } else { ch })
                .collect();
            let t = s.trim_end_matches(|ch| ch == ' ' || ch == '.').to_string();
            if t.is_empty() { "_".to_string() } else { t }
        })
        .collect()
}

fn key(path: &str) -> String {
    path.trim_matches('\\').to_lowercase()
}

impl Provider {
    pub fn new(archive: &Path, kind: Kind, raw: Vec<RawEntry>) -> Provider {
        let mtime = std::fs::metadata(archive)
            .and_then(|m| m.modified())
            .unwrap_or_else(|_| SystemTime::now());
        let secs = mtime.duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
        let mut p = Provider {
            items: Vec::new(),
            by_path: HashMap::new(),
            children: HashMap::new(),
            enums: Mutex::new(HashMap::new()),
            archive: archive.to_path_buf(),
            kind,
            filetime: (secs + 11_644_473_600) * 10_000_000,
            files: 0,
            dirs: 0,
            bytes: 0,
        };
        for e in raw {
            let comps = components(&e.name);
            if comps.is_empty() {
                continue;
            }
            // Parent folders (implicit in many archives).
            let mut parent = String::new();
            let mut ok = true;
            for c in &comps[..comps.len() - 1] {
                let path = if parent.is_empty() { c.clone() } else { format!("{}\\{}", parent, c) };
                if !p.ensure_dir(&parent, c, &path) {
                    ok = false;
                    break;
                }
                parent = path;
            }
            if !ok {
                continue;
            }
            let leaf = &comps[comps.len() - 1];
            let path = if parent.is_empty() { leaf.clone() } else { format!("{}\\{}", parent, leaf) };
            if e.is_dir {
                p.ensure_dir(&parent, leaf, &path);
                continue;
            }
            match p.by_path.get(&key(&path)).copied() {
                Some(i) if !p.items[i].is_dir => {
                    // Later duplicate (e.g. appended TAR member) wins.
                    let it = &mut p.items[i];
                    p.bytes = p.bytes - it.size + e.size;
                    it.size = e.size;
                    it.orig = e.name.clone();
                    it.idx = e.idx;
                }
                Some(_) => {} // a folder already has this name
                None => {
                    let i = p.items.len();
                    p.items.push(Item {
                        name_w: wide(leaf),
                        path_w: wide(&path),
                        size: e.size,
                        is_dir: false,
                        orig: e.name.clone(),
                        idx: e.idx,
                    });
                    p.by_path.insert(key(&path), i);
                    p.children.entry(key(&parent)).or_default().push(i);
                    p.files += 1;
                    p.bytes += e.size;
                }
            }
        }
        p
    }

    /// Empty provider - used only to clean up a leftover projection folder.
    pub fn empty() -> Provider {
        Provider {
            items: Vec::new(),
            by_path: HashMap::new(),
            children: HashMap::new(),
            enums: Mutex::new(HashMap::new()),
            archive: PathBuf::new(),
            kind: Kind::Zip,
            filetime: 0,
            files: 0,
            dirs: 0,
            bytes: 0,
        }
    }

    fn ensure_dir(&mut self, parent: &str, name: &str, path: &str) -> bool {
        match self.by_path.get(&key(path)).copied() {
            Some(i) => self.items[i].is_dir,
            None => {
                let i = self.items.len();
                self.items.push(Item {
                    name_w: wide(name),
                    path_w: wide(path),
                    size: 0,
                    is_dir: true,
                    orig: String::new(),
                    idx: 0,
                });
                self.by_path.insert(key(path), i);
                self.children.entry(key(parent)).or_default().push(i);
                self.dirs += 1;
                true
            }
        }
    }

    fn basic(&self, it: &Item) -> BasicInfo {
        BasicInfo {
            is_dir: it.is_dir as u8,
            size: if it.is_dir { 0 } else { it.size as i64 },
            creation: self.filetime,
            last_access: self.filetime,
            last_write: self.filetime,
            change: self.filetime,
            attributes: if it.is_dir { 0x10 } else { 0x80 },
        }
    }

    fn lookup(&self, path: &str) -> Option<&Item> {
        self.by_path.get(&key(path)).map(|&i| &self.items[i])
    }
}

// ------------------------------------------------------------------ callbacks

unsafe fn wstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
}

unsafe fn wvec(p: *const u16) -> Vec<u16> {
    let mut v = Vec::new();
    if !p.is_null() {
        let mut n = 0;
        while *p.add(n) != 0 {
            v.push(*p.add(n));
            n += 1;
        }
    }
    v.push(0);
    v
}

fn gid(g: &Guid) -> u128 {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&g.d1.to_le_bytes());
    b[4..6].copy_from_slice(&g.d2.to_le_bytes());
    b[6..8].copy_from_slice(&g.d3.to_le_bytes());
    b[8..16].copy_from_slice(&g.d4);
    u128::from_le_bytes(b)
}

unsafe fn provider<'a>(d: *const CallbackData) -> &'a Provider {
    &*((*d).instance as *const Provider)
}

unsafe extern "system" fn cb_start_enum(d: *const CallbackData, id: *const Guid) -> i32 {
    let api = match API.get() {
        Some(a) => a,
        None => return E_FAIL,
    };
    let p = provider(d);
    let dir = key(&wstr((*d).file_path));
    if !dir.is_empty() && !p.lookup(&dir).map_or(false, |it| it.is_dir) {
        return E_FILE_NOT_FOUND;
    }
    let mut list = p.children.get(&dir).cloned().unwrap_or_default();
    list.sort_by(|&a, &b| (api.name_cmp)(p.items[a].name_w.as_ptr(), p.items[b].name_w.as_ptr()).cmp(&0));
    if let Ok(mut g) = p.enums.lock() {
        g.insert(gid(&*id), Enum { list, next: 0, pattern: None });
    }
    S_OK
}

unsafe extern "system" fn cb_end_enum(d: *const CallbackData, id: *const Guid) -> i32 {
    let p = provider(d);
    if let Ok(mut g) = p.enums.lock() {
        g.remove(&gid(&*id));
    }
    S_OK
}

unsafe extern "system" fn cb_get_enum(d: *const CallbackData, id: *const Guid, search: *const u16, handle: *mut c_void) -> i32 {
    let api = match API.get() {
        Some(a) => a,
        None => return E_FAIL,
    };
    let p = provider(d);
    let mut g = match p.enums.lock() {
        Ok(g) => g,
        Err(_) => return E_FAIL,
    };
    let e = match g.get_mut(&gid(&*id)) {
        Some(e) => e,
        None => return E_INVALIDARG,
    };
    if (*d).flags & FLAG_RESTART_SCAN != 0 || e.pattern.is_none() {
        e.next = 0;
        e.pattern = Some(if search.is_null() || *search == 0 { wide("*") } else { wvec(search) });
    }
    let pattern = e.pattern.clone().unwrap_or_else(|| wide("*"));
    let mut added = 0;
    while e.next < e.list.len() {
        let it = &p.items[e.list[e.next]];
        if (api.name_match)(it.name_w.as_ptr(), pattern.as_ptr()) == 0 {
            e.next += 1;
            continue;
        }
        let info = p.basic(it);
        let hr = (api.fill)(it.name_w.as_ptr(), &info, handle);
        if hr != S_OK {
            if added == 0 {
                return hr; // buffer too small for even one entry
            }
            break; // buffer full - continue on the next call
        }
        added += 1;
        e.next += 1;
    }
    S_OK
}

unsafe extern "system" fn cb_placeholder(d: *const CallbackData) -> i32 {
    let api = match API.get() {
        Some(a) => a,
        None => return E_FAIL,
    };
    let p = provider(d);
    let it = match p.lookup(&wstr((*d).file_path)) {
        Some(it) => it,
        None => return E_FILE_NOT_FOUND,
    };
    let mut info: PlaceholderInfo = std::mem::zeroed();
    info.basic = p.basic(it);
    (api.write_ph)((*d).ctx, it.path_w.as_ptr(), &info, size_of::<PlaceholderInfo>() as u32)
}

unsafe extern "system" fn cb_file_data(d: *const CallbackData, offset: u64, len: u32) -> i32 {
    let api = match API.get() {
        Some(a) => a,
        None => return E_FAIL,
    };
    let p = provider(d);
    let path = wstr((*d).file_path);
    let it = match p.lookup(&path) {
        Some(it) => it,
        None => return E_FILE_NOT_FOUND,
    };
    if it.is_dir {
        return E_INVALIDARG;
    }
    if len == 0 {
        return S_OK;
    }
    let ctx = (*d).ctx;
    let sid = (*d).stream_id;
    let chunk = (len as usize).min(CHUNK);
    let buf = (api.alloc)(ctx, chunk) as *mut u8;
    if buf.is_null() {
        return E_OUTOFMEMORY;
    }
    let mut fill = 0usize;
    let mut pos = offset;
    let result = {
        let mut sink = |data: &[u8]| -> Result<(), String> {
            let mut s = data;
            while !s.is_empty() {
                let n = (chunk - fill).min(s.len());
                ptr::copy_nonoverlapping(s.as_ptr(), buf.add(fill), n);
                fill += n;
                s = &s[n..];
                if fill == chunk {
                    let hr = (api.write_data)(ctx, &sid, buf as *mut c_void, pos, fill as u32);
                    if hr != S_OK {
                        return Err(format!("PrjWriteFileData failed (0x{:08X})", hr as u32));
                    }
                    pos += fill as u64;
                    fill = 0;
                }
            }
            Ok(())
        };
        archive::stream(&p.archive, p.kind, &it.orig, it.idx, offset, len as u64, &mut sink)
    };
    let result = result.and_then(|_| {
        if fill > 0 {
            let hr = (api.write_data)(ctx, &sid, buf as *mut c_void, pos, fill as u32);
            if hr != S_OK {
                return Err(format!("PrjWriteFileData failed (0x{:08X})", hr as u32));
            }
        }
        Ok(())
    });
    (api.free)(buf as *mut c_void);
    match result {
        Ok(()) => S_OK,
        Err(e) => {
            warn!("archive read failed for {}: {}", path, e);
            E_FAIL
        }
    }
}

// ------------------------------------------------------------------ running instance

struct Handle(*mut c_void);
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}

pub struct Running {
    handle: Handle,
    ctx: Handle,
}

fn instance_guid(root: &Path) -> Guid {
    let n = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) as u64;
    let h = crate::disk::fnv1a(root.to_string_lossy().as_bytes());
    let mut d4 = [0u8; 8];
    d4.copy_from_slice(&h.to_le_bytes());
    d4[0] = (d4[0] & 0x3f) | 0x80;
    Guid { d1: n as u32, d2: (n >> 32) as u16, d3: (((n >> 48) as u16) & 0x0fff) | 0x4000, d4 }
}

/// Start projecting `prov` into `root`. `mark` = the folder is new and must be
/// marked as a virtualization root first.
pub fn start(root: &Path, prov: Arc<Provider>, mark: bool) -> Result<Running, String> {
    let api = load()?;
    let root_w = wide_path(root);
    unsafe {
        if mark {
            let g = instance_guid(root);
            let hr = (api.mark)(root_w.as_ptr(), ptr::null(), ptr::null(), &g);
            if hr != S_OK {
                return Err(format!("could not prepare the projection folder (0x{:08X})", hr as u32));
            }
        }
        let ctx = Arc::into_raw(prov) as *const c_void;
        let mut handle: *mut c_void = ptr::null_mut();
        let hr = (api.start)(root_w.as_ptr(), &CALLBACKS, ctx, ptr::null(), &mut handle);
        if hr != S_OK {
            drop(Arc::from_raw(ctx as *const Provider));
            return Err(format!(
                "Windows Projected File System could not start (0x{:08X}) - if ProjFS was just enabled, restart Windows",
                hr as u32
            ));
        }
        Ok(Running { handle: Handle(handle), ctx: Handle(ctx as *mut c_void) })
    }
}

impl Running {
    pub fn stop(self) {
        if let Some(api) = API.get() {
            unsafe {
                (api.stop)(self.handle.0);
                drop(Arc::from_raw(self.ctx.0 as *const Provider));
            }
        }
    }
}

/// Delete a projection folder. If ProjFS refuses (provider offline), restart an
/// empty provider on it so the placeholders can be removed, then delete again.
pub fn purge(root: &Path) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }
    if std::fs::remove_dir_all(root).is_ok() {
        return Ok(());
    }
    if let Ok(run) = start(root, Arc::new(Provider::empty()), false) {
        if let Ok(rd) = std::fs::read_dir(root) {
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    let _ = std::fs::remove_dir_all(&p);
                } else {
                    let _ = std::fs::remove_file(&p);
                }
            }
        }
        run.stop();
    }
    std::fs::remove_dir_all(root).map_err(|e| format!("could not delete {}: {}", root.display(), e))
}
