//! MAME CHD reader (via the pure-Rust `chd` crate, compiled into CERBERUS).
//!   * hard-disk CHDs  -> served as a disk (512-byte sectors)
//!   * CD CHDs         -> first data track served as a CD-ROM (2048-byte sectors)
//!   * DVD CHDs        -> served as a DVD-ROM
//! The CHD decoder runs on its own thread (it owns the file), so the reader
//! never has to be shared between threads.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, BufReader};
use std::path::Path;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::backend::Backend;
use crate::formats::Opened;

const SYNC: [u8; 12] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];

enum Req {
    Hunk(u32, Sender<Result<Vec<u8>, String>>),
}

struct ChdFile {
    tx: Mutex<Sender<Req>>,
    hunk_size: u64,
    unit: u64,
    logical: u64,
    hunks: u32,
    cache: Mutex<VecDeque<(u32, Arc<Vec<u8>>)>>,
}

impl ChdFile {
    fn open(path: &Path) -> Result<ChdFile, String> {
        let p = path.to_path_buf();
        let (tx, rx) = channel::<Req>();
        let (itx, irx) = channel::<Result<(u64, u64, u64, u32), String>>();
        thread::Builder::new()
            .name("chd-reader".into())
            .spawn(move || {
                let file = match File::open(&p) {
                    Ok(f) => f,
                    Err(e) => {
                        let _ = itx.send(Err(format!("cannot open CHD: {}", e)));
                        return;
                    }
                };
                let mut chd = match chd::Chd::open(BufReader::new(file), None) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = itx.send(Err(format!(
                            "cannot read CHD ({:?}) - CHDs that need a parent file are not supported",
                            e
                        )));
                        return;
                    }
                };
                let info = {
                    let h = chd.header();
                    (h.hunk_size() as u64, h.unit_bytes() as u64, h.logical_bytes() as u64, h.hunk_count() as u32)
                };
                let _ = itx.send(Ok(info));
                let mut cmp: Vec<u8> = Vec::new();
                while let Ok(req) = rx.recv() {
                    match req {
                        Req::Hunk(n, reply) => {
                            let mut out = chd.get_hunksized_buffer();
                            let r = match chd.hunk(n) {
                                Ok(mut h) => h.read_hunk_in(&mut cmp, &mut out).map(|_| ()).map_err(|e| format!("{:?}", e)),
                                Err(e) => Err(format!("{:?}", e)),
                            };
                            let _ = reply.send(r.map(|_| out));
                        }
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        let (hunk_size, unit, logical, hunks) =
            irx.recv().map_err(|_| "CHD reader thread failed to start".to_string())??;
        if hunk_size == 0 || logical == 0 {
            return Err("CHD header is invalid".into());
        }
        Ok(ChdFile { tx: Mutex::new(tx), hunk_size, unit, logical, hunks, cache: Mutex::new(VecDeque::new()) })
    }

    fn hunk(&self, n: u32) -> io::Result<Arc<Vec<u8>>> {
        if let Ok(c) = self.cache.lock() {
            if let Some((_, h)) = c.iter().find(|(k, _)| *k == n) {
                return Ok(h.clone());
            }
        }
        let (rtx, rrx) = channel();
        self.tx
            .lock()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "CHD lock poisoned"))?
            .send(Req::Hunk(n, rtx))
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "CHD reader stopped"))?;
        let v = rrx
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::Other, "CHD reader stopped"))?
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("CHD hunk {}: {}", n, e)))?;
        let a = Arc::new(v);
        if let Ok(mut c) = self.cache.lock() {
            if c.len() >= 16 {
                c.pop_front();
            }
            c.push_back((n, a.clone()));
        }
        Ok(a)
    }

    /// Read the CHD's logical byte stream.
    fn read_raw(&self, mut off: u64, buf: &mut [u8]) -> io::Result<()> {
        let mut done = 0usize;
        while done < buf.len() {
            let hn = off / self.hunk_size;
            let within = (off % self.hunk_size) as usize;
            let n = (self.hunk_size as usize - within).min(buf.len() - done);
            let out = &mut buf[done..done + n];
            if hn >= self.hunks as u64 {
                out.fill(0);
            } else {
                let h = self.hunk(hn as u32)?;
                let avail = h.len().saturating_sub(within).min(n);
                out[..avail].copy_from_slice(&h[within..within + avail]);
                out[avail..].fill(0);
            }
            done += n;
            off += n as u64;
        }
        Ok(())
    }
}

enum Layout {
    Linear,
    /// CD: each frame is `unit` bytes (2352 + 96 subcode); user data at `data_off`.
    Cd { data_off: u64, frames: u64 },
}

pub struct ChdImg {
    c: ChdFile,
    layout: Layout,
}

impl Backend for ChdImg {
    fn size(&self) -> u64 {
        match self.layout {
            Layout::Linear => self.c.logical,
            Layout::Cd { frames, .. } => frames * 2048,
        }
    }

    fn read_at(&self, mut off: u64, buf: &mut [u8]) -> io::Result<()> {
        match self.layout {
            Layout::Linear => self.c.read_raw(off, buf),
            Layout::Cd { data_off, .. } => {
                let mut done = 0usize;
                while done < buf.len() {
                    let s = off / 2048;
                    let within = off % 2048;
                    let n = ((2048 - within) as usize).min(buf.len() - done);
                    self.c.read_raw(s * self.c.unit + data_off + within, &mut buf[done..done + n])?;
                    done += n;
                    off += n as u64;
                }
                Ok(())
            }
        }
    }
}

pub fn open(path: &Path) -> Result<Opened, String> {
    let c = ChdFile::open(path)?;
    let unit = c.unit;
    if unit == 2448 || unit == 2352 {
        let mut f0 = [0u8; 16];
        c.read_raw(0, &mut f0).map_err(|e| e.to_string())?;
        let data_off = if f0[..12] == SYNC {
            if f0[15] == 2 { 24 } else { 16 }
        } else {
            0
        };
        let frames = c.logical / unit;
        let detail = format!("CD CHD - {} frames, data at +{} in each {}-byte frame", frames, data_off, unit);
        let img = ChdImg { c, layout: Layout::Cd { data_off, frames } };
        return Ok(Opened {
            backend: Box::new(img),
            block_size: 2048,
            optical: true,
            removable: true,
            container: false,
            kind: "CD image (CHD)".into(),
            detail,
        });
    }
    if unit == 2048 {
        let detail = format!("DVD CHD - {} MB", c.logical >> 20);
        let img = ChdImg { c, layout: Layout::Linear };
        return Ok(Opened {
            backend: Box::new(img),
            block_size: 2048,
            optical: true,
            removable: true,
            container: false,
            kind: "DVD image (CHD)".into(),
            detail,
        });
    }
    let detail = format!("hard-disk CHD - {} MB, {}-byte hunks", c.logical >> 20, c.hunk_size);
    let img = ChdImg { c, layout: Layout::Linear };
    Ok(Opened {
        backend: Box::new(img),
        block_size: 512,
        optical: false,
        removable: false,
        container: true,
        kind: "Hard-disk image (CHD)".into(),
        detail,
    })
}
