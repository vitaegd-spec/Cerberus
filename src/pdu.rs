//! iSCSI PDU framing (RFC 7143). Header/data digests are never negotiated, so
//! a PDU is: 48-byte BHS + optional AHS + data segment padded to 4 bytes.

use std::io::{self, Read, Write};

pub const BHS_LEN: usize = 48;

pub mod op {
    // Initiator -> target
    pub const NOP_OUT: u8 = 0x00;
    pub const SCSI_CMD: u8 = 0x01;
    pub const TASK_MGMT_REQ: u8 = 0x02;
    pub const LOGIN_REQ: u8 = 0x03;
    pub const TEXT_REQ: u8 = 0x04;
    pub const DATA_OUT: u8 = 0x05;
    pub const LOGOUT_REQ: u8 = 0x06;
    pub const SNACK_REQ: u8 = 0x10;
    // Target -> initiator
    pub const NOP_IN: u8 = 0x20;
    pub const SCSI_RESP: u8 = 0x21;
    pub const TASK_MGMT_RESP: u8 = 0x22;
    pub const LOGIN_RESP: u8 = 0x23;
    pub const TEXT_RESP: u8 = 0x24;
    pub const DATA_IN: u8 = 0x25;
    pub const LOGOUT_RESP: u8 = 0x26;
    pub const R2T: u8 = 0x31;
    pub const REJECT: u8 = 0x3f;
}

pub fn op_name(opcode: u8) -> &'static str {
    match opcode {
        op::NOP_OUT => "NOP-Out",
        op::SCSI_CMD => "SCSI-Command",
        op::TASK_MGMT_REQ => "TaskMgmt-Req",
        op::LOGIN_REQ => "Login-Req",
        op::TEXT_REQ => "Text-Req",
        op::DATA_OUT => "Data-Out",
        op::LOGOUT_REQ => "Logout-Req",
        op::SNACK_REQ => "SNACK",
        op::NOP_IN => "NOP-In",
        op::SCSI_RESP => "SCSI-Response",
        op::TASK_MGMT_RESP => "TaskMgmt-Resp",
        op::LOGIN_RESP => "Login-Resp",
        op::TEXT_RESP => "Text-Resp",
        op::DATA_IN => "Data-In",
        op::LOGOUT_RESP => "Logout-Resp",
        op::R2T => "R2T",
        op::REJECT => "Reject",
        _ => "Unknown",
    }
}

#[derive(Clone)]
pub struct Pdu {
    pub bhs: [u8; BHS_LEN],
    pub data: Vec<u8>,
}

impl Pdu {
    pub fn new(opcode: u8) -> Pdu {
        let mut bhs = [0u8; BHS_LEN];
        bhs[0] = opcode;
        Pdu { bhs, data: Vec::new() }
    }
    pub fn opcode(&self) -> u8 {
        self.bhs[0] & 0x3f
    }
    pub fn immediate(&self) -> bool {
        self.bhs[0] & 0x40 != 0
    }
    pub fn flags(&self) -> u8 {
        self.bhs[1]
    }
    pub fn u16_at(&self, o: usize) -> u16 {
        u16::from_be_bytes([self.bhs[o], self.bhs[o + 1]])
    }
    pub fn set_u16(&mut self, o: usize, v: u16) {
        self.bhs[o..o + 2].copy_from_slice(&v.to_be_bytes());
    }
    pub fn u32_at(&self, o: usize) -> u32 {
        u32::from_be_bytes([self.bhs[o], self.bhs[o + 1], self.bhs[o + 2], self.bhs[o + 3]])
    }
    pub fn set_u32(&mut self, o: usize, v: u32) {
        self.bhs[o..o + 4].copy_from_slice(&v.to_be_bytes());
    }
    pub fn itt(&self) -> u32 {
        self.u32_at(16)
    }
    pub fn set_itt(&mut self, v: u32) {
        self.set_u32(16, v);
    }
    pub fn lun_raw(&self) -> [u8; 8] {
        let mut l = [0u8; 8];
        l.copy_from_slice(&self.bhs[8..16]);
        l
    }
    pub fn set_lun_raw(&mut self, l: &[u8; 8]) {
        self.bhs[8..16].copy_from_slice(l);
    }
    pub fn describe(&self) -> String {
        format!(
            "{} op=0x{:02x} flags=0x{:02x} itt=0x{:08x} dlen={}",
            op_name(self.opcode()),
            self.opcode(),
            self.flags(),
            self.itt(),
            self.data.len()
        )
    }
}

/// Fill `buf` completely. Ok(false) = clean EOF before the first byte.
fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => {
                if got == 0 {
                    return Ok(false);
                }
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "connection closed mid-PDU"));
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// Read one PDU. Ok(None) = peer closed the connection cleanly.
pub fn read_pdu<R: Read>(r: &mut R, max_data: usize) -> io::Result<Option<Pdu>> {
    let mut bhs = [0u8; BHS_LEN];
    if !read_full(r, &mut bhs)? {
        return Ok(None);
    }
    let ahs_len = bhs[4] as usize * 4;
    let dlen = ((bhs[5] as usize) << 16) | ((bhs[6] as usize) << 8) | bhs[7] as usize;
    if ahs_len > 0 {
        // AHS (extended CDB / bidirectional length) is not used by this target - skip it.
        let mut ahs = vec![0u8; ahs_len];
        r.read_exact(&mut ahs)?;
    }
    if dlen > max_data {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("data segment {} exceeds limit {}", dlen, max_data),
        ));
    }
    let padded = (dlen + 3) & !3;
    let mut data = vec![0u8; padded];
    if padded > 0 {
        r.read_exact(&mut data)?;
    }
    data.truncate(dlen);
    Ok(Some(Pdu { bhs, data }))
}

/// Serialise and send one PDU in a single write.
pub fn write_pdu<W: Write>(w: &mut W, p: &Pdu) -> io::Result<()> {
    let dlen = p.data.len();
    let padded = (dlen + 3) & !3;
    let mut out = Vec::with_capacity(BHS_LEN + padded);
    out.extend_from_slice(&p.bhs);
    out[4] = 0; // no AHS
    out[5] = ((dlen >> 16) & 0xff) as u8;
    out[6] = ((dlen >> 8) & 0xff) as u8;
    out[7] = (dlen & 0xff) as u8;
    out.extend_from_slice(&p.data);
    out.resize(BHS_LEN + padded, 0);
    w.write_all(&out)
}
