//! VirtualBox VDI reader (format version 1.1 - every VDI made by VirtualBox 2.x+).
//! Dynamic and fixed images. Differencing (snapshot) images need their parent
//! and are rejected with a clear message.

use std::io;

use crate::backend::{le32, le64, Backend, Src};

const SIGNATURE: u32 = 0xBEDA_107F;
const UNALLOCATED: u32 = 0xFFFF_FFFF;
const ZERO_BLOCK: u32 = 0xFFFF_FFFE;

pub struct Vdi {
    src: Src,
    map: Vec<u32>,
    block: u64,
    extra: u64,
    data_off: u64,
    size: u64,
    kind: u32,
}

pub fn is_vdi(head: &[u8]) -> bool {
    head.len() >= 0x48 && le32(head, 0x40) == SIGNATURE
}

pub fn open(src: Src) -> Result<Vdi, String> {
    let h = src.read_vec(0, 0x200).map_err(|e| e.to_string())?;
    if !is_vdi(&h) {
        return Err("not a VirtualBox VDI file (signature missing)".into());
    }
    let ver = le32(&h, 0x44);
    if ver >> 16 != 1 {
        return Err(format!("unsupported VDI format version {}.{}", ver >> 16, ver & 0xffff));
    }
    let kind = le32(&h, 0x4C);
    if kind == 4 {
        return Err("this VDI is a differencing (snapshot) image - it needs its parent; mount the base VDI instead".into());
    }
    let off_blocks = le32(&h, 0x154) as u64;
    let data_off = le32(&h, 0x158) as u64;
    let size = le64(&h, 0x170);
    let block = le32(&h, 0x178) as u64;
    let extra = le32(&h, 0x17C) as u64;
    let n = le32(&h, 0x180) as u64;
    if block == 0 || n == 0 || size == 0 || n.saturating_mul(block) < size {
        return Err("VDI header is invalid".into());
    }
    if n > (1 << 26) {
        return Err("VDI block map is unreasonably large".into());
    }
    let raw = src.read_vec(off_blocks, (n * 4) as usize).map_err(|e| e.to_string())?;
    let map = raw.chunks_exact(4).map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    Ok(Vdi { src, map, block, extra, data_off, size, kind })
}

impl Vdi {
    pub fn describe(&self) -> String {
        let t = match self.kind {
            1 => "dynamic",
            2 => "fixed",
            3 => "undo",
            _ => "unknown type",
        };
        let used = self.map.iter().filter(|&&e| e < ZERO_BLOCK).count();
        format!(
            "VDI {} - {} blocks of {} KB, {} allocated",
            t,
            self.map.len(),
            self.block / 1024,
            used
        )
    }
}

impl Backend for Vdi {
    fn size(&self) -> u64 {
        self.size
    }

    fn read_at(&self, mut off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let bi = (off / self.block) as usize;
            let within = off % self.block;
            let n = ((self.block - within) as usize).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            match self.map.get(bi).copied().unwrap_or(UNALLOCATED) {
                UNALLOCATED | ZERO_BLOCK => out.fill(0),
                e => {
                    let pos = self.data_off + e as u64 * (self.block + self.extra) + self.extra + within;
                    self.src.read(pos, out)?;
                }
            }
            done += n;
            off += n as u64;
        }
        Ok(())
    }
}
