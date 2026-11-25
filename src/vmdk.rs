//! VMware VMDK reader.
//!   * monolithicSparse / streamOptimized (single file with KDMV header,
//!     zlib-compressed grains supported - this is the format inside OVAs)
//!   * descriptor files with FLAT / SPARSE / ZERO extents
//!     (monolithicFlat, twoGbMaxExtentSparse, twoGbMaxExtentFlat, ESX "VMFS")
//! Snapshot (child) disks need their parent and are rejected with a message.

use std::collections::HashMap;
use std::io::{self, Read};
use std::path::Path;
use std::sync::{Arc, Mutex};

use flate2::read::ZlibDecoder;

use crate::backend::{le16, le32, le64, Backend, Src};

const MAGIC: u32 = 0x564d_444b; // "KDMV"
const FLAG_COMPRESSED: u32 = 1 << 16;
const GD_AT_END: u64 = u64::MAX;

fn s(e: impl std::fmt::Display) -> String {
    e.to_string()
}

pub fn is_sparse(head: &[u8]) -> bool {
    head.len() >= 4 && le32(head, 0) == MAGIC
}

#[derive(Clone, Copy)]
struct Header {
    flags: u32,
    capacity: u64,
    grain: u64,
    desc_off: u64,
    desc_size: u64,
    gtes: u32,
    gd_off: u64,
    compress: u16,
}

fn parse_header(b: &[u8]) -> Result<Header, String> {
    if b.len() < 79 || le32(b, 0) != MAGIC {
        return Err("not a VMDK sparse extent (KDMV signature missing)".into());
    }
    Ok(Header {
        flags: le32(b, 8),
        capacity: le64(b, 12),
        grain: le64(b, 20),
        desc_off: le64(b, 28),
        desc_size: le64(b, 36),
        gtes: le32(b, 44),
        gd_off: le64(b, 56),
        compress: le16(b, 77),
    })
}

// ------------------------------------------------------------ sparse extent

pub struct Sparse {
    src: Src,
    grain_bytes: u64,
    gtes: u64,
    gd: Vec<u32>,
    compressed: bool,
    capacity: u64,
    gt_cache: Mutex<HashMap<u32, Arc<Vec<u32>>>>,
    grain_cache: Mutex<Option<(u64, Arc<Vec<u8>>)>>,
    pub descriptor: String,
}

impl Sparse {
    pub fn open(src: Src) -> Result<Sparse, String> {
        let first = src.read_vec(0, 512).map_err(s)?;
        let mut h = parse_header(&first)?;
        if h.gd_off == GD_AT_END {
            // streamOptimized: the real header (with the GD location) is the footer.
            if src.len < 1536 {
                return Err("stream-optimized VMDK is truncated (no footer)".into());
            }
            let foot = src.read_vec(src.len - 1024, 512).map_err(s)?;
            h = parse_header(&foot)
                .map_err(|_| "stream-optimized VMDK footer not found - the file may be incomplete".to_string())?;
        }
        if h.grain == 0 || h.gtes == 0 || h.capacity == 0 || h.gd_off == GD_AT_END {
            return Err("VMDK header is invalid".into());
        }
        let cov = h.grain * h.gtes as u64;
        let n_gd = (h.capacity + cov - 1) / cov;
        if n_gd > (1 << 24) {
            return Err("VMDK grain directory is unreasonably large".into());
        }
        let raw = src.read_vec(h.gd_off * 512, (n_gd * 4) as usize).map_err(s)?;
        let gd = raw.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
        let descriptor = if h.desc_off > 0 && h.desc_size > 0 && h.desc_size < 4096 {
            let d = src.read_vec(h.desc_off * 512, (h.desc_size * 512) as usize).unwrap_or_default();
            String::from_utf8_lossy(&d).trim_end_matches('\0').to_string()
        } else {
            String::new()
        };
        Ok(Sparse {
            src,
            grain_bytes: h.grain * 512,
            gtes: h.gtes as u64,
            gd,
            compressed: h.flags & FLAG_COMPRESSED != 0 && h.compress != 0,
            capacity: h.capacity * 512,
            gt_cache: Mutex::new(HashMap::new()),
            grain_cache: Mutex::new(None),
            descriptor,
        })
    }

