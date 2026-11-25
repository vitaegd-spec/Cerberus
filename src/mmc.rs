//! MMC (CD/DVD-ROM) command set for optical images. The device is a read-only
//! CD-ROM (or DVD-ROM for images larger than a CD) with a disc always present.

use std::sync::atomic::Ordering;

use crate::disk::Disk;
use crate::scsi::{rw_params, sense_fixed, ScsiResult};

const ILLEGAL: u8 = 0x05;
const VENDOR: &[u8; 8] = b"CERBERUS";
const PRODUCT: &[u8; 16] = b"Virtual CD/DVD  ";
const REVISION: &[u8; 4] = b"0.3 ";
const MAX_BLOCKS: u64 = 16384;

fn b16(b: &[u8]) -> u64 {
    ((b[0] as u64) << 8) | b[1] as u64
}
fn b32(b: &[u8]) -> u64 {
    ((b[0] as u64) << 24) | ((b[1] as u64) << 16) | ((b[2] as u64) << 8) | b[3] as u64
}
fn trunc(mut v: Vec<u8>, alloc: u64) -> Vec<u8> {
    if v.len() as u64 > alloc {
        v.truncate(alloc as usize);
    }
    v
}
fn good(d: Vec<u8>) -> ScsiResult {
    ScsiResult::good(d)
}
fn bad(key: u8, asc: u8, ascq: u8) -> ScsiResult {
    ScsiResult::check(key, asc, ascq)
}

fn msf(lba: u64) -> [u8; 4] {
    let l = lba + 150;
    [0, (l / 4500) as u8, ((l / 75) % 60) as u8, (l % 75) as u8]
}
fn addr(lba: u64, use_msf: bool) -> [u8; 4] {
    if use_msf {
        msf(lba)
    } else {
        (lba as u32).to_be_bytes()
    }
}

pub fn execute(disk: &Disk, lun: u16, cdb: &[u8; 16]) -> ScsiResult {
    let op = cdb[0];
    if lun != 0 {
        match op {
            0x12 => {
                let mut d = vec![0u8; 36];
                d[0] = 0x7F;
                d[3] = 0x02;
                d[4] = 31;
                return good(trunc(d, b16(&cdb[3..5])));
            }
            0xA0 | 0x03 => {}
            _ => return bad(ILLEGAL, 0x25, 0x00),
        }
    }
    let blocks = disk.blocks;
    let dvd = blocks > 900 * 512; // > ~900 MB -> present as DVD-ROM
    match op {
        // no-ops: TEST UNIT READY, START STOP, PREVENT/ALLOW, SET CD SPEED,
        // SET STREAMING, SYNC CACHE, SEEK, MODE SELECT, VERIFY
        0x00 | 0x1B | 0x1E | 0xBB | 0xB6 | 0x35 | 0x2B | 0x15 | 0x55 | 0x2F | 0x8F => good(Vec::new()),
        0x03 => good(trunc(sense_fixed(0, 0, 0), cdb[4] as u64)),
        0x12 => inquiry(disk, cdb),
        0x25 => {
            let mut d = Vec::with_capacity(8);
            d.extend_from_slice(&((blocks - 1) as u32).to_be_bytes());
            d.extend_from_slice(&2048u32.to_be_bytes());
            good(d)
        }
        0x08 | 0x28 | 0xA8 | 0x88 => {
            let (lba, n) = rw_params(cdb).unwrap_or((0, 0));
            read(disk, lba, n)
        }
        0xBE => {
            // READ CD: only "user data" requests are supported.
            let lba = b32(&cdb[2..6]);
            let n = ((cdb[6] as u64) << 16) | ((cdb[7] as u64) << 8) | cdb[8] as u64;
            let flags = cdb[9];
            if flags == 0 {
                good(Vec::new())
            } else if flags & 0xF8 == 0x10 {
                read(disk, lba, n)
            } else {
                bad(ILLEGAL, 0x24, 0x00)
            }
        }
        0x43 => read_toc(blocks, cdb),
        0x46 => get_configuration(dvd, cdb),
        0x4A => {
            if cdb[1] & 1 == 0 {
                return bad(ILLEGAL, 0x24, 0x00); // asynchronous notification not supported
            }
            let alloc = b16(&cdb[7..9]);
            let d = if cdb[4] & 0x10 != 0 {
                let code = if disk.media_event.swap(false, Ordering::SeqCst) { 0x02 } else { 0x00 };
                vec![0, 6, 0x04, 0x10, code, 0x02, 0, 0]
            } else {
                vec![0, 2, 0x80, 0x10]
            };
            good(trunc(d, alloc))
        }
        0x51 => {
            let mut d = vec![0u8; 34];
            d[0..2].copy_from_slice(&32u16.to_be_bytes());
            d[2] = 0x0E; // complete disc, complete last session
            d[3] = 1;
            d[4] = 1;
            d[5] = 1;
            d[6] = 1;
            d[7] = 0x20;
            for b in &mut d[16..24] {
                *b = 0xFF;
            }
            good(trunc(d, b16(&cdb[7..9])))
        }
        0x52 => {
            let mut d = vec![0u8; 36];
            d[0..2].copy_from_slice(&34u16.to_be_bytes());
            d[2] = 1;
            d[3] = 1;
            d[5] = 0x04; // data track
            d[6] = 0x01; // mode 1
            d[24..28].copy_from_slice(&(blocks as u32).to_be_bytes());
            good(trunc(d, b16(&cdb[7..9])))
        }
        0x1A | 0x5A => mode_sense(cdb),
        0xAD => dvd_structure(dvd, blocks, cdb),
        0xBD => good(trunc(vec![0u8; 8], b16(&cdb[8..10]))),
        0x42 => good(trunc(vec![0, 0x15, 0, 0], b16(&cdb[7..9]))),
        0xA0 => {
            let mut d = vec![0u8; 16];
            d[3] = 8;
            good(trunc(d, b32(&cdb[6..10])))
        }
        0x0A | 0x2A | 0xAA | 0x8A => bad(0x07, 0x27, 0x00),
        _ => bad(ILLEGAL, 0x20, 0x00),
    }
}

