//! OVA = a TAR archive containing an .ovf descriptor and one or more VMDK disks.
//! The disk is read where it sits inside the TAR - nothing is extracted.

use crate::backend::Src;

pub struct Entry {
    pub name: String,
    pub off: u64,
    pub size: u64,
}

fn octal(b: &[u8]) -> u64 {
    if !b.is_empty() && b[0] & 0x80 != 0 {
        // GNU base-256 encoding for large sizes.
        let mut v = 0u64;
        for &x in &b[1..] {
            v = (v << 8) | x as u64;
        }
        return v;
    }
    let mut v = 0u64;
    let mut seen = false;
    for &x in b {
        if (b'0'..=b'7').contains(&x) {
            v = v * 8 + (x - b'0') as u64;
            seen = true;
        } else if seen {
            break;
        }
    }
    v
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).to_string()
}

pub fn list(src: &Src) -> Result<Vec<Entry>, String> {
    let mut out = Vec::new();
    let mut pos = 0u64;
    let mut long_name: Option<String> = None;
    let mut pax_size: Option<u64> = None;
    let mut pax_path: Option<String> = None;
    let mut guard = 0;
    while pos + 512 <= src.len && guard < 100_000 {
        guard += 1;
        let h = src.read_vec(pos, 512).map_err(|e| e.to_string())?;
        if h.iter().all(|&c| c == 0) {
            break;
        }
        let mut name = cstr(&h[0..100]);
        if &h[257..262] == b"ustar" {
            let pre = cstr(&h[345..500]);
            if !pre.is_empty() {
                name = format!("{}/{}", pre, name);
            }
        }
        let mut size = octal(&h[124..136]);
        let ty = h[156];
        let data = pos + 512;
        match ty {
            b'L' => {
                let d = src.read_vec(data, size.min(4096) as usize).map_err(|e| e.to_string())?;
                long_name = Some(cstr(&d));
            }
            b'x' => {
                let d = src.read_vec(data, size.min(65536) as usize).map_err(|e| e.to_string())?;
                let text = String::from_utf8_lossy(&d).to_string();
                for rec in text.split('\n') {
                    if let Some(i) = rec.find(' ') {
                        let kv = &rec[i + 1..];
                        if let Some(v) = kv.strip_prefix("size=") {
                            pax_size = v.trim().parse().ok();
                        } else if let Some(v) = kv.strip_prefix("path=") {
                            pax_path = Some(v.trim().to_string());
                        }
                    }
                }
            }
            b'0' | 0 | b'7' => {
                if let Some(l) = long_name.take() {
                    name = l;
                }
                if let Some(p) = pax_path.take() {
                    name = p;
                }
                if let Some(sz) = pax_size.take() {
                    size = sz;
                }
                out.push(Entry { name, off: data, size });
            }
            _ => {
                long_name = None;
                pax_path = None;
                pax_size = None;
            }
        }
        pos = data + (size + 511) / 512 * 512;
    }
    if out.is_empty() {
        return Err("this OVA is not a valid TAR archive (no files found)".into());
    }
    Ok(out)
}
