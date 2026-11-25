//! SCSI block-device (SBC) emulation for one LUN backed by a disk image.
//! Optical images are handed to the MMC (CD/DVD) command set in mmc.rs.

use crate::disk::Disk;

pub const GOOD: u8 = 0x00;
pub const CHECK_CONDITION: u8 = 0x02;

// Sense keys
const NOT_READY: u8 = 0x02;
const MEDIUM_ERROR: u8 = 0x03;
const ILLEGAL_REQUEST: u8 = 0x05;
const DATA_PROTECT: u8 = 0x07;

const VENDOR: &[u8; 8] = b"CERBERUS";
const PRODUCT_DISK: &[u8; 16] = b"Virtual Disk    ";
const PRODUCT_FLOPPY: &[u8; 16] = b"Virtual Floppy  ";
const REVISION: &[u8; 4] = b"0.3 ";

/// Largest single transfer we accept (also advertised in VPD page B0).
pub const MAX_XFER_BLOCKS: u64 = 2048; // 1 MiB at 512 B
const HARD_LIMIT_BLOCKS: u64 = 65536; // 32 MiB safety cap

pub struct ScsiResult {
    pub status: u8,
    pub data: Vec<u8>,
    pub sense: Vec<u8>,
}

impl ScsiResult {
    pub(crate) fn good(data: Vec<u8>) -> ScsiResult {
        ScsiResult { status: GOOD, data, sense: Vec::new() }
    }
    pub(crate) fn check(key: u8, asc: u8, ascq: u8) -> ScsiResult {
        ScsiResult { status: CHECK_CONDITION, data: Vec::new(), sense: sense_fixed(key, asc, ascq) }
    }
}

pub fn sense_fixed(key: u8, asc: u8, ascq: u8) -> Vec<u8> {
    let mut s = vec![0u8; 18];
    s[0] = 0x70;
    s[2] = key;
    s[7] = 10;
    s[12] = asc;
    s[13] = ascq;
    s
}

fn be16(b: &[u8]) -> u64 {
    ((b[0] as u64) << 8) | b[1] as u64
}
fn be32(b: &[u8]) -> u64 {
    ((b[0] as u64) << 24) | ((b[1] as u64) << 16) | ((b[2] as u64) << 8) | b[3] as u64
}
fn be64(b: &[u8]) -> u64 {
    (be32(&b[0..4]) << 32) | be32(&b[4..8])
}
fn trunc(mut v: Vec<u8>, alloc: u64) -> Vec<u8> {
    if (v.len() as u64) > alloc {
        v.truncate(alloc as usize);
    }
    v
}

pub fn op_name(op: u8) -> &'static str {
    match op {
        0x00 => "TEST UNIT READY",
        0x03 => "REQUEST SENSE",
        0x08 => "READ(6)",
        0x0A => "WRITE(6)",
        0x12 => "INQUIRY",
        0x15 => "MODE SELECT(6)",
        0x1A => "MODE SENSE(6)",
        0x1B => "START STOP UNIT",
        0x1E => "PREVENT ALLOW MEDIUM REMOVAL",
        0x23 => "READ FORMAT CAPACITIES",
        0x25 => "READ CAPACITY(10)",
        0x28 => "READ(10)",
        0x2A => "WRITE(10)",
        0x2B => "SEEK(10)",
        0x2F => "VERIFY(10)",
        0x35 => "SYNCHRONIZE CACHE(10)",
        0x42 => "READ SUB-CHANNEL",
        0x43 => "READ TOC/PMA/ATIP",
        0x46 => "GET CONFIGURATION",
        0x4A => "GET EVENT STATUS NOTIFICATION",
        0x4D => "LOG SENSE",
        0x51 => "READ DISC INFORMATION",
        0x52 => "READ TRACK INFORMATION",
        0x55 => "MODE SELECT(10)",
        0x5A => "MODE SENSE(10)",
        0x5E => "PERSISTENT RESERVE IN",
        0x5F => "PERSISTENT RESERVE OUT",
        0x88 => "READ(16)",
        0x8A => "WRITE(16)",
        0x8F => "VERIFY(16)",
        0x91 => "SYNCHRONIZE CACHE(16)",
        0x9E => "SERVICE ACTION IN(16)",
        0xA0 => "REPORT LUNS",
        0xA3 => "MAINTENANCE IN",
        0xA8 => "READ(12)",
        0xAA => "WRITE(12)",
        0xAD => "READ DVD STRUCTURE",
        0xB6 => "SET STREAMING",
        0xBB => "SET CD SPEED",
        0xBD => "MECHANISM STATUS",
        0xBE => "READ CD",
        _ => "UNSUPPORTED",
    }
}

