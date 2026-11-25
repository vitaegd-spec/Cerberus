//! A disk as seen by the iSCSI engine: a format backend + optional write overlay.

use std::io;
use std::sync::atomic::AtomicBool;

use crate::backend::Backend;
use crate::overlay::Overlay;

pub struct Disk {
    backend: Box<dyn Backend>,
    overlay: Option<Overlay>,
    pub block_size: u32,
    pub blocks: u64,
    pub readonly: bool,
    pub removable: bool,
    /// CD/DVD device (MMC command set, 2048-byte sectors).
    pub optical: bool,
    pub serial: String,
    pub id: u64,
    pub tail_bytes: u64,
    /// One-shot "new media" event for GET EVENT STATUS NOTIFICATION.
    pub media_event: AtomicBool,
}

impl Disk {
    pub fn new(
        backend: Box<dyn Backend>,
        block_size: u32,
        readonly: bool,
        removable: bool,
        optical: bool,
        key: &str,
        overlay: Option<Overlay>,
    ) -> io::Result<Disk> {
        let size = backend.size();
        let blocks = size / block_size as u64;
        if blocks == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "image is smaller than one sector"));
        }
        let id = fnv1a(key.to_lowercase().as_bytes());
        Ok(Disk {
            backend,
            overlay,
            block_size,
            blocks,
            readonly,
            removable,
            optical,
            serial: format!("{:016X}", id),
            id,
            tail_bytes: size % block_size as u64,
            media_event: AtomicBool::new(true),
        })
    }

    pub fn size_bytes(&self) -> u64 {
        self.blocks * self.block_size as u64
    }

    pub fn has_overlay(&self) -> bool {
        self.overlay.is_some()
    }

    pub fn overlay_bytes(&self) -> u64 {
        self.overlay.as_ref().map(|o| o.chunks() as u64 * crate::overlay::CHUNK).unwrap_or(0)
    }

    /// Decide what happens to the overlay file when this disk is finally dropped.
    pub fn set_keep_changes(&self, keep: bool) {
        if let Some(o) = &self.overlay {
            o.set_keep(keep);
        }
    }

    pub fn read(&self, lba: u64, count: u64) -> io::Result<Vec<u8>> {
        let bs = self.block_size as u64;
        let mut buf = vec![0u8; (count * bs) as usize];
        let off = lba * bs;
        match &self.overlay {
            Some(o) => o.read(&*self.backend, off, &mut buf)?,
            None => self.backend.read_at(off, &mut buf)?,
        }
        Ok(buf)
    }

    pub fn write(&self, lba: u64, data: &[u8]) -> io::Result<()> {
        if self.readonly {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "read-only"));
        }
        let off = lba * self.block_size as u64;
        match &self.overlay {
            Some(o) => o.write(&*self.backend, off, data),
            None => self.backend.write_at(off, data),
        }
    }

    pub fn flush(&self) -> io::Result<()> {
        if self.readonly {
            return Ok(());
        }
        match &self.overlay {
            Some(o) => o.flush(),
            None => self.backend.flush(),
        }
    }
}

pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}
