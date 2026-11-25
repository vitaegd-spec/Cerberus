//! One iSCSI connection = one session (MaxConnections=1, ErrorRecoveryLevel=0).
//! Handles Login (security + operational negotiation), discovery (SendTargets),
//! SCSI commands with Data-In / R2T / Data-Out, NOP, task management and logout.
//! Phase 1: several images are served at once - each one is its own target (IQN)
//! looked up in the shared Registry at login time.

use std::collections::HashMap;
use std::io;
use std::net::TcpStream;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use crate::disk::Disk;
use crate::login::{self, kv};
use crate::pdu::{self, op, Pdu};
use crate::scsi::{self, ScsiResult, GOOD};
use crate::target::Registry;

pub struct TargetConfig {
    pub registry: Arc<Registry>,
    pub portal_tag: u16,
}

const OUR_MRDSL: u32 = 262_144; // what we can receive per PDU
const OUR_MAX_BURST: u32 = 262_144;
const OUR_FIRST_BURST: u32 = 65_536;
const CMD_WINDOW: u32 = 32;
const MAX_PDU_DATA: usize = 16 << 20;
const MAX_WRITE_BYTES: u32 = 64 << 20;

static NEXT_TSIH: AtomicU16 = AtomicU16::new(1);

#[derive(Clone, Copy, PartialEq, Debug)]
enum SessionType {
    Unknown,
    Discovery,
    Normal,
}

struct WriteTask {
    lun: u16,
    lun_raw: [u8; 8],
    cdb: [u8; 16],
    expected: u32,
    buf: Vec<u8>,
    received: u32,
    burst_end: u32,
    ttt: u32,
    r2tsn: u32,
}

pub struct Conn {
    stream: TcpStream,
    peer: String,
    local_addr: String,
    cfg: Arc<TargetConfig>,
    disk: Option<Arc<Disk>>,
    target_name: String,
    stat_sn: u32,
    exp_cmd_sn: u32,
    full_feature: bool,
    session_type: SessionType,
    initiator_name: String,
    target_ok: bool,
    // negotiated
    peer_mrdsl: u32,
    max_burst: u32,
    first_burst: u32,
    immediate_data: bool,
    sent_mrdsl: bool,
    sent_tpgt: bool,
    tsih: u16,
    login_buf: Vec<u8>,
    pending: HashMap<u32, WriteTask>,
    next_ttt: u32,
    // stats
    cmds: u64,
    bytes_read: u64,
    bytes_written: u64,
    warned_ops: Vec<u8>,
}

fn decode_lun(l: &[u8; 8]) -> u16 {
    (((l[0] & 0x3f) as u16) << 8) | l[1] as u16
}

impl Conn {
    pub fn new(stream: TcpStream, cfg: Arc<TargetConfig>) -> Conn {
        let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_else(|_| "?".into());
        let local_addr = stream
            .local_addr()
            .map(|a| a.to_string())
            .unwrap_or_else(|_| "127.0.0.1:3260".into());
        let _ = stream.set_nodelay(true);
        Conn {
            stream,
            peer,
            local_addr,
            cfg,
            disk: None,
            target_name: String::new(),
            stat_sn: 1,
            exp_cmd_sn: 0,
            full_feature: false,
            session_type: SessionType::Unknown,
            initiator_name: String::new(),
            target_ok: false,
            peer_mrdsl: 8192,
            max_burst: OUR_MAX_BURST,
            first_burst: OUR_FIRST_BURST,
            immediate_data: true,
            sent_mrdsl: false,
            sent_tpgt: false,
            tsih: 0,
            login_buf: Vec::new(),
            pending: HashMap::new(),
            next_ttt: 1,
            cmds: 0,
            bytes_read: 0,
            bytes_written: 0,
            warned_ops: Vec::new(),
        }
    }

