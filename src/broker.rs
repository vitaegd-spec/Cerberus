//! Administrator helper ("broker").
//!
//! The CERBERUS window runs as a normal (non-elevated) program. That is what
//! makes drag & drop from Explorer work: Windows blocks drag & drop from a normal
//! program into an elevated one (UIPI). The few operations that really need
//! administrator rights (Mount-DiskImage, the iSCSI Initiator, disk and volume
//! cmdlets, enabling Windows features) are sent to a second, elevated copy of
//! cerberus.exe started once with a single UAC prompt.
//!
//! The helper connects back to the UI over 127.0.0.1, proves itself with a
//! one-time token, then executes PowerShell script bodies and returns their JSON
//! result. It exits by itself as soon as the UI process goes away.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde_json::{json, Value};

use crate::ps;

static CONN: Mutex<Option<TcpStream>> = Mutex::new(None);
static STARTING: Mutex<()> = Mutex::new(());

const MAX_FRAME: usize = 64 << 20;
const HELLO_TIMEOUT: Duration = Duration::from_secs(90);

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn write_frame(s: &mut TcpStream, b: &[u8]) -> io::Result<()> {
    s.write_all(&(b.len() as u32).to_le_bytes())?;
    s.write_all(b)?;
    s.flush()
}

fn read_frame(s: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut l = [0u8; 4];
    s.read_exact(&mut l)?;
    let n = u32::from_le_bytes(l) as usize;
    if n > MAX_FRAME {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "frame too large"));
    }
    let mut b = vec![0u8; n];
    s.read_exact(&mut b)?;
    Ok(b)
}

/// True while an elevated helper is connected.
pub fn connected() -> bool {
    CONN.lock().map(|g| g.is_some()).unwrap_or(false)
}

fn new_token() -> String {
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let seed = format!("{}-{}-{:p}", t, std::process::id(), &CONN);
    let h = crate::disk::fnv1a(seed.as_bytes());
    format!("{:016x}{:016x}", h, h.rotate_left(29) ^ (t as u64))
}

/// Launch the elevated helper (one UAC prompt) and wait for it to connect.
pub fn ensure() -> Result<(), String> {
    if connected() {
        return Ok(());
    }
    let _starting = STARTING.lock().map_err(|_| "internal lock error".to_string())?;
    if connected() {
        return Ok(());
    }
    let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|e| format!("cannot start the admin helper channel: {}", e))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let token = new_token();
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe_s = exe.to_string_lossy().to_string();
    let dir = exe.parent().map(|p| p.to_string_lossy().to_string()).unwrap_or_else(|| ".".into());

    info!("Requesting administrator permission (UAC) for the CERBERUS helper…");
    let launch = format!(
        "$ErrorActionPreference='Stop'; Start-Process -FilePath {} -ArgumentList {} -WorkingDirectory {} -Verb RunAs",
        ps::q(&exe_s),
        ps::q(&format!("--broker {} {}", port, token)),
        ps::q(&dir)
    );
    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command"])
        .arg(&launch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(CREATE_NO_WINDOW);
    let status = cmd.status().map_err(|e| format!("cannot start PowerShell: {}", e))?;
    if !status.success() {
        return Err("Administrator permission was declined (UAC). Disk and disc images need it - mount the image again to get the permission prompt again. (Archives don't need it.)".into());
    }

    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let start = Instant::now();
    while start.elapsed() < HELLO_TIMEOUT {
        match listener.accept() {
            Ok((mut s, _)) => {
                let _ = s.set_nonblocking(false);
                let _ = s.set_nodelay(true);
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                let ok = read_frame(&mut s).map(|b| b == token.as_bytes()).unwrap_or(false);
                if !ok {
                    continue; // not our helper - keep waiting
                }
                let _ = s.set_read_timeout(None);
                if let Ok(mut g) = CONN.lock() {
                    *g = Some(s);
                }
                info!("Administrator helper is running");
                return Ok(());
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::sleep(Duration::from_millis(100)),
            Err(e) => return Err(format!("admin helper channel failed: {}", e)),
        }
    }
    Err("The administrator helper did not start in time.".into())
}

/// Run a PowerShell script body in the elevated helper.
pub fn call(body: &str, timeout_secs: u64) -> Result<Value, String> {
    ensure()?;
    let mut g = CONN.lock().map_err(|_| "internal lock error".to_string())?;
    let s = g.as_mut().ok_or("the administrator helper is not connected")?;
    let mut req = Vec::with_capacity(8 + body.len());
    req.extend_from_slice(&timeout_secs.to_le_bytes());
    req.extend_from_slice(body.as_bytes());
    let reply = write_frame(s, &req).and_then(|_| read_frame(s));
    match reply {
        Ok(b) => {
            let v: Value = serde_json::from_slice(&b).map_err(|e| format!("bad reply from the admin helper: {}", e))?;
            if let Some(e) = v.get("e").and_then(|e| e.as_str()) {
                Err(e.to_string())
            } else {
                Ok(v.get("r").cloned().unwrap_or(Value::Null))
            }
        }
        Err(e) => {
            *g = None;
            warn!("Lost the connection to the administrator helper: {}", e);
            Err(format!("lost the connection to the administrator helper ({}) - try again", e))
        }
    }
}

/// Entry point of the elevated helper process (`cerberus.exe --broker <port> <token>`).
pub fn serve(port: u16, token: &str) {
    let mut s = match TcpStream::connect(("127.0.0.1", port)) {
        Ok(s) => s,
        Err(_) => return,
    };
    let _ = s.set_nodelay(true);
    if write_frame(&mut s, token.as_bytes()).is_err() {
        return;
    }
    loop {
        let req = match read_frame(&mut s) {
            Ok(r) => r,
            Err(_) => return, // UI closed - helper exits
        };
        if req.len() < 8 {
            return;
        }
        let mut t = [0u8; 8];
        t.copy_from_slice(&req[..8]);
        let timeout = u64::from_le_bytes(t);
        let body = String::from_utf8_lossy(&req[8..]).to_string();
        let resp = match ps::run_local(&body, timeout) {
            Ok(v) => json!({ "r": v }),
            Err(e) => json!({ "e": e }),
        };
        if write_frame(&mut s, resp.to_string().as_bytes()).is_err() {
            return;
        }
    }
}