fn read(disk: &Disk, lba: u64, n: u64) -> ScsiResult {
    if lba.checked_add(n).map_or(true, |e| e > disk.blocks) {
        return bad(ILLEGAL, 0x21, 0x00);
    }
    if n > MAX_BLOCKS {
        return bad(ILLEGAL, 0x24, 0x00);
    }
    if n == 0 {
        return good(Vec::new());
    }
    match disk.read(lba, n) {
        Ok(d) => good(d),
        Err(e) => {
            warn!("optical read error lba={} n={}: {}", lba, n, e);
            bad(0x03, 0x11, 0x00)
        }
    }
}

fn inquiry(disk: &Disk, cdb: &[u8; 16]) -> ScsiResult {
    let evpd = cdb[1] & 1 != 0;
    let page = cdb[2];
    let alloc = b16(&cdb[3..5]);
    if !evpd {
        if page != 0 {
            return bad(ILLEGAL, 0x24, 0x00);
        }
        let mut d = vec![0u8; 36];
        d[0] = 0x05; // CD/DVD device
        d[1] = 0x80; // removable
        d[2] = 0x05;
        d[3] = 0x02;
        d[4] = 31;
        d[8..16].copy_from_slice(VENDOR);
        d[16..32].copy_from_slice(PRODUCT);
        d[32..36].copy_from_slice(REVISION);
        return good(trunc(d, alloc));
    }
    let mut d: Vec<u8> = vec![0x05, page, 0, 0];
    match page {
        0x00 => d.extend_from_slice(&[0x00, 0x80, 0x83]),
        0x80 => d.extend_from_slice(disk.serial.as_bytes()),
        0x83 => {
            let naa = (0x3u64 << 60) | (disk.id & 0x0FFF_FFFF_FFFF_FFFF);
            d.extend_from_slice(&[0x01, 0x03, 0x00, 0x08]);
            d.extend_from_slice(&naa.to_be_bytes());
        }
        _ => return bad(ILLEGAL, 0x24, 0x00),
    }
    let len = (d.len() - 4) as u16;
    d[2..4].copy_from_slice(&len.to_be_bytes());
    good(trunc(d, alloc))
}

fn read_toc(blocks: u64, cdb: &[u8; 16]) -> ScsiResult {
    let use_msf = cdb[1] & 0x02 != 0;
    let mut format = cdb[2] & 0x0F;
    if format == 0 && cdb[9] >> 6 != 0 {
        format = cdb[9] >> 6;
    }
    let alloc = b16(&cdb[7..9]);
    let mut d: Vec<u8> = vec![0, 0, 1, 1];
    match format {
        0 => {
            if cdb[6] <= 1 {
                d.extend_from_slice(&[0, 0x14, 1, 0]);
                d.extend_from_slice(&addr(0, use_msf));
            }
            d.extend_from_slice(&[0, 0x14, 0xAA, 0]);
            d.extend_from_slice(&addr(blocks, use_msf));
        }
        1 => {
            d.extend_from_slice(&[0, 0x14, 1, 0]);
            d.extend_from_slice(&addr(0, use_msf));
        }
        2 => {
            let lo = msf(blocks);
            d.extend_from_slice(&[1, 0x14, 0, 0xA0, 0, 0, 0, 0, 1, 0x00, 0]);
            d.extend_from_slice(&[1, 0x14, 0, 0xA1, 0, 0, 0, 0, 1, 0, 0]);
            d.extend_from_slice(&[1, 0x14, 0, 0xA2, 0, 0, 0, 0, lo[1], lo[2], lo[3]]);
            d.extend_from_slice(&[1, 0x14, 0, 1, 0, 0, 0, 0, 0, 2, 0]);
        }
        _ => return bad(ILLEGAL, 0x24, 0x00),
    }
    let len = (d.len() - 2) as u16;
    d[0..2].copy_from_slice(&len.to_be_bytes());
    good(trunc(d, alloc))
}