    fn gt(&self, idx: usize) -> io::Result<Option<Arc<Vec<u32>>>> {
        let sec = match self.gd.get(idx) {
            Some(&sec) if sec != 0 => sec,
            _ => return Ok(None),
        };
        if let Ok(c) = self.gt_cache.lock() {
            if let Some(t) = c.get(&sec) {
                return Ok(Some(t.clone()));
            }
        }
        let raw = self.src.read_vec(sec as u64 * 512, (self.gtes * 4) as usize)?;
        let t: Arc<Vec<u32>> =
            Arc::new(raw.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect());
        if let Ok(mut c) = self.gt_cache.lock() {
            if c.len() > 4096 {
                c.clear();
            }
            c.insert(sec, t.clone());
        }
        Ok(Some(t))
    }

    fn compressed_grain(&self, sec: u64) -> io::Result<Arc<Vec<u8>>> {
        if let Ok(c) = self.grain_cache.lock() {
            if let Some((cs, g)) = c.as_ref() {
                if *cs == sec {
                    return Ok(g.clone());
                }
            }
        }
        // Grain marker: u64 LBA, u32 compressed size, then zlib data.
        let hdr = self.src.read_vec(sec * 512, 12)?;
        let size = le32(&hdr, 8) as usize;
        if size == 0 || size > self.grain_bytes as usize * 2 + 4096 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "corrupt compressed VMDK grain"));
        }
        let comp = self.src.read_vec(sec * 512 + 12, size)?;
        let mut out = Vec::with_capacity(self.grain_bytes as usize);
        ZlibDecoder::new(&comp[..]).read_to_end(&mut out)?;
        out.resize(self.grain_bytes as usize, 0);
        let g = Arc::new(out);
        if let Ok(mut c) = self.grain_cache.lock() {
            *c = Some((sec, g.clone()));
        }
        Ok(g)
    }
}

impl Backend for Sparse {
    fn size(&self) -> u64 {
        self.capacity
    }

    fn read_at(&self, mut off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let g = off / self.grain_bytes;
            let within = off % self.grain_bytes;
            let n = ((self.grain_bytes - within) as usize).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            let gte = match self.gt((g / self.gtes) as usize)? {
                Some(t) => t.get((g % self.gtes) as usize).copied().unwrap_or(0),
                None => 0,
            };
            if gte <= 1 {
                out.fill(0); // 0 = unallocated, 1 = explicitly zeroed
            } else if self.compressed {
                let gr = self.compressed_grain(gte as u64)?;
                let w = within as usize;
                out.copy_from_slice(&gr[w..w + n]);
            } else {
                self.src.read(gte as u64 * 512 + within, out)?;
            }
            done += n;
            off += n as u64;
        }
        Ok(())
    }
}

// ------------------------------------------------------------ whole disk

enum Extent {
    Sparse(Sparse),
    Flat(Src),
    Zero,
}

pub struct Vmdk {
    parts: Vec<(u64, u64, Extent)>,
    size: u64,
    pub create_type: String,
}