    pub fn run(mut self) {
        debug!("[{}] connection opened (target side {})", self.peer, self.local_addr);
        loop {
            let p = match pdu::read_pdu(&mut self.stream, MAX_PDU_DATA) {
                Ok(Some(p)) => p,
                Ok(None) => {
                    debug!("[{}] initiator closed the connection", self.peer);
                    break;
                }
                Err(e) => {
                    debug!("[{}] receive ended: {}", self.peer, e);
                    break;
                }
            };
            trace!("[{}] <- {}", self.peer, p.describe());
            let r = if self.full_feature { self.handle_ffp(p) } else { self.handle_login(p) };
            match r {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    warn!("[{}] send error: {}", self.peer, e);
                    break;
                }
            }
        }
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if self.session_type == SessionType::Normal && self.full_feature {
            info!(
                "iSCSI session closed ({}) - {} commands, {} read, {} written",
                short_name(&self.target_name),
                self.cmds,
                human(self.bytes_read),
                human(self.bytes_written)
            );
        }
    }

    fn send(&mut self, p: &Pdu) -> io::Result<()> {
        trace!("[{}] -> {}", self.peer, p.describe());
        pdu::write_pdu(&mut self.stream, p)
    }

    fn max_cmd_sn(&self) -> u32 {
        self.exp_cmd_sn.wrapping_add(CMD_WINDOW - 1)
    }

    /// Fill StatSN / ExpCmdSN / MaxCmdSN, optionally consuming a StatSN.
    fn stamp(&mut self, p: &mut Pdu, advance: bool) {
        p.set_u32(24, self.stat_sn);
        p.set_u32(28, self.exp_cmd_sn);
        p.set_u32(32, self.max_cmd_sn());
        if advance {
            self.stat_sn = self.stat_sn.wrapping_add(1);
        }
    }

    fn stamp_no_status(&self, p: &mut Pdu) {
        p.set_u32(28, self.exp_cmd_sn);
        p.set_u32(32, self.max_cmd_sn());
    }

    // ------------------------------------------------------------------ login

    fn handle_login(&mut self, p: Pdu) -> io::Result<bool> {
        if p.opcode() != op::LOGIN_REQ {
            warn!("[{}] {} received before login completed - closing", self.peer, pdu::op_name(p.opcode()));
            return Ok(false);
        }
        let flags = p.flags();
        let transit = flags & 0x80 != 0;
        let cont = flags & 0x40 != 0;
        let csg = (flags >> 2) & 0x03;
        let nsg = flags & 0x03;
        let itt = p.itt();
        let tsih_req = p.u16_at(14);
        let mut isid = [0u8; 6];
        isid.copy_from_slice(&p.bhs[8..14]);
        self.exp_cmd_sn = p.u32_at(24); // login is immediate: ExpCmdSN = CmdSN

        self.login_buf.extend_from_slice(&p.data);
        if cont {
            // Initiator is still sending text; acknowledge with an empty response.
            let mut r = self.login_resp(&isid, itt, csg << 2, 0, 0);
            self.stamp(&mut r, true);
            self.send(&r)?;
            return Ok(true);
        }
        let keys = login::parse(&std::mem::take(&mut self.login_buf));
        debug!(
            "[{}] LOGIN stage {}->{}{}  {}",
            self.peer,
            stage_name(csg),
            if transit { stage_name(nsg) } else { "(stay)" },
            if tsih_req != 0 { format!(" TSIH={}", tsih_req) } else { String::new() },
            login::render(&keys)
        );

        let mut out: Vec<(String, String)> = Vec::new();
        let mut fail: Option<(u8, u8, &str)> = None;
        let discovery_irrelevant = [
            "MaxConnections",
            "InitialR2T",
            "ImmediateData",
            "MaxBurstLength",
            "FirstBurstLength",
            "MaxOutstandingR2T",
            "DataPDUInOrder",
            "DataSequenceInOrder",
        ];

        for (k, v) in &keys {
            // Session type must be known before deciding relevance.
            if k == "SessionType" {
                self.session_type = if v == "Discovery" { SessionType::Discovery } else { SessionType::Normal };
            }
        }
        if self.session_type == SessionType::Unknown {
            self.session_type = SessionType::Normal;
        }

        for (k, v) in keys {
            if v == "NotUnderstood" || v == "Irrelevant" || v == "Reject" {
                continue;
            }
            if self.session_type == SessionType::Discovery && discovery_irrelevant.contains(&k.as_str()) {
                out.push(kv(&k, "Irrelevant"));
                continue;
            }
            match k.as_str() {
                "InitiatorName" => self.initiator_name = v,
                "InitiatorAlias" | "SessionType" => {}
                "TargetName" => {
                    let found = self.cfg.registry.get(&v);
                    match found {
                        Some(d) => {
                            self.disk = Some(d);
                            self.target_ok = true;
                            self.target_name = v;
                        }
                        None => fail = Some((0x02, 0x03, "unknown TargetName (image no longer mounted)")),
                    }
                }
                "AuthMethod" => {
                    if v.split(',').any(|m| m == "None") {
                        out.push(kv("AuthMethod", "None"));
                    } else {
                        fail = Some((0x02, 0x01, "only AuthMethod=None is supported (disable CHAP)"));
                    }
                }
                "HeaderDigest" | "DataDigest" => {
                    let val = if v.split(',').any(|m| m == "None") { "None" } else { "Reject" };
                    out.push(kv(&k, val));
                }
                "MaxRecvDataSegmentLength" => {
                    self.peer_mrdsl = v.parse::<u32>().unwrap_or(8192).clamp(512, 16_777_215);
                }
                "MaxBurstLength" => {
                    let n = v.parse::<u32>().unwrap_or(OUR_MAX_BURST).clamp(512, OUR_MAX_BURST);
                    self.max_burst = n;
                    out.push(kv(&k, &n.to_string()));
                }
                "FirstBurstLength" => {
                    let n = v.parse::<u32>().unwrap_or(OUR_FIRST_BURST).clamp(512, OUR_FIRST_BURST);
                    self.first_burst = n;
                    out.push(kv(&k, &n.to_string()));
                }
                "MaxConnections" => out.push(kv(&k, "1")),
                "ErrorRecoveryLevel" => out.push(kv(&k, "0")),
                "InitialR2T" => out.push(kv(&k, "Yes")),
                "ImmediateData" => {
                    self.immediate_data = v == "Yes";
                    out.push(kv(&k, if self.immediate_data { "Yes" } else { "No" }));
                }
                "DataPDUInOrder" | "DataSequenceInOrder" => out.push(kv(&k, "Yes")),
                "DefaultTime2Wait" => {
                    let n = v.parse::<u32>().unwrap_or(2).max(2);
                    out.push(kv(&k, &n.to_string()));
                }
                "DefaultTime2Retain" => out.push(kv(&k, "0")),
                "MaxOutstandingR2T" => out.push(kv(&k, "1")),
                "IFMarker" | "OFMarker" => out.push(kv(&k, "No")),
                "IFMarkInt" | "OFMarkInt" => out.push(kv(&k, "Irrelevant")),
                _ => out.push(kv(&k, "NotUnderstood")),
            }
        }

        // TargetName is mandatory in the first login PDU of a normal session.
        if fail.is_none() && self.session_type == SessionType::Normal && !self.target_ok {
            fail = Some((0x02, 0x07, "TargetName missing for a normal session"));
        }

        if let Some((class, detail, why)) = fail {
            debug!("[{}] LOGIN REJECTED: {} (status {:02x}/{:02x})", self.peer, why, class, detail);
            let mut r = self.login_resp(&isid, itt, csg << 2, class, detail);
            self.stamp(&mut r, true);
            self.send(&r)?;
            return Ok(false);
        }

        if self.session_type == SessionType::Normal && !self.sent_tpgt {
            out.push(kv("TargetPortalGroupTag", &self.cfg.portal_tag.to_string()));
            self.sent_tpgt = true;
        }
        if csg == 1 && !self.sent_mrdsl {
            out.push(kv("MaxRecvDataSegmentLength", &OUR_MRDSL.to_string()));
            self.sent_mrdsl = true;
        }

        let to_ffp = transit && nsg == 3;
        let out_flags = if transit { 0x80 | (csg << 2) | nsg } else { csg << 2 };
        let mut r = self.login_resp(&isid, itt, out_flags, 0, 0);
        if to_ffp {
            self.tsih = if tsih_req != 0 { tsih_req } else { NEXT_TSIH.fetch_add(1, Ordering::Relaxed).max(1) };
            r.set_u16(14, self.tsih);
        }
        r.data = login::encode(&out);
        debug!("[{}] LOGIN reply  {}", self.peer, login::render(&out));
        self.stamp(&mut r, true);
        self.send(&r)?;

        if to_ffp {
            self.full_feature = true;
            if self.session_type == SessionType::Normal {
                info!(
                    "iSCSI session established ({}) - MaxBurst={} FirstBurst={} ImmediateData={}",
                    short_name(&self.target_name),
                    self.max_burst,
                    self.first_burst,
                    self.immediate_data
                );
            } else {
                debug!("[{}] discovery session established", self.peer);
            }
        }
        Ok(true)
    }

    fn login_resp(&self, isid: &[u8; 6], itt: u32, flags: u8, class: u8, detail: u8) -> Pdu {
        let mut r = Pdu::new(op::LOGIN_RESP);
        r.bhs[1] = flags;
        r.bhs[2] = 0x00; // Version-max
        r.bhs[3] = 0x00; // Version-active
        r.bhs[8..14].copy_from_slice(isid);
        r.set_itt(itt);
        r.bhs[36] = class;
        r.bhs[37] = detail;
        r
    }

    // ------------------------------------------------------- full feature phase

    fn handle_ffp(&mut self, p: Pdu) -> io::Result<bool> {
        let opc = p.opcode();
        let numbered = matches!(opc, op::NOP_OUT | op::SCSI_CMD | op::TASK_MGMT_REQ | op::TEXT_REQ | op::LOGOUT_REQ);
        if numbered && !p.immediate() {
            let cmd_sn = p.u32_at(24);
            if cmd_sn != self.exp_cmd_sn {
                debug!("[{}] CmdSN {} != ExpCmdSN {} (resyncing)", self.peer, cmd_sn, self.exp_cmd_sn);
            }
            self.exp_cmd_sn = cmd_sn.wrapping_add(1);
        }
        match opc {
            op::NOP_OUT => self.nop(p),
            op::SCSI_CMD => self.scsi_cmd(p),
            op::DATA_OUT => self.data_out(p),
            op::TEXT_REQ => self.text(p),
            op::TASK_MGMT_REQ => self.tmf(p),
            op::LOGOUT_REQ => {
                self.logout(p)?;
                Ok(false)
            }
            _ => {
                warn!("[{}] unsupported PDU {} - sending Reject", self.peer, p.describe());
                self.reject(&p, 0x04)
            }
        }
    }

    fn nop(&mut self, p: Pdu) -> io::Result<bool> {
        if p.itt() == 0xFFFF_FFFF {
            return Ok(true); // reply to a target ping (we never ping)
        }
        let mut r = Pdu::new(op::NOP_IN);
        r.bhs[1] = 0x80;
        r.set_lun_raw(&p.lun_raw());
        r.set_itt(p.itt());
        r.set_u32(20, 0xFFFF_FFFF);
        r.data = p.data.clone();
        self.stamp(&mut r, true);
        self.send(&r)?;
        Ok(true)
    }

    fn text(&mut self, p: Pdu) -> io::Result<bool> {
        let keys = login::parse(&p.data);
        debug!("[{}] TEXT  {}", self.peer, login::render(&keys));
        let mut out = Vec::new();
        for (k, v) in keys {
            if k == "SendTargets" {
                for iqn in self.cfg.registry.list() {
                    if v == "All" || v == iqn || v.is_empty() {
                        out.push(kv("TargetName", &iqn));
                        out.push(kv("TargetAddress", &format!("{},{}", self.local_addr, self.cfg.portal_tag)));
                    }
                }
            } else {
                out.push(kv(&k, "NotUnderstood"));
            }
        }
        debug!("[{}] TEXT reply  {}", self.peer, login::render(&out));
        let mut r = Pdu::new(op::TEXT_RESP);
        r.bhs[1] = 0x80;
        r.set_lun_raw(&p.lun_raw());
        r.set_itt(p.itt());
        r.set_u32(20, 0xFFFF_FFFF);
        r.data = login::encode(&out);
        self.stamp(&mut r, true);
        self.send(&r)?;
        Ok(true)
    }

    fn tmf(&mut self, p: Pdu) -> io::Result<bool> {
        let func = p.bhs[1] & 0x7f;
        let ref_itt = p.u32_at(20);
        let response = match func {
            1 => {
                self.pending.remove(&ref_itt);
                0
            }
            2 | 3 | 4 | 5 | 6 => {
                self.pending.clear();
                0
            }
            _ => 5, // not supported
        };
        info!("[{}] TASK MGMT function {} -> response {}", self.peer, func, response);
        let mut r = Pdu::new(op::TASK_MGMT_RESP);
        r.bhs[1] = 0x80;
        r.bhs[2] = response;
        r.set_itt(p.itt());
        self.stamp(&mut r, true);
        self.send(&r)?;
        Ok(true)
    }

    fn logout(&mut self, p: Pdu) -> io::Result<()> {
        debug!("[{}] LOGOUT (reason {})", self.peer, p.bhs[1] & 0x7f);
        let mut r = Pdu::new(op::LOGOUT_RESP);
        r.bhs[1] = 0x80;
        r.bhs[2] = 0;
        r.set_itt(p.itt());
        self.stamp(&mut r, true);
        self.send(&r)
    }

    fn reject(&mut self, p: &Pdu, reason: u8) -> io::Result<bool> {
        let mut r = Pdu::new(op::REJECT);
        r.bhs[1] = 0x80;
        r.bhs[2] = reason;
        r.set_itt(0xFFFF_FFFF);
        r.data = p.bhs.to_vec();
        self.stamp(&mut r, true);
        self.send(&r)?;
        Ok(true)
    }

    // ------------------------------------------------------------------- SCSI

    fn scsi_cmd(&mut self, p: Pdu) -> io::Result<bool> {
        let disk = match self.disk.clone() {
            Some(d) => d,
            None => {
                warn!("[{}] SCSI command on a session without a disk - closing", self.peer);
                return Ok(false);
            }
        };
        self.cmds += 1;
        let flags = p.flags();
        let read = flags & 0x40 != 0;
        let write = flags & 0x20 != 0;
        let itt = p.itt();
        let expected = p.u32_at(20);
        let mut cdb = [0u8; 16];
        cdb.copy_from_slice(&p.bhs[32..48]);
        let lun_raw = p.lun_raw();
        let lun = decode_lun(&lun_raw);

        if crate::log::enabled(1) {
            match scsi::rw_params(&cdb) {
                Some((lba, n)) => debug!(
                    "[{}] SCSI {} lba={} blocks={} xfer={}",
                    self.peer,
                    scsi::op_name(cdb[0]),
                    lba,
                    n,
                    expected
                ),
                None => debug!(
                    "[{}] SCSI {} (0x{:02x}) lun={} xfer={} cdb={}",
                    self.peer,
                    scsi::op_name(cdb[0]),
                    cdb[0],
                    lun,
                    expected,
                    hex(&cdb[..cdb_len(cdb[0])])
                ),
            }
        }
        if scsi::op_name(cdb[0]) == "UNSUPPORTED" && !self.warned_ops.contains(&cdb[0]) {
            self.warned_ops.push(cdb[0]);
            debug!(
                "[{}] note: SCSI opcode 0x{:02x} not implemented (answered ILLEGAL REQUEST - normally harmless)",
                self.peer,
                cdb[0]
            );
        }

        if write && expected > 0 {
            if disk.readonly && scsi::is_write_op(cdb[0]) {
                let res = ScsiResult {
                    status: scsi::CHECK_CONDITION,
                    data: Vec::new(),
                    sense: scsi::sense_fixed(0x07, 0x27, 0x00),
                };
                return self.complete(itt, &lun_raw, false, expected, res, 0);
            }
            if expected > MAX_WRITE_BYTES {
                let res = ScsiResult {
                    status: scsi::CHECK_CONDITION,
                    data: Vec::new(),
                    sense: scsi::sense_fixed(0x05, 0x24, 0x00),
                };
                return self.complete(itt, &lun_raw, false, expected, res, 0);
            }
            let mut buf = vec![0u8; expected as usize];
            let imm = p.data.len().min(expected as usize);
            buf[..imm].copy_from_slice(&p.data[..imm]);
            let task = WriteTask {
                lun,
                lun_raw,
                cdb,
                expected,
                buf,
                received: imm as u32,
                burst_end: imm as u32,
                ttt: 0,
                r2tsn: 0,
            };
            if task.received >= expected {
                return self.finish_write(itt, task);
            }
            self.pending.insert(itt, task);
            self.send_r2t(itt)?;
            return Ok(true);
        }

        let res = scsi::execute(&disk, lun, &cdb, &[]);
        self.complete(itt, &lun_raw, read, expected, res, 0)
    }

    fn send_r2t(&mut self, itt: u32) -> io::Result<()> {
        let ttt = self.next_ttt;
        self.next_ttt = self.next_ttt.wrapping_add(1);
        if self.next_ttt == 0xFFFF_FFFF {
            self.next_ttt = 1;
        }
        let max_burst = self.max_burst;
        let (lun_raw, offset, len, r2tsn) = match self.pending.get_mut(&itt) {
            Some(t) => {
                let offset = t.received;
                let len = (t.expected - offset).min(max_burst);
                t.burst_end = offset + len;
                t.ttt = ttt;
                let sn = t.r2tsn;
                t.r2tsn += 1;
                (t.lun_raw, offset, len, sn)
            }
            None => return Ok(()),
        };
        let mut r = Pdu::new(op::R2T);
        r.bhs[1] = 0x80;
        r.set_lun_raw(&lun_raw);
        r.set_itt(itt);
        r.set_u32(20, ttt);
        self.stamp(&mut r, false);
        r.set_u32(36, r2tsn);
        r.set_u32(40, offset);
        r.set_u32(44, len);
        trace!("[{}] R2T itt=0x{:08x} offset={} len={}", self.peer, itt, offset, len);
        self.send(&r)
    }

    fn data_out(&mut self, p: Pdu) -> io::Result<bool> {
        let itt = p.itt();
        let offset = p.u32_at(40) as usize;
        let is_final = p.flags() & 0x80 != 0;
        let (complete, need_r2t) = match self.pending.get_mut(&itt) {
            Some(t) => {
                let end = offset + p.data.len();
                if end > t.buf.len() {
                    warn!("[{}] Data-Out beyond expected length (offset {} len {}) - ignored", self.peer, offset, p.data.len());
                    return Ok(true);
                }
                t.buf[offset..end].copy_from_slice(&p.data);
                t.received += p.data.len() as u32;
                let complete = t.received >= t.expected;
                let need_r2t = !complete && is_final && t.received >= t.burst_end;
                (complete, need_r2t)
            }
            None => {
                debug!("[{}] Data-Out for unknown ITT 0x{:08x} (aborted?) - ignored", self.peer, itt);
                return Ok(true);
            }
        };
        if complete {
            if let Some(t) = self.pending.remove(&itt) {
                return self.finish_write(itt, t);
            }
        } else if need_r2t {
            self.send_r2t(itt)?;
        }
        Ok(true)
    }

    fn finish_write(&mut self, itt: u32, t: WriteTask) -> io::Result<bool> {
        let disk = match self.disk.clone() {
            Some(d) => d,
            None => return Ok(false),
        };
        let res = scsi::execute(&disk, t.lun, &t.cdb, &t.buf);
        if res.status == GOOD && scsi::is_write_op(t.cdb[0]) {
            self.bytes_written += t.expected as u64;
        }
        self.complete(itt, &t.lun_raw, false, t.expected, res, t.r2tsn)
    }

    /// Send the result: Data-In PDUs (status folded into the last one) or a SCSI Response.
    fn complete(&mut self, itt: u32, lun_raw: &[u8; 8], read: bool, expected: u32, res: ScsiResult, r2t_count: u32) -> io::Result<bool> {
        let expected = expected as usize;
        if res.status == GOOD && read && !res.data.is_empty() && expected > 0 {
            let total = res.data.len().min(expected);
            let (over, under, residual) = if res.data.len() > expected {
                (true, false, res.data.len() - expected)
            } else if res.data.len() < expected {
                (false, true, expected - res.data.len())
            } else {
                (false, false, 0)
            };
            let seg = (self.peer_mrdsl as usize).max(512);
            let burst = (self.max_burst as usize).max(seg);
            let mut off = 0usize;
            let mut data_sn = 0u32;
            while off < total {
                let burst_left = burst - (off % burst);
                let n = seg.min(total - off).min(burst_left);
                let last = off + n == total;
                let end_of_burst = (off + n) % burst == 0;
                let mut d = Pdu::new(op::DATA_IN);
                let mut f = 0u8;
                if last || end_of_burst {
                    f |= 0x80;
                }
                d.set_lun_raw(lun_raw);
                d.set_itt(itt);
                d.set_u32(20, 0xFFFF_FFFF);
                d.set_u32(36, data_sn);
                d.set_u32(40, off as u32);
                d.data = res.data[off..off + n].to_vec();
                if last {
                    f |= 0x01; // S: status included
                    if over {
                        f |= 0x04;
                    }
                    if under {
                        f |= 0x02;
                    }
                    d.bhs[3] = GOOD;
                    d.set_u32(44, residual as u32);
                    self.stamp(&mut d, true);
                } else {
                    self.stamp_no_status(&mut d);
                }
                d.bhs[1] = f;
                self.send(&d)?;
                off += n;
                data_sn += 1;
            }
            self.bytes_read += total as u64;
            return Ok(true);
        }

        let mut r = Pdu::new(op::SCSI_RESP);
        let mut f = 0x80u8;
        if res.status == GOOD && read && expected > 0 && res.data.is_empty() {
            f |= 0x02;
            r.set_u32(44, expected as u32);
        }
        r.bhs[1] = f;
        r.bhs[2] = 0x00; // command completed at target
        r.bhs[3] = res.status;
        r.set_itt(itt);
        r.set_u32(36, r2t_count);
        if !res.sense.is_empty() {
            let mut data = Vec::with_capacity(2 + res.sense.len());
            data.extend_from_slice(&(res.sense.len() as u16).to_be_bytes());
            data.extend_from_slice(&res.sense);
            debug!(
                "[{}]   -> CHECK CONDITION key=0x{:02x} asc=0x{:02x} ascq=0x{:02x}",
                self.peer,
                res.sense[2],
                res.sense[12],
                res.sense[13]
            );
            r.data = data;
        }
        self.stamp(&mut r, true);
        self.send(&r)?;
        Ok(true)
    }
}

fn short_name(iqn: &str) -> &str {
    iqn.rsplit(':').next().unwrap_or(iqn)
}

fn stage_name(s: u8) -> &'static str {
    match s {
        0 => "Security",
        1 => "Operational",
        3 => "FullFeature",
        _ => "?",
    }
}

fn cdb_len(op: u8) -> usize {
    match op >> 5 {
        0 => 6,
        1 | 2 => 10,
        4 => 16,
        5 => 12,
        _ => 16,
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect::<Vec<_>>().join(" ")
}

pub fn human(n: u64) -> String {
    let n = n as f64;
    if n >= 1073741824.0 {
        format!("{:.2} GB", n / 1073741824.0)
    } else if n >= 1048576.0 {
        format!("{:.2} MB", n / 1048576.0)
    } else if n >= 1024.0 {
        format!("{:.1} KB", n / 1024.0)
    } else {
        format!("{} B", n)
    }
}