fn feature(code: u16, flags: u8, data: &[u8]) -> (u16, Vec<u8>) {
    let mut v = vec![(code >> 8) as u8, code as u8, flags, data.len() as u8];
    v.extend_from_slice(data);
    (code, v)
}

fn get_configuration(dvd: bool, cdb: &[u8; 16]) -> ScsiResult {
    let rt = cdb[1] & 3;
    let start = b16(&cdb[2..4]) as u16;
    let alloc = b16(&cdb[7..9]);
    let profile: u16 = if dvd { 0x0010 } else { 0x0008 };
    let mut feats = vec![
        feature(0x0000, 0x03, &[(profile >> 8) as u8, profile as u8, 0x01, 0]),
        feature(0x0001, 0x0B, &[0, 0, 0, 1, 0x01, 0, 0, 0]),
        feature(0x0002, 0x07, &[0x02, 0, 0, 0]),
        feature(0x0003, 0x0B, &[0x29, 0, 0, 0]),
        feature(0x0010, 0x03, &[0, 0, 0x08, 0, 0, 1, 0, 0]),
    ];
    if dvd {
        feats.push(feature(0x001F, 0x03, &[0, 0, 0, 0]));
    } else {
        feats.push(feature(0x001E, 0x0B, &[0, 0, 0, 0]));
    }
    let mut body = Vec::new();
    for (code, v) in feats {
        let take = if rt == 2 { code == start } else { code >= start };
        if take {
            body.extend_from_slice(&v);
        }
    }
    let mut d = vec![0u8; 8];
    d[0..4].copy_from_slice(&((body.len() + 4) as u32).to_be_bytes());
    d[6..8].copy_from_slice(&profile.to_be_bytes());
    d.extend_from_slice(&body);
    good(trunc(d, alloc))
}

fn mode_sense(cdb: &[u8; 16]) -> ScsiResult {
    let ten = cdb[0] == 0x5A;
    let pc = cdb[2] >> 6;
    let page = cdb[2] & 0x3f;
    let alloc = if ten { b16(&cdb[7..9]) } else { cdb[4] as u64 };
    if pc == 3 {
        return bad(ILLEGAL, 0x39, 0x00);
    }
    if page != 0x2A && page != 0x3F {
        return bad(ILLEGAL, 0x24, 0x00);
    }
    let mut p = vec![0u8; 22];
    p[0] = 0x2A;
    p[1] = 20;
    if pc != 1 {
        p[2] = 0x00; // reads CD-ROM
        p[6] = 0x29; // tray, eject, lock
        p[8..10].copy_from_slice(&7056u16.to_be_bytes());
        p[10..12].copy_from_slice(&256u16.to_be_bytes());
        p[12..14].copy_from_slice(&64u16.to_be_bytes());
        p[14..16].copy_from_slice(&7056u16.to_be_bytes());
    }
    let mut d = Vec::new();
    if ten {
        let total = 8 + p.len();
        d.extend_from_slice(&((total - 2) as u16).to_be_bytes());
        d.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    } else {
        let total = 4 + p.len();
        d.extend_from_slice(&[(total - 1) as u8, 0, 0, 0]);
    }
    d.extend_from_slice(&p);
    good(trunc(d, alloc))
}

fn dvd_structure(dvd: bool, blocks: u64, cdb: &[u8; 16]) -> ScsiResult {
    if !dvd {
        return bad(ILLEGAL, 0x30, 0x02); // cannot read medium - incompatible format
    }
    let alloc = b16(&cdb[8..10]);
    match cdb[7] {
        0x00 => {
            let mut d = vec![0u8; 4 + 17];
            d[0..2].copy_from_slice(&19u16.to_be_bytes());
            let b = 4;
            d[b] = 0x01; // DVD-ROM, part version 1
            d[b + 1] = 0x02; // 120 mm, max rate 10 Mbps
            d[b + 2] = 0x01; // single layer, read-only
            d[b + 5] = 0x03; // start PSN 0x030000
            let end = 0x0003_0000u64 + blocks - 1;
            d[b + 9] = (end >> 16) as u8;
            d[b + 10] = (end >> 8) as u8;
            d[b + 11] = end as u8;
            good(trunc(d, alloc))
        }
        0x01 => good(trunc(vec![0, 6, 0, 0, 0, 0, 0, 0], alloc)),
        _ => bad(ILLEGAL, 0x24, 0x00),
    }
}
