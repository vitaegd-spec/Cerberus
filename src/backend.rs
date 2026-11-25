//! Storage backends: anything that can serve (and optionally accept) bytes at an
//! offset. Every image format reader implements `Backend`; the iSCSI engine only
//! ever talks to a `Disk`, which wraps a backend plus an optional write overlay.

use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;
use std::sync::Arc;

pub trait Backend: Send + Sync {
    /// Logical size in bytes.
    fn size(&self) -> u64;
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()>;
    fn write_at(&self, _off: u64, _data: &[u8]) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "this image format cannot be written directly",
        ))
    }
    fn flush(&self) -> io::Result<()> {
        Ok(())
    }
}

// ------------------------------------------------------------ positional I/O

#[cfg(windows)]
fn read_once(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    f.seek_read(buf, off)
}
#[cfg(not(windows))]
fn read_once(f: &File, buf: &mut [u8], off: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    f.read_at(buf, off)
}
#[cfg(windows)]
fn write_once(f: &File, data: &[u8], off: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    f.seek_write(data, off)
}
#[cfg(not(windows))]
fn write_once(f: &File, data: &[u8], off: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    f.write_at(data, off)
}

/// Read exactly `buf.len()` bytes at `off` (thread-safe, no shared file pointer).
pub fn pread(f: &File, buf: &mut [u8], mut off: u64) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        match read_once(f, &mut buf[done..], off) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past end of file")),
            Ok(n) => {
                done += n;
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Write all of `data` at `off`.
pub fn pwrite(f: &File, data: &[u8], mut off: u64) -> io::Result<()> {
    let mut done = 0;
    while done < data.len() {
        match write_once(f, &data[done..], off) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0 bytes")),
            Ok(n) => {
                done += n;
                off += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ------------------------------------------------------------ byte helpers

pub fn le16(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}
pub fn le32(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub fn le64(b: &[u8], o: usize) -> u64 {
    (le32(b, o) as u64) | ((le32(b, o + 4) as u64) << 32)
}
pub fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([b[o], b[o + 1]])
}
pub fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}
pub fn be64(b: &[u8], o: usize) -> u64 {
    ((be32(b, o) as u64) << 32) | (be32(b, o + 4) as u64)
}

// ------------------------------------------------------------ Src: file window

/// A read-only window into a file (the whole file, or e.g. one member of an OVA).
#[derive(Clone)]
pub struct Src {
    pub f: Arc<File>,
    pub base: u64,
    pub len: u64,
}

impl Src {
    pub fn file(f: File) -> io::Result<Src> {
        let len = f.metadata()?.len();
        Ok(Src { f: Arc::new(f), base: 0, len })
    }

    pub fn open(path: &Path) -> Result<Src, String> {
        let f = File::open(path).map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
        Src::file(f).map_err(|e| e.to_string())
    }

    /// Sub-window; clamped to the data that actually exists.
    pub fn sub(&self, off: u64, len: u64) -> Src {
        let avail = self.len.saturating_sub(off);
        Src { f: self.f.clone(), base: self.base + off, len: len.min(avail) }
    }

    /// Read; anything beyond the end of the window reads as zeros.
    pub fn read(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        if off >= self.len {
            buf.fill(0);
            return Ok(());
        }
        let avail = (self.len - off).min(buf.len() as u64) as usize;
        pread(&self.f, &mut buf[..avail], self.base + off)?;
        buf[avail..].fill(0);
        Ok(())
    }

    pub fn read_vec(&self, off: u64, n: usize) -> io::Result<Vec<u8>> {
        let mut v = vec![0u8; n];
        self.read(off, &mut v)?;
        Ok(v)
    }
}

// ------------------------------------------------------------ Raw

/// The image file IS the disk (raw .img / .ima / .flp / .raw).
pub struct Raw {
    f: File,
    size: u64,
}

impl Raw {
    pub fn open(path: &Path, writable: bool) -> io::Result<Raw> {
        let f = OpenOptions::new().read(true).write(writable).open(path)?;
        let size = f.metadata()?.len();
        Ok(Raw { f, size })
    }
}

impl Backend for Raw {
    fn size(&self) -> u64 {
        self.size
    }
    fn read_at(&self, off: u64, buf: &mut [u8]) -> io::Result<()> {
        if off >= self.size {
            buf.fill(0);
            return Ok(());
        }
        let avail = (self.size - off).min(buf.len() as u64) as usize;
        pread(&self.f, &mut buf[..avail], off)?;
        buf[avail..].fill(0);
        Ok(())
    }
    fn write_at(&self, off: u64, data: &[u8]) -> io::Result<()> {
        pwrite(&self.f, data, off)
    }
    fn flush(&self) -> io::Result<()> {
        self.f.sync_data()
    }
}