fn field(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix(key) {
            if let Some(v) = rest.trim_start().strip_prefix('=') {
                return Some(v.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

fn check_parent(text: &str) -> Result<(), String> {
    if let Some(p) = field(text, "parentCID") {
        if !p.eq_ignore_ascii_case("ffffffff") {
            return Err(
                "this VMDK is a snapshot (child) disk - it needs its parent; mount the base disk instead".into(),
            );
        }
    }
    Ok(())
}

fn from_sparse(src: Src) -> Result<Vmdk, String> {
    let sp = Sparse::open(src)?;
    check_parent(&sp.descriptor)?;
    let ct = field(&sp.descriptor, "createType").unwrap_or_else(|| {
        if sp.compressed { "streamOptimized".into() } else { "monolithicSparse".into() }
    });
    let size = sp.capacity;
    Ok(Vmdk { parts: vec![(0, size, Extent::Sparse(sp))], size, create_type: ct })
}

/// Open a .vmdk file from disk (sparse file or text descriptor).
pub fn open_path(path: &Path) -> Result<Vmdk, String> {
    let src = Src::open(path)?;
    let head = src.read_vec(0, 512).map_err(s)?;
    if is_sparse(&head) {
        return from_sparse(src);
    }
    if src.len > (1 << 20) {
        return Err("not a VMDK file (no KDMV header and too large to be a descriptor)".into());
    }
    let text = String::from_utf8_lossy(&src.read_vec(0, src.len as usize).map_err(s)?).to_string();
    if !text.to_lowercase().contains("createtype") {
        return Err("not a VMDK descriptor file".into());
    }
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    from_descriptor(&text, dir)
}

/// Open a VMDK that lives inside another file (e.g. a member of an OVA).
pub fn open_src(src: Src) -> Result<Vmdk, String> {
    let head = src.read_vec(0, 512).map_err(s)?;
    if is_sparse(&head) {
        from_sparse(src)
    } else {
        Err("the disk inside the OVA is not a sparse/stream-optimized VMDK".into())
    }
}

fn from_descriptor(text: &str, dir: &Path) -> Result<Vmdk, String> {
    check_parent(text)?;
    let mut parts = Vec::new();
    let mut pos = 0u64;
    for line in text.lines() {
        let l = line.trim();
        let mut it = l.splitn(2, char::is_whitespace);
        let access = it.next().unwrap_or("");
        if !matches!(access, "RW" | "RDONLY" | "NOACCESS") {
            continue;
        }
        let rest = it.next().unwrap_or("").trim();
        let mut it2 = rest.splitn(3, char::is_whitespace);
        let sectors: u64 = it2
            .next()
            .unwrap_or("")
            .parse()
            .map_err(|_| format!("bad VMDK extent line: {}", l))?;
        let ty = it2.next().unwrap_or("").to_uppercase();
        let tail = it2.next().unwrap_or("").trim();
        let (file, after): (String, &str) = match tail.strip_prefix('"') {
            Some(t) => match t.find('"') {
                Some(i) => (t[..i].to_string(), t[i + 1..].trim()),
                None => return Err(format!("bad VMDK extent line: {}", l)),
            },
            None => (String::new(), tail),
        };
        let offset: u64 = after.split_whitespace().next().and_then(|v| v.parse().ok()).unwrap_or(0);
        let len = sectors * 512;
        let ext = match ty.as_str() {
            "FLAT" | "VMFS" => {
                let p = dir.join(&file);
                let src = Src::open(&p).map_err(|e| format!("missing VMDK extent: {}", e))?;
                Extent::Flat(src.sub(offset * 512, len))
            }
            "SPARSE" => {
                let p = dir.join(&file);
                let src = Src::open(&p).map_err(|e| format!("missing VMDK extent: {}", e))?;
                Extent::Sparse(Sparse::open(src)?)
            }
            "ZERO" => Extent::Zero,
            other => return Err(format!("VMDK extent type {} is not supported", other)),
        };
        parts.push((pos, len, ext));
        pos += len;
    }
    if parts.is_empty() {
        return Err("VMDK descriptor lists no extents".into());
    }
    Ok(Vmdk { parts, size: pos, create_type: field(text, "createType").unwrap_or_default() })
}

impl Vmdk {
    pub fn describe(&self) -> String {
        format!("VMDK {} - {} extent(s)", self.create_type, self.parts.len())
    }
}

impl Backend for Vmdk {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, mut off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let found = self.parts.iter().find(|(st, l, _)| off >= *st && off < *st + *l);
            let (st, l, e) = match found {
                Some((st, l, e)) => (*st, *l, e),
                None => {
                    buf[done..].fill(0);
                    return Ok(());
                }
            };
            let within = off - st;
            let n = ((l - within) as usize).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            match e {
                Extent::Sparse(sp) => sp.read_at(within, out)?,
                Extent::Flat(src) => src.read(within, out)?,
                Extent::Zero => out.fill(0),
            }
            done += n;
            off += n as u64;
        }
        Ok(())
    }
}