pub fn is_write_op(op: u8) -> bool {
    matches!(op, 0x0A | 0x2A | 0xAA | 0x8A)
}

/// (lba, block count) for READ/WRITE/VERIFY CDBs.
pub fn rw_params(cdb: &[u8; 16]) -> Option<(u64, u64)> {
    match cdb[0] {
        0x08 | 0x0A => {
            let lba = (((cdb[1] & 0x1f) as u64) << 16) | ((cdb[2] as u64) << 8) | cdb[3] as u64;
            let n = if cdb[4] == 0 { 256 } else { cdb[4] as u64 };
            Some((lba, n))
        }
        0x28 | 0x2A | 0x2F => Some((be32(&cdb[2..6]), be16(&cdb[7..9]))),
        0xA8 | 0xAA => Some((be32(&cdb[2..6]), be32(&cdb[6..10]))),
        0x88 | 0x8A | 0x8F => Some((be64(&cdb[2..10]), be32(&cdb[10..14]))),
        _ => None,
    }
}

pub fn execute(disk: &Disk, lun: u16, cdb: &[u8; 16], data_out: &[u8]) -> ScsiResult {
    if disk.optical {
        return crate::mmc::execute(disk, lun, cdb);
    }
    let op = cdb[0];

    if lun != 0 {
        match op {
            0x12 => {
                // INQUIRY on a non-existent LUN: peripheral qualifier 011b, type 1Fh.
                let mut d = vec![0u8; 36];
                d[0] = 0x7F;
                d[3] = 0x02;
                d[4] = 31;
                return ScsiResult::good(trunc(d, be16(&cdb[3..5])));
            }
            0xA0 | 0x03 => {}
            _ => return ScsiResult::check(ILLEGAL_REQUEST, 0x25, 0x00),
        }
    }

    match op {
        0x00 | 0x1E | 0x1B => ScsiResult::good(Vec::new()),
        0x03 => {
            let d = sense_fixed(0, 0, 0);
            ScsiResult::good(trunc(d, cdb[4] as u64))
        }
        0x12 => inquiry(disk, cdb),
        0x25 => {
            let last = disk.blocks - 1;
            let last32 = if last > 0xFFFF_FFFE { 0xFFFF_FFFFu32 } else { last as u32 };
            let mut d = Vec::with_capacity(8);
            d.extend_from_slice(&last32.to_be_bytes());
            d.extend_from_slice(&disk.block_size.to_be_bytes());
            ScsiResult::good(d)
        }
        0x9E => {
            if cdb[1] & 0x1f != 0x10 {
                return ScsiResult::check(ILLEGAL_REQUEST, 0x24, 0x00);
            }
            let mut d = vec![0u8; 32];
            d[0..8].copy_from_slice(&(disk.blocks - 1).to_be_bytes());
            d[8..12].copy_from_slice(&disk.block_size.to_be_bytes());
            ScsiResult::good(trunc(d, be32(&cdb[10..14])))
        }
        0x23 => {
            // READ FORMAT CAPACITIES (used for removable / floppy media).
            let mut d = vec![0u8; 12];
            d[3] = 8;
            let n = disk.blocks.min(0xFFFF_FFFF) as u32;
            d[4..8].copy_from_slice(&n.to_be_bytes());
            d[8] = 0x02; // formatted media
            let bs = disk.block_size;
            d[9] = (bs >> 16) as u8;
            d[10] = (bs >> 8) as u8;
            d[11] = bs as u8;
            ScsiResult::good(trunc(d, be16(&cdb[7..9])))
        }
        0x1A | 0x5A => mode_sense(disk, cdb),
        0x15 | 0x55 => ScsiResult::good(Vec::new()), // accept & ignore MODE SELECT
        0x35 | 0x91 => match disk.flush() {
            Ok(()) => ScsiResult::good(Vec::new()),
            Err(_) => ScsiResult::check(MEDIUM_ERROR, 0x0C, 0x00),
        },
        0xA0 => {
            let mut d = vec![0u8; 16];
            d[3] = 8; // one LUN (LUN 0 = all zeros)
            ScsiResult::good(trunc(d, be32(&cdb[6..10])))
        }
        0x2F | 0x8F => {
            let (lba, n) = rw_params(cdb).unwrap_or((0, 0));
            if lba.checked_add(n).map_or(true, |e| e > disk.blocks) {
                return ScsiResult::check(ILLEGAL_REQUEST, 0x21, 0x00);
            }
            ScsiResult::good(Vec::new())
        }
        0x08 | 0x28 | 0xA8 | 0x88 => {
            let (lba, n) = rw_params(cdb).unwrap_or((0, 0));
            if lba.checked_add(n).map_or(true, |e| e > disk.blocks) {
                return ScsiResult::check(ILLEGAL_REQUEST, 0x21, 0x00);
            }
            if n > HARD_LIMIT_BLOCKS {
                return ScsiResult::check(ILLEGAL_REQUEST, 0x24, 0x00);
            }
            if n == 0 {
                return ScsiResult::good(Vec::new());
            }
            match disk.read(lba, n) {
                Ok(d) => ScsiResult::good(d),
                Err(e) => {
                    warn!("read error lba={} n={}: {}", lba, n, e);
                    ScsiResult::check(MEDIUM_ERROR, 0x11, 0x00)
                }
            }
        }
        0x0A | 0x2A | 0xAA | 0x8A => {
            if disk.readonly {
                return ScsiResult::check(DATA_PROTECT, 0x27, 0x00);
            }
            let (lba, n) = rw_params(cdb).unwrap_or((0, 0));
            if lba.checked_add(n).map_or(true, |e| e > disk.blocks) {
                return ScsiResult::check(ILLEGAL_REQUEST, 0x21, 0x00);
            }
            let need = (n * disk.block_size as u64) as usize;
            if data_out.len() < need {
                warn!("write lba={} n={} got only {} of {} bytes", lba, n, data_out.len(), need);
                return ScsiResult::check(ILLEGAL_REQUEST, 0x1A, 0x00);
            }
            if n == 0 {
                return ScsiResult::good(Vec::new());
            }
            if let Err(e) = disk.write(lba, &data_out[..need]) {
                warn!("write error lba={} n={}: {}", lba, n, e);
                return ScsiResult::check(MEDIUM_ERROR, 0x0C, 0x00);
            }
            // FUA (force unit access) on WRITE(10/12/16)
            if op != 0x0A && cdb[1] & 0x08 != 0 {
                let _ = disk.flush();
            }
            ScsiResult::good(Vec::new())
        }
        _ => ScsiResult::check(ILLEGAL_REQUEST, 0x20, 0x00),
    }
}

