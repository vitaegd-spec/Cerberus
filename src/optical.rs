//! CD/DVD image readers. Each produces a `Track`: the first data track of the
//! disc, exposed as plain 2048-byte sectors (what Windows' CD-ROM driver reads).
//!   BIN/CUE, MDS/MDF, CCD/IMG/SUB, CDI, NRG
//! Where a format's own table can't be used, CERBERUS locates the data track by
//! finding the ISO9660 / UDF volume descriptor and the CD sync pattern.

use std::io;
use std::path::{Path, PathBuf};

use crate::backend::{be16, be32, be64, Backend, Src};

const SYNC: [u8; 12] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];

pub struct Track {
    pub src: Src,
    /// Byte offset of the track's first sector in the file.
    pub start: u64,
    /// Physical sector size in the file (2048, 2336, 2352 or 2448).
    pub ss: u64,
    /// Offset of the 2048 user-data bytes inside each physical sector.
    pub off: u64,
    pub sectors: u64,
    pub how: String,
}

impl Backend for Track {
    fn size(&self) -> u64 {
        self.sectors * 2048
    }

    fn read_at(&self, mut off: u64, buf: &mut [u8]) -> io::Result<()> {
        if self.ss == 2048 {
            return self.src.read(self.start + off, buf);
        }
        let mut done = 0usize;
        while done < buf.len() {
            let sct = off / 2048;
            let within = off % 2048;
            let n = ((2048 - within) as usize).min(buf.len() - done);
            self.src.read(self.start + sct * self.ss + self.off + within, &mut buf[done..done + n])?;
            done += n;
            off += n as u64;
        }
        Ok(())
    }
}

/// Where the 2048 data bytes sit inside a physical sector, if this looks valid.
fn mode_off(src: &Src, start: u64, ss: u64) -> Option<u64> {
    match ss {
        2048 => Some(0),
        2336 => Some(8),
        2352 | 2448 => {
            let h = src.read_vec(start, 16).ok()?;
            if h[..12] != SYNC {
                return None;
            }
            match h[15] {
                1 => Some(16),
                2 => Some(24),
                _ => None,
            }
        }
        _ => None,
    }
}

/// ISO9660 primary volume descriptor or UDF volume recognition at sector 16.
fn has_fs(src: &Src, start: u64, ss: u64, off: u64) -> bool {
    match src.read_vec(start + 16 * ss + off, 6) {
        Ok(b) => &b[1..6] == b"CD001" || &b[1..6] == b"BEA01",
        Err(_) => false,
    }
}

fn make(src: &Src, start: u64, ss: u64, off: u64, end: Option<u64>, how: &str) -> Track {
    let end = end.unwrap_or(src.len).min(src.len);
    Track {
        src: src.clone(),
        start,
        ss,
        off,
        sectors: end.saturating_sub(start) / ss,
        how: how.to_string(),
    }
}

fn try_at(src: &Src, start: u64, end: Option<u64>, how: &str) -> Option<Track> {
    for ss in [2352u64, 2448, 2048, 2336] {
        if let Some(off) = mode_off(src, start, ss) {
            if has_fs(src, start, ss, off) {
                return Some(make(src, start, ss, off, end, how));
            }
        }
    }
    None
}

/// Search the first 1 GB for a volume descriptor and work back to the track start.
pub fn scan(src: &Src, how: &str) -> Result<Track, String> {
    const WIN: u64 = 8 << 20;
    let limit = src.len.min(1 << 30);
    let mut pos = 0u64;
    while pos < limit {
        let n = (WIN + 8).min(src.len - pos) as usize;
        let buf = src.read_vec(pos, n).map_err(|e| e.to_string())?;
        let mut i = 0usize;
        while i + 6 <= buf.len() {
            let hit = (buf[i] == 1 && &buf[i + 1..i + 6] == b"CD001") || (buf[i] == 0 && &buf[i + 1..i + 6] == b"BEA01");
            if hit {
                let p = pos + i as u64;
                // Sync-checked layouts first; plain 2048 / 2336 last.
                for (ss, off) in [(2352u64, 16u64), (2352, 24), (2448, 16), (2448, 24), (2048, 0), (2336, 8)] {
                    let back = off + 16 * ss;
                    if p < back {
                        continue;
                    }
                    let start = p - back;
                    if mode_off(src, start, ss) == Some(off) {
                        return Ok(make(src, start, ss, off, None, how));
                    }
                }
            }
            i += 1;
        }
        pos += WIN;
    }
    Err("no ISO9660/UDF data track found in this image (audio-only CD or unsupported layout)".into())
}

/// Raw image of unknown layout (BIN without CUE, MDF, CloneCD IMG, ...).
pub fn guess(src: Src, how: &str) -> Result<Track, String> {
    match try_at(&src, 0, None, how) {
        Some(t) => Ok(t),
        None => scan(&src, how),
    }
}

// ------------------------------------------------------------ CUE

fn msf(s: &str) -> Option<u64> {
    let p: Vec<u64> = s.split(':').filter_map(|x| x.trim().parse().ok()).collect();
    if p.len() == 3 {
        Some((p[0] * 60 + p[1]) * 75 + p[2])
    } else {
        None
    }
}

struct CueTrack {
    file: Option<String>,
    mode: String,
    idx0: Option<u64>,
    idx1: Option<u64>,
}

fn resolve(cue: &Path, name: &str) -> Result<PathBuf, String> {
    let dir = cue.parent().unwrap_or_else(|| Path::new("."));
    let direct = dir.join(name);
    if direct.exists() {
        return Ok(direct);
    }
    if let Some(fname) = Path::new(name).file_name() {
        let p = dir.join(fname);
        if p.exists() {
            return Ok(p);
        }
    }
    for e in ["bin", "BIN", "img", "IMG"] {
        let p = cue.with_extension(e);
        if p.exists() {
            return Ok(p);
        }
    }
    Err(format!("the CUE sheet points to \"{}\", which was not found next to it", name))
}

