//! Minimal per-user registry access (HKEY_CURRENT_USER only - no admin rights
//! needed). Used for "Start with Windows", the Explorer right-click menu and
//! file associations.

use std::ffi::c_void;
use std::ptr;

type Hkey = *mut c_void;

#[link(name = "advapi32")]
extern "system" {
    fn RegCreateKeyExW(
        hkey: Hkey,
        sub: *const u16,
        reserved: u32,
        class: *const u16,
        options: u32,
        sam: u32,
        security: *const c_void,
        result: *mut Hkey,
        disposition: *mut u32,
    ) -> i32;
    fn RegOpenKeyExW(hkey: Hkey, sub: *const u16, options: u32, sam: u32, result: *mut Hkey) -> i32;
    fn RegSetValueExW(hkey: Hkey, name: *const u16, reserved: u32, ty: u32, data: *const u8, len: u32) -> i32;
    fn RegQueryValueExW(hkey: Hkey, name: *const u16, reserved: *mut u32, ty: *mut u32, data: *mut u8, len: *mut u32) -> i32;
    fn RegDeleteValueW(hkey: Hkey, name: *const u16) -> i32;
    fn RegDeleteTreeW(hkey: Hkey, sub: *const u16) -> i32;
    fn RegCloseKey(hkey: Hkey) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn SHChangeNotify(event: i32, flags: u32, item1: *const c_void, item2: *const c_void);
}

const KEY_READ: u32 = 0x20019;
const KEY_WRITE: u32 = 0x20006;
const REG_NONE: u32 = 0;
const REG_SZ: u32 = 1;
const ERROR_SUCCESS: i32 = 0;
const ERROR_FILE_NOT_FOUND: i32 = 2;
const SHCNE_ASSOCCHANGED: i32 = 0x0800_0000;

fn hkcu() -> Hkey {
    // ((HKEY)(ULONG_PTR)((LONG)0x80000001)) - sign-extended on 64-bit Windows.
    0x8000_0001u32 as i32 as isize as Hkey
}

fn w(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn name_ptr(name: Option<&str>, buf: &mut Vec<u16>) -> *const u16 {
    match name {
        Some(n) => {
            *buf = w(n);
            buf.as_ptr()
        }
        None => ptr::null(),
    }
}

struct Key(Hkey);
impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            RegCloseKey(self.0);
        }
    }
}

fn create(path: &str) -> Result<Key, String> {
    let p = w(path);
    let mut h: Hkey = ptr::null_mut();
    let rc = unsafe { RegCreateKeyExW(hkcu(), p.as_ptr(), 0, ptr::null(), 0, KEY_WRITE, ptr::null(), &mut h, ptr::null_mut()) };
    if rc != ERROR_SUCCESS {
        return Err(format!("cannot write registry key HKCU\\{} (error {})", path, rc));
    }
    Ok(Key(h))
}

fn open_read(path: &str) -> Option<Key> {
    let p = w(path);
    let mut h: Hkey = ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(hkcu(), p.as_ptr(), 0, KEY_READ, &mut h) };
    if rc == ERROR_SUCCESS {
        Some(Key(h))
    } else {
        None
    }
}

fn open_write(path: &str) -> Option<Key> {
    let p = w(path);
    let mut h: Hkey = ptr::null_mut();
    let rc = unsafe { RegOpenKeyExW(hkcu(), p.as_ptr(), 0, KEY_WRITE, &mut h) };
    if rc == ERROR_SUCCESS {
        Some(Key(h))
    } else {
        None
    }
}

/// Write a string value (`name` = None writes the key's default value).
pub fn set_str(path: &str, name: Option<&str>, value: &str) -> Result<(), String> {
    let k = create(path)?;
    let mut nb = Vec::new();
    let np = name_ptr(name, &mut nb);
    let data = w(value);
    let rc = unsafe { RegSetValueExW(k.0, np, 0, REG_SZ, data.as_ptr() as *const u8, (data.len() * 2) as u32) };
    if rc != ERROR_SUCCESS {
        return Err(format!("cannot write registry value in HKCU\\{} (error {})", path, rc));
    }
    Ok(())
}

/// Write an empty REG_NONE value (used for OpenWithProgids entries).
pub fn set_none(path: &str, name: &str) -> Result<(), String> {
    let k = create(path)?;
    let n = w(name);
    let rc = unsafe { RegSetValueExW(k.0, n.as_ptr(), 0, REG_NONE, ptr::null(), 0) };
    if rc != ERROR_SUCCESS {
        return Err(format!("cannot write registry value in HKCU\\{} (error {})", path, rc));
    }
    Ok(())
}

/// Read a string value; None if the key or value does not exist.
pub fn get_str(path: &str, name: Option<&str>) -> Option<String> {
    let k = open_read(path)?;
    let mut nb = Vec::new();
    let np = name_ptr(name, &mut nb);
    let mut ty = 0u32;
    let mut len = 0u32;
    let rc = unsafe { RegQueryValueExW(k.0, np, ptr::null_mut(), &mut ty, ptr::null_mut(), &mut len) };
    if rc != ERROR_SUCCESS || ty != REG_SZ {
        return None;
    }
    let mut buf = vec![0u16; (len as usize / 2) + 1];
    let mut len2 = (buf.len() * 2) as u32;
    let rc = unsafe { RegQueryValueExW(k.0, np, ptr::null_mut(), &mut ty, buf.as_mut_ptr() as *mut u8, &mut len2) };
    if rc != ERROR_SUCCESS {
        return None;
    }
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..end]))
}

/// Delete a value (missing values are fine).
pub fn delete_value(path: &str, name: Option<&str>) -> Result<(), String> {
    let k = match open_write(path) {
        Some(k) => k,
        None => return Ok(()),
    };
    let mut nb = Vec::new();
    let np = name_ptr(name, &mut nb);
    let rc = unsafe { RegDeleteValueW(k.0, np) };
    if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
        return Err(format!("cannot delete registry value in HKCU\\{} (error {})", path, rc));
    }
    Ok(())
}

/// Delete a key and everything below it (missing keys are fine).
pub fn delete_tree(path: &str) -> Result<(), String> {
    let p = w(path);
    let rc = unsafe { RegDeleteTreeW(hkcu(), p.as_ptr()) };
    if rc != ERROR_SUCCESS && rc != ERROR_FILE_NOT_FOUND {
        return Err(format!("cannot delete registry key HKCU\\{} (error {})", path, rc));
    }
    // RegDeleteTreeW removes the contents; remove the (now empty) key itself.
    if let Some((parent, leaf)) = path.rsplit_once('\\') {
        if let Some(k) = open_write(parent) {
            let l = w(leaf);
            unsafe {
                RegDeleteTreeW(k.0, l.as_ptr());
            }
        }
    }
    Ok(())
}

/// Tell Explorer that file associations / context menus changed.
pub fn notify_assoc() {
    unsafe {
        SHChangeNotify(SHCNE_ASSOCCHANGED, 0, ptr::null(), ptr::null());
    }
}