fn inquiry(disk: &Disk, cdb: &[u8; 16]) -> ScsiResult {
    let evpd = cdb[1] & 0x01 != 0;
    let page = cdb[2];
    let alloc = be16(&cdb[3..5]);
    if !evpd {
        if page != 0 {
            return ScsiResult::check(ILLEGAL_REQUEST, 0x24, 0x00);
        }
        let mut d = vec![0u8; 36];
        d[0] = 0x00; // direct-access block device
        d[1] = if disk.removable { 0x80 } else { 0x00 };
        d[2] = 0x06; // SPC-4
        d[3] = 0x12; // HiSup + response data format 2
        d[4] = 31;
        d[7] = 0x02; // CmdQue
        d[8..16].copy_from_slice(VENDOR);
        d[16..32].copy_from_slice(if disk.removable { PRODUCT_FLOPPY } else { PRODUCT_DISK });
        d[32..36].copy_from_slice(REVISION);
        return ScsiResult::good(trunc(d, alloc));
    }
    let mut d: Vec<u8> = vec![0x00, page, 0, 0];
    match page {
        0x00 => d.extend_from_slice(&[0x00, 0x80, 0x83, 0xB0, 0xB1]),
        0x80 => d.extend_from_slice(disk.serial.as_bytes()),
        0x83 => {
            // NAA 3 (locally assigned) - unique per image path.
            let naa = (0x3u64 << 60) | (disk.id & 0x0FFF_FFFF_FFFF_FFFF);
            d.extend_from_slice(&[0x01, 0x03, 0x00, 0x08]);
            d.extend_from_slice(&naa.to_be_bytes());
            // T10 vendor ID designator.
            let mut t10 = Vec::new();
            t10.extend_from_slice(VENDOR);
            t10.extend_from_slice(disk.serial.as_bytes());
            d.extend_from_slice(&[0x02, 0x01, 0x00, t10.len() as u8]);
            d.extend_from_slice(&t10);
        }
        0xB0 => {
            let mut p = vec![0u8; 0x3C];
            p[4..8].copy_from_slice(&(MAX_XFER_BLOCKS as u32).to_be_bytes()); // max transfer length
            p[8..12].copy_from_slice(&128u32.to_be_bytes()); // optimal transfer length
            d.extend_from_slice(&p);
        }
        0xB1 => {
            let mut p = vec![0u8; 0x3C];
            p[0..2].copy_from_slice(&1u16.to_be_bytes()); // non-rotating medium
            d.extend_from_slice(&p);
        }
        _ => return ScsiResult::check(ILLEGAL_REQUEST, 0x24, 0x00),
    }
    let len = (d.len() - 4) as u16;
    d[2..4].copy_from_slice(&len.to_be_bytes());
    ScsiResult::good(trunc(d, alloc))
}