pub fn from_cue(cue: &Path) -> Result<Track, String> {
    let bytes = std::fs::read(cue).map_err(|e| format!("cannot read {}: {}", cue.display(), e))?;
    let text = String::from_utf8_lossy(&bytes).to_string();
    let mut cur_file: Option<String> = None;
    let mut tracks: Vec<CueTrack> = Vec::new();
    for line in text.lines() {
        let l = line.trim();
        let up = l.to_uppercase();
        if up.starts_with("FILE ") {
            let rest = l[5..].trim_start();
            let name = if let Some(r) = rest.strip_prefix('"') {
                r.split('"').next().unwrap_or("").to_string()
            } else {
                rest.split_whitespace().next().unwrap_or("").to_string()
            };
            cur_file = Some(name);
        } else if up.starts_with("TRACK ") {
            let mode = up.split_whitespace().nth(2).unwrap_or("").to_string();
            tracks.push(CueTrack { file: cur_file.clone(), mode, idx0: None, idx1: None });
        } else if up.starts_with("INDEX ") {
            let parts: Vec<&str> = up.split_whitespace().collect();
            if parts.len() >= 3 {
                let f = msf(parts[2]);
                if let Some(t) = tracks.last_mut() {
                    match parts[1].trim_start_matches('0') {
                        "1" => t.idx1 = f,
                        "" => t.idx0 = f,
                        _ => {}
                    }
                }
            }
        }
    }
    let i = tracks
        .iter()
        .position(|t| t.mode != "AUDIO")
        .ok_or("the CUE sheet has no data track (audio CDs cannot be mounted as a drive)")?;
    let t = &tracks[i];
    let (ss, default_off) = match t.mode.as_str() {
        "MODE1/2048" | "MODE2/2048" => (2048u64, 0u64),
        "MODE1/2352" => (2352, 16),
        "MODE2/2352" | "CDI/2352" => (2352, 24),
        "MODE2/2336" | "CDI/2336" => (2336, 8),
        m => return Err(format!("CUE track mode {} is not supported", m)),
    };
    let fname = t.file.clone().ok_or("the CUE sheet has no FILE line")?;
    let bin = resolve(cue, &fname)?;
    let src = Src::open(&bin)?;
    let start = t.idx1.unwrap_or(0) * ss;
    let end = tracks
        .get(i + 1)
        .filter(|n| n.file == t.file)
        .and_then(|n| n.idx0.or(n.idx1))
        .map(|f| f * ss);
    let off = mode_off(&src, start, ss).unwrap_or(default_off);
    Ok(make(&src, start, ss, off, end, &format!("CUE track {} {}", i + 1, t.mode)))
}

// ------------------------------------------------------------ NRG

fn nrg_footer(src: &Src) -> Option<u64> {
    if src.len < 12 {
        return None;
    }
    let t = src.read_vec(src.len - 12, 12).ok()?;
    if &t[0..4] == b"NER5" {
        return Some(be64(&t, 4));
    }
    if &t[4..8] == b"NERO" {
        return Some(be32(&t, 8) as u64);
    }
    None
}

pub fn from_nrg(src: Src) -> Result<Track, String> {
    // (start, end, sector size or 0 = unknown)
    let mut cands: Vec<(u64, Option<u64>, u64)> = Vec::new();
    if let Some(mut off) = nrg_footer(&src) {
        for _ in 0..256 {
            if off + 8 > src.len {
                break;
            }
            let h = match src.read_vec(off, 8) {
                Ok(h) => h,
                Err(_) => break,
            };
            let size = be32(&h, 4) as u64;
            let data = off + 8;
            let id = [h[0], h[1], h[2], h[3]];
            match &id {
                b"DAOX" | b"DAOI" => {
                    let x = &id == b"DAOX";
                    let d = src.read_vec(data, size.min(1 << 20) as usize).unwrap_or_default();
                    if d.len() >= 22 {
                        let count = (d[21] as usize).saturating_sub(d[20] as usize) + 1;
                        let el = if x { 42 } else { 30 };
                        for k in 0..count {
                            let e = 22 + k * el;
                            if e + el > d.len() {
                                break;
                            }
                            let ss = be16(&d, e + 12) as u64;
                            let (idx1, end) = if x {
                                (be64(&d, e + 26), be64(&d, e + 34))
                            } else {
                                (be32(&d, e + 22) as u64, be32(&d, e + 26) as u64)
                            };
                            cands.push((idx1, Some(end), ss));
                        }
                    }
                }
                b"ETN2" | b"ETNF" => {
                    let x = &id == b"ETN2";
                    let d = src.read_vec(data, size.min(1 << 20) as usize).unwrap_or_default();
                    let el = if x { 32 } else { 20 };
                    let mut e = 0;
                    while e + el <= d.len() {
                        let (o, sz) = if x {
                            (be64(&d, e), be64(&d, e + 8))
                        } else {
                            (be32(&d, e) as u64, be32(&d, e + 4) as u64)
                        };
                        cands.push((o, Some(o + sz), 0));
                        e += el;
                    }
                }
                b"END!" => break,
                _ => {}
            }
            off = data + size;
        }
    }
    for (start, end, ss) in cands {
        if ss != 0 {
            if let Some(o) = mode_off(&src, start, ss) {
                if has_fs(&src, start, ss, o) {
                    return Ok(make(&src, start, ss, o, end, "NRG track table"));
                }
            }
        }
        if let Some(t) = try_at(&src, start, end, "NRG track table") {
            return Ok(t);
        }
    }
    scan(&src, "NRG (located by scan)")
}
