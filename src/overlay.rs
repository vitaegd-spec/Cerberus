//! Write overlay ("changes file") for container formats (VDI, VMDK, OVA, CHD).
//! The original image is never modified: every write lands in 64 KiB chunks in
//! data\overlays\<key>.dat, indexed by <key>.idx. On unmount the overlay is either
//! kept (re-applied automatically next time the same image is mounted) or deleted.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::backend::{le64, pread, pwrite, Backend};

const MAGIC: &[u8; 8] = b"CERBOVL1";
const HDR: u64 = 24; // magic + base size + base mtime
pub const CHUNK: u64 = 64 * 1024;

struct State {
    map: HashMap<u64, u64>,
    next: u64,
    idx_len: u64,
}

pub struct Overlay {
    data: Option<File>,
    idx: Option<File>,
    state: Mutex<State>,
    keep: AtomicBool,
    pub data_path: PathBuf,
    pub idx_path: PathBuf,
    /// Chunks that already existed when the overlay was opened (saved changes).
    pub saved_chunks: usize,
}

pub struct OpenResult {
    pub overlay: Option<Overlay>,
    /// A saved overlay existed but belonged to a different version of the image.
    pub discarded_stale: bool,
}

fn poisoned() -> io::Error {
    io::Error::new(io::ErrorKind::Other, "overlay lock poisoned")
}
fn closed() -> io::Error {
    io::Error::new(io::ErrorKind::Other, "overlay is closed")
}

impl Overlay {
    pub fn paths(dir: &Path, key: u64) -> (PathBuf, PathBuf) {
        (dir.join(format!("{:016x}.dat", key)), dir.join(format!("{:016x}.idx", key)))
    }

    pub fn open(dir: &Path, key: u64, base_len: u64, base_mtime: u64, writable: bool) -> io::Result<OpenResult> {
        fs::create_dir_all(dir)?;
        let (dp, ip) = Self::paths(dir, key);
        let mut stale = false;
        let mut existing: Option<(HashMap<u64, u64>, u64)> = None;
        if dp.exists() && ip.exists() {
            match load_index(&ip, base_len, base_mtime) {
                Some(m) => existing = Some(m),
                None => {
                    stale = true;
                    let _ = fs::remove_file(&dp);
                    let _ = fs::remove_file(&ip);
                }
            }
        }
        if existing.is_none() && !writable {
            return Ok(OpenResult { overlay: None, discarded_stale: stale });
        }
        let (map, idx_len, fresh) = match existing {
            Some((m, l)) => (m, l, false),
            None => {
                let mut h = Vec::with_capacity(HDR as usize);
                h.extend_from_slice(MAGIC);
                h.extend_from_slice(&base_len.to_le_bytes());
                h.extend_from_slice(&base_mtime.to_le_bytes());
                fs::write(&ip, &h)?;
                File::create(&dp)?;
                (HashMap::new(), HDR, true)
            }
        };
        let data = OpenOptions::new().read(true).write(writable).open(&dp)?;
        let idx = OpenOptions::new().read(true).write(writable).open(&ip)?;
        let len = data.metadata()?.len();
        let next = (len + CHUNK - 1) / CHUNK * CHUNK;
        let saved = if fresh { 0 } else { map.len() };
        Ok(OpenResult {
            overlay: Some(Overlay {
                data: Some(data),
                idx: Some(idx),
                state: Mutex::new(State { map, next, idx_len }),
                keep: AtomicBool::new(true),
                data_path: dp,
                idx_path: ip,
                saved_chunks: saved,
            }),
            discarded_stale: stale,
        })
    }

    pub fn chunks(&self) -> usize {
        self.state.lock().map(|s| s.map.len()).unwrap_or(0)
    }

    pub fn set_keep(&self, keep: bool) {
        self.keep.store(keep, Ordering::SeqCst);
    }

    pub fn read(&self, base: &dyn Backend, mut off: u64, buf: &mut [u8]) -> io::Result<()> {
        let st = self.state.lock().map_err(|_| poisoned())?;
        let data = self.data.as_ref().ok_or_else(closed)?;
        let mut done = 0usize;
        while done < buf.len() {
            let c = off / CHUNK;
            let within = off % CHUNK;
            let n = ((CHUNK - within) as usize).min(buf.len() - done);
            match st.map.get(&c) {
                Some(&pos) => pread(data, &mut buf[done..done + n], pos + within)?,
                None => base.read_at(off, &mut buf[done..done + n])?,
            }
            done += n;
            off += n as u64;
        }
        Ok(())
    }

    pub fn write(&self, base: &dyn Backend, mut off: u64, input: &[u8]) -> io::Result<()> {
        let mut st = self.state.lock().map_err(|_| poisoned())?;
        let data = self.data.as_ref().ok_or_else(closed)?;
        let idx = self.idx.as_ref().ok_or_else(closed)?;
        let size = base.size();
        let mut done = 0usize;
        while done < input.len() {
            let c = off / CHUNK;
            let within = off % CHUNK;
            let n = ((CHUNK - within) as usize).min(input.len() - done);
            let existing = st.map.get(&c).copied();
            let pos = match existing {
                Some(p) => p,
                None => {
                    // Copy-on-write: start the chunk from the original image contents.
                    let cstart = c * CHUNK;
                    let mut chunk = vec![0u8; CHUNK as usize];
                    if cstart < size {
                        let l = (size - cstart).min(CHUNK) as usize;
                        base.read_at(cstart, &mut chunk[..l])?;
                    }
                    let p = st.next;
                    pwrite(data, &chunk, p)?;
                    st.next += CHUNK;
                    let mut e = [0u8; 16];
                    e[0..8].copy_from_slice(&c.to_le_bytes());
                    e[8..16].copy_from_slice(&p.to_le_bytes());
                    let il = st.idx_len;
                    pwrite(idx, &e, il)?; // index entry only after the data is on disk
                    st.idx_len += 16;
                    st.map.insert(c, p);
                    p
                }
            };
            pwrite(data, &input[done..done + n], pos + within)?;
            done += n;
            off += n as u64;
        }
        Ok(())
    }

    pub fn flush(&self) -> io::Result<()> {
        if let Some(d) = &self.data {
            d.sync_data()?;
        }
        if let Some(i) = &self.idx {
            i.sync_data()?;
        }
        Ok(())
    }
}

fn load_index(ip: &Path, base_len: u64, base_mtime: u64) -> Option<(HashMap<u64, u64>, u64)> {
    let b = fs::read(ip).ok()?;
    if b.len() < HDR as usize || &b[0..8] != MAGIC {
        return None;
    }
    if le64(&b, 8) != base_len || le64(&b, 16) != base_mtime {
        return None;
    }
    let n = (b.len() - HDR as usize) / 16;
    let mut map = HashMap::with_capacity(n);
    for i in 0..n {
        let o = HDR as usize + i * 16;
        map.insert(le64(&b, o), le64(&b, o + 8));
    }
    Some((map, HDR + n as u64 * 16))
}

impl Drop for Overlay {
    fn drop(&mut self) {
        let keep = self.keep.load(Ordering::SeqCst);
        if let Some(f) = self.data.take() {
            if keep {
                let _ = f.sync_all();
            }
            drop(f);
        }
        if let Some(f) = self.idx.take() {
            if keep {
                let _ = f.sync_all();
            }
            drop(f);
        }
        if !keep {
            let _ = fs::remove_file(&self.data_path);
            let _ = fs::remove_file(&self.idx_path);
        }
    }
}