fn mode_page(page: u8, changeable: bool) -> Option<Vec<u8>> {
    let mut p = match page {
        0x08 => {
            let mut v = vec![0u8; 20];
            v[0] = 0x08;
            v[1] = 0x12;
            v // WCE=0 (write-through), RCD=0
        }
        0x0A => {
            let mut v = vec![0u8; 12];
            v[0] = 0x0A;
            v[1] = 0x0A;
            v
        }
        0x1C => {
            let mut v = vec![0u8; 12];
            v[0] = 0x1C;
            v[1] = 0x0A;
            v
        }
        _ => return None,
    };
    if changeable {
        for b in p.iter_mut().skip(2) {
            *b = 0;
        }
    }
    Some(p)
}

fn mode_sense(disk: &Disk, cdb: &[u8; 16]) -> ScsiResult {
    let ten = cdb[0] == 0x5A;
    let dbd = cdb[1] & 0x08 != 0;
    let pc = cdb[2] >> 6;
    let page = cdb[2] & 0x3f;
    let subpage = cdb[3];
    let alloc = if ten { be16(&cdb[7..9]) } else { cdb[4] as u64 };
    if pc == 3 {
        return ScsiResult::check(ILLEGAL_REQUEST, 0x39, 0x00); // saving parameters not supported
    }
    if subpage != 0 && !(page == 0x3f && subpage == 0xff) {
        return ScsiResult::check(ILLEGAL_REQUEST, 0x24, 0x00);
    }
    let changeable = pc == 1;
    let mut pages = Vec::new();
    if page == 0x3f {
        for pg in [0x08u8, 0x0A, 0x1C] {
            if let Some(p) = mode_page(pg, changeable) {
                pages.extend_from_slice(&p);
            }
        }
    } else {
        match mode_page(page, changeable) {
            Some(p) => pages.extend_from_slice(&p),
            None => return ScsiResult::check(ILLEGAL_REQUEST, 0x24, 0x00),
        }
    }
    let mut bd = Vec::new();
    if !dbd {
        let n = disk.blocks.min(0xFFFF_FFFF) as u32;
        bd.extend_from_slice(&n.to_be_bytes());
        bd.push(0);
        let bs = disk.block_size;
        bd.extend_from_slice(&[(bs >> 16) as u8, (bs >> 8) as u8, bs as u8]);
    }
    let wp = if disk.readonly { 0x80 } else { 0x00 };
    let mut d = Vec::new();
    if ten {
        let total = 8 + bd.len() + pages.len();
        d.extend_from_slice(&((total - 2) as u16).to_be_bytes());
        d.push(0); // medium type
        d.push(wp);
        d.push(0);
        d.push(0);
        d.extend_from_slice(&(bd.len() as u16).to_be_bytes());
    } else {
        let total = 4 + bd.len() + pages.len();
        d.push((total - 1) as u8);
        d.push(0);
        d.push(wp);
        d.push(bd.len() as u8);
    }
    d.extend_from_slice(&bd);
    d.extend_from_slice(&pages);
    ScsiResult::good(trunc(d, alloc))
}

#[allow(dead_code)]
pub fn not_ready() -> ScsiResult {
    ScsiResult::check(NOT_READY, 0x3A, 0x00)
}
