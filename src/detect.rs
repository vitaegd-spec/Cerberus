//! Content-based detection of what a raw .img actually contains.
//! (The extension is never trusted.)

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Blank,
    Mbr,
    Gpt,
    Floppy,
    Volume,
    Iso9660,
    /// Raw CD image (2352-byte sectors starting with the CD sync pattern).
    CdRaw,
    Unknown,
}

pub struct Detection {
    pub kind: Kind,
    pub description: String,
}

const FLOPPY_SIZES: [u64; 8] = [163_840, 184_320, 327_680, 368_640, 737_280, 1_228_800, 1_474_560, 2_949_120];
const CD_SYNC: [u8; 12] = [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0];

pub fn detect(path: &Path) -> io::Result<Detection> {
    let mut f = File::open(path)?;
    let size = f.metadata()?.len();
    let mut head = vec![0u8; (size.min(64 * 1024)) as usize];
    f.read_exact(&mut head)?;

    let d = |kind: Kind, s: String| Detection { kind, description: s };

    if head.iter().all(|b| *b == 0) {
        return Ok(d(Kind::Blank, "blank / uninitialised (all zeros)".into()));
    }
    if head.len() >= 16 && head[..12] == CD_SYNC {
        return Ok(d(Kind::CdRaw, "raw CD image (2352-byte sectors)".into()));
    }
    // ISO9660 primary volume descriptor at 2048-byte sector 16.
    if head.len() >= 32768 + 6 && &head[32769..32774] == b"CD001" {
        return Ok(d(Kind::Iso9660, "ISO9660 optical image".into()));
    }
    if head.len() >= 512 {
        let boot_jump = head[0] == 0xEB || head[0] == 0xE9;
        let bps = u16::from_le_bytes([head[11], head[12]]);
        if &head[3..11] == b"NTFS    " {
            return Ok(d(Kind::Volume, "NTFS volume with no partition table (superfloppy layout)".into()));
        }
        let fat_tag = if &head[54..59] == b"FAT12" {
            Some("FAT12")
        } else if &head[54..59] == b"FAT16" {
            Some("FAT16")
        } else if &head[82..87] == b"FAT32" {
            Some("FAT32")
        } else if &head[3..11] == b"EXFAT   " {
            Some("exFAT")
        } else {
            None
        };
        if boot_jump && bps == 512 {
            if let Some(fs) = fat_tag {
                if FLOPPY_SIZES.contains(&size) {
                    return Ok(d(Kind::Floppy, format!("floppy disk image ({}, {} KB, no partition table)", fs, size / 1024)));
                }
                return Ok(d(Kind::Volume, format!("{} volume with no partition table (superfloppy layout)", fs)));
            }
        }
        if head[510] == 0x55 && head[511] == 0xAA {
            if head.len() >= 520 && &head[512..520] == b"EFI PART" {
                return Ok(d(Kind::Gpt, "GPT partitioned hard disk image".into()));
            }
            let mut parts = 0;
            for i in 0..4 {
                let t = head[0x1BE + i * 16 + 4];
                if t == 0xEE {
                    return Ok(d(Kind::Gpt, "GPT partitioned hard disk image (protective MBR)".into()));
                }
                if t != 0 {
                    parts += 1;
                }
            }
            if parts > 0 {
                return Ok(d(Kind::Mbr, format!("MBR partitioned hard disk image ({} partition(s))", parts)));
            }
        }
    }
    if FLOPPY_SIZES.contains(&size) {
        return Ok(d(Kind::Floppy, format!("floppy disk image (by size: {} KB)", size / 1024)));
    }
    Ok(d(Kind::Unknown, "unrecognised raw data - served as-is".into()))
}
