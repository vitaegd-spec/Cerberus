//! Hidden PowerShell bridge. Every Windows-side operation (Mount-DiskImage,
//! iSCSI Initiator, Disk/Volume cmdlets) runs here with no console window,
//! a timeout, and a single JSON line as the result.
//! The cmdlets are the same ones proven in Phase 0's scripts.
//!
//! The CERBERUS window itself runs non-elevated (so Explorer drag & drop works);
//! `run` forwards scripts to the elevated helper (broker.rs) in that case.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, Instant};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

use serde_json::Value;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(windows)]
fn hide(cmd: &mut Command) {
    cmd.creation_flags(CREATE_NO_WINDOW);
}
#[cfg(not(windows))]
fn hide(_cmd: &mut Command) {}

// Get-DiskLetters only gives drive letters to partitions whose file system
// Windows can actually read. Partitions it can't read (Linux ext4 / swap / LVM,
// etc.) are skipped - and any letter Windows auto-assigned to them is removed -
// so Explorer never shows "H:\ is unavailable". Skipped partitions are reported
// back to CERBERUS through $global:CerbSkipped.
const PRELUDE: &str = r#"
$ProgressPreference = 'SilentlyContinue'
$ErrorActionPreference = 'Stop'
try { [Console]::OutputEncoding = [Text.Encoding]::UTF8 } catch {}
$global:CerbSkipped = @()
function Out-Result($h) { $h | ConvertTo-Json -Compress -Depth 4 }
function Is-Letter($c) { ([string]$c) -match '^[A-Za-z]$' }
function Get-AllLetters {
    @(Get-Volume -ErrorAction SilentlyContinue | Where-Object { Is-Letter $_.DriveLetter } | ForEach-Object { ([string]$_.DriveLetter).ToUpper() })
}
function Get-CdLetters {
    @(Get-CimInstance Win32_LogicalDisk -Filter 'DriveType=5' -ErrorAction SilentlyContinue | ForEach-Object { ([string]$_.DeviceID).Substring(0, 1).ToUpper() })
}
function Format-Size([double]$b) {
    if ($b -ge 1GB) { return ('{0:N1} GB' -f ($b / 1GB)) }
    if ($b -ge 1MB) { return ('{0:N0} MB' -f ($b / 1MB)) }
    return ('{0:N0} KB' -f ($b / 1KB))
}
function Get-PartKind($p) {
    $g = ([string]$p.GptType).ToLower()
    $m = $p.MbrType
    if ($g -match '0fc63daf-8483-4772-8e79-3d69d8477de4' -or $m -eq 131) { return 'Linux ext2/3/4' }
    if ($g -match '0657fd6d-a4ab-43c4-84e5-0933c84b4f4f' -or $m -eq 130) { return 'Linux swap' }
    if ($g -match 'e6d6d379-f507-44c2-a23c-238f2a3df928' -or $m -eq 142) { return 'Linux LVM' }
    if ($g -match 'a19d880f-05fc-4d3b-a006-743f0f84911e' -or $m -eq 253) { return 'Linux RAID' }
    if ($g -match '21686148-6449-6e6f-744e-656564454649') { return 'BIOS boot' }
    if ($g -match '48465300-0000-11aa-aa11-00306543ecac' -or $g -match '7c3457ef-0000-11aa-aa11-00306543ecac' -or $m -eq 175) { return 'Apple HFS+/APFS' }
    return 'unknown file system'
}
function Get-DiskLetters([int]$num) {
    foreach ($p in @(Get-Partition -DiskNumber $num -ErrorAction SilentlyContinue)) {
        if ($p.Type -in @('Reserved', 'System', 'Recovery') -or $p.Size -le 0) { continue }
        $fs = ''
        try { $fs = [string]($p | Get-Volume -ErrorAction Stop).FileSystem } catch {}
        if ($fs -notmatch '^(NTFS|FAT|FAT12|FAT16|FAT32|exFAT|ReFS|UDF|CDFS)$') {
            if (Is-Letter $p.DriveLetter) {
                try { $p | Remove-PartitionAccessPath -AccessPath (([string]$p.DriveLetter) + ':\') -ErrorAction Stop } catch {}
            }
            $global:CerbSkipped += ('partition ' + $p.PartitionNumber + ': ' + (Get-PartKind $p) + ', ' + (Format-Size $p.Size))
            continue
        }
        if (-not (Is-Letter $p.DriveLetter)) {
            try {
                $p | Add-PartitionAccessPath -AssignDriveLetter -ErrorAction Stop | Out-Null
                $p = Get-Partition -DiskNumber $num -PartitionNumber $p.PartitionNumber
            } catch {}
        }
        if (Is-Letter $p.DriveLetter) { ([string]$p.DriveLetter).ToUpper() }
    }
}
function Flush-Letters($letters) {
    foreach ($l in @($letters)) { try { Write-VolumeCache -DriveLetter $l -ErrorAction Stop } catch {} }
}
"#;

/// Single-quoted PowerShell literal (PowerShell also treats curly quotes as quotes).
pub fn q(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if matches!(c, '\'' | '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}') {
            out.push(c);
        }
        out.push(c);
    }
    out.push('\'');
    out
}

pub fn ps_array(items: &[String]) -> String {
    if items.is_empty() {
        return "@()".to_string();
    }
    let parts: Vec<String> = items.iter().map(|s| q(s)).collect();
    format!("@({})", parts.join(", "))
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((bytes.len() + 2) / 3 * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = if chunk.len() > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if chunk.len() > 2 { chunk[2] as u32 } else { 0 };
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[((n >> 6) & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// True when this process has administrator rights (opening a physical drive
/// for reading only succeeds when elevated). Cached.
pub fn is_elevated() -> bool {
    static E: OnceLock<bool> = OnceLock::new();
    *E.get_or_init(|| std::fs::File::open(r"\\.\PHYSICALDRIVE0").is_ok())
}

/// Run a script body with administrator rights: directly when this process is
/// elevated, otherwise through the elevated helper.
pub fn run(body: &str, timeout_secs: u64) -> Result<Value, String> {
    if is_elevated() {
        run_local(body, timeout_secs)
    } else {
        crate::broker::call(body, timeout_secs)
    }
}

/// Run a script body in this process. Ok(json) when the script reported ok=true, Err(message) otherwise.
pub fn run_local(body: &str, timeout_secs: u64) -> Result<Value, String> {
    let mut script = String::new();
    script.push_str(PRELUDE);
    script.push_str("\ntry {\n");
    script.push_str(body);
    script.push_str("\n} catch {\n    Out-Result @{ ok = $false; error = $_.Exception.Message }\n}\n");
    let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    let encoded = b64(&utf16);

    let mut cmd = Command::new("powershell.exe");
    cmd.args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"])
        .arg(&encoded)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    hide(&mut cmd);
    let mut child = cmd.spawn().map_err(|e| format!("cannot start PowerShell: {}", e))?;

    let so = child.stdout.take();
    let se = child.stderr.take();
    let h_out = thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(mut s) = so {
            let _ = s.read_to_end(&mut b);
        }
        b
    });
    let h_err = thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(mut s) = se {
            let _ = s.read_to_end(&mut b);
        }
        b
    });

    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if start.elapsed() > Duration::from_secs(timeout_secs) {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("Windows operation timed out after {} seconds", timeout_secs));
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("PowerShell wait failed: {}", e)),
        }
    }
    let out = String::from_utf8_lossy(&h_out.join().unwrap_or_default()).to_string();
    let err = String::from_utf8_lossy(&h_err.join().unwrap_or_default()).to_string();

    let line = out.lines().rev().map(|l| l.trim()).find(|l| l.starts_with('{'));
    match line {
        Some(l) => {
            let v: Value = serde_json::from_str(l).map_err(|e| format!("unexpected PowerShell output ({}): {}", e, l))?;
            if v.get("ok").and_then(|o| o.as_bool()) == Some(true) {
                Ok(v)
            } else {
                let msg = v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown Windows error");
                Err(msg.trim().to_string())
            }
        }
        None => {
            let e: String = err.trim().chars().take(300).collect();
            Err(format!("PowerShell returned no result. {}", e))
        }
    }
}

fn string_list(v: &Value, key: &str) -> Vec<String> {
    match v.get(key) {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect(),
        Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
        _ => Vec::new(),
    }
}

/// Extract the "letters" array from a script result.
pub fn letters(v: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for s in string_list(v, "letters") {
        let s = s.trim().to_uppercase();
        if s.len() == 1 && s.chars().all(|c| c.is_ascii_alphabetic()) && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// Partitions that were skipped because Windows can't read their file system.
pub fn skipped(v: &Value) -> Vec<String> {
    string_list(v, "skipped")
}

// ------------------------------------------------------------------ scripts

const NATIVE_MOUNT: &str = r#"
$p = __PATH__
$isIso = __ISO__
$before = @(Get-AllLetters)
$img = Get-DiskImage -ImagePath $p__ST__ -ErrorAction SilentlyContinue
if ($img -and $img.Attached) { throw 'This image is already attached in Windows (mounted outside CERBERUS). Eject it in Explorer first.' }
Mount-DiskImage -ImagePath $p__ST__ -Access __ACCESS__ | Out-Null
$letters = @(); $num = $null; $style = ''
for ($i = 0; $i -lt 25; $i++) {
    Start-Sleep -Milliseconds 400
    $global:CerbSkipped = @()
    $img = Get-DiskImage -ImagePath $p__ST__
    if ($null -ne $img.Number) { $num = [int]$img.Number }
    if ($isIso) {
        $letters = @($img | Get-Volume -ErrorAction SilentlyContinue | Where-Object { Is-Letter $_.DriveLetter } | ForEach-Object { ([string]$_.DriveLetter).ToUpper() })
    } elseif ($null -ne $num) {
        $d = Get-Disk -Number $num
        $style = [string]$d.PartitionStyle
        if ($d.IsOffline) { Set-Disk -Number $num -IsOffline $false; continue }
        if ($style -ne 'RAW') { $letters = @(Get-DiskLetters $num) }
        elseif ($i -ge 6) { break }
    }
    if ($letters.Count -gt 0) { break }
    if ($global:CerbSkipped.Count -gt 0 -and $i -ge 4) { break }
}
if ($letters.Count -eq 0 -and $global:CerbSkipped.Count -eq 0) { $letters = @(Get-AllLetters | Where-Object { $before -notcontains $_ }) }
Out-Result @{ ok = $true; letters = $letters; disk = $num; style = $style; skipped = @($global:CerbSkipped) }
"#;

pub fn native_mount(path: &str, readonly: bool, storage: &str, iso: bool) -> String {
    NATIVE_MOUNT
        .replace("__PATH__", &q(path))
        .replace("__ISO__", if iso { "$true" } else { "$false" })
        .replace("__ST__", &storage_arg(storage))
        .replace("__ACCESS__", if readonly { "ReadOnly" } else { "ReadWrite" })
}

const NATIVE_DISMOUNT: &str = r#"
$p = __PATH__
Flush-Letters __LETTERS__
$img = Get-DiskImage -ImagePath $p__ST__ -ErrorAction SilentlyContinue
if ($img -and $img.Attached) { Dismount-DiskImage -ImagePath $p__ST__ | Out-Null }
Out-Result @{ ok = $true }
"#;

pub fn native_dismount(path: &str, storage: &str, letters: &[String]) -> String {
    NATIVE_DISMOUNT
        .replace("__PATH__", &q(path))
        .replace("__LETTERS__", &ps_array(letters))
        .replace("__ST__", &storage_arg(storage))
}

fn storage_arg(storage: &str) -> String {
    if storage.is_empty() {
        String::new()
    } else {
        format!(" -StorageType {}", storage)
    }
}

/// Shared first half of both connect scripts: service, portal, discovery, login.
const CONNECT_COMMON: &str = r#"
$iqn = __IQN__
$port = __PORT__
$svc = Get-Service MSiSCSI
if ($svc.StartType -ne 'Automatic') { Set-Service MSiSCSI -StartupType Automatic }
if ($svc.Status -ne 'Running') { Start-Service MSiSCSI; Start-Sleep -Seconds 2 }
$before = @(Get-AllLetters)
$beforeCd = @(Get-CdLetters)
$portal = @(Get-IscsiTargetPortal -ErrorAction SilentlyContinue | Where-Object { $_.TargetPortalAddress -eq '127.0.0.1' -and [int]$_.TargetPortalPortNumber -eq $port })
if ($portal.Count -gt 0) { Update-IscsiTargetPortal -TargetPortalAddress 127.0.0.1 -TargetPortalPortNumber $port | Out-Null }
else { New-IscsiTargetPortal -TargetPortalAddress 127.0.0.1 -TargetPortalPortNumber $port | Out-Null }
$t = Get-IscsiTarget | Where-Object NodeAddress -eq $iqn
if (-not $t) { throw 'Windows did not discover the CERBERUS target (SendTargets).' }
if (-not $t.IsConnected) { Connect-IscsiTarget -NodeAddress $iqn -TargetPortalAddress 127.0.0.1 -TargetPortalPortNumber $port | Out-Null }
"#;

const CONNECT_DISK_TAIL: &str = r#"
$disk = $null
for ($i = 0; $i -lt 27 -and -not $disk; $i++) {
    Start-Sleep -Milliseconds 750
    $s = Get-IscsiSession | Where-Object TargetNodeAddress -eq $iqn
    if ($s) { $disk = $s | Get-Disk -ErrorAction SilentlyContinue | Select-Object -First 1 }
}
if (-not $disk) { throw 'The iSCSI session is up but Windows did not surface a disk.' }
if ($disk.IsOffline) { Set-Disk -Number $disk.Number -IsOffline $false; Start-Sleep -Seconds 1; $disk = Get-Disk -Number $disk.Number }
$letters = @()
for ($i = 0; $i -lt 8; $i++) {
    $global:CerbSkipped = @()
    if ([string]$disk.PartitionStyle -ne 'RAW') { $letters = @(Get-DiskLetters $disk.Number) }
    if ($letters.Count -eq 0 -and $global:CerbSkipped.Count -eq 0) { $letters = @(Get-AllLetters | Where-Object { $before -notcontains $_ }) }
    if ($letters.Count -gt 0) { break }
    if ($global:CerbSkipped.Count -gt 0 -and $i -ge 3) { break }
    Start-Sleep -Milliseconds 500
    $disk = Get-Disk -Number $disk.Number
}
Out-Result @{ ok = $true; letters = $letters; disk = [int]$disk.Number; style = [string]$disk.PartitionStyle; skipped = @($global:CerbSkipped) }
"#;

/// CD/DVD devices never appear in Get-Disk: wait for a new CD-ROM drive letter.
const CONNECT_OPTICAL_TAIL: &str = r#"
$s = $null
for ($i = 0; $i -lt 20 -and -not $s; $i++) {
    $s = Get-IscsiSession | Where-Object TargetNodeAddress -eq $iqn
    if (-not $s) { Start-Sleep -Milliseconds 500 }
}
if (-not $s) { throw 'The iSCSI session for the virtual CD/DVD drive was not established.' }
$letters = @()
for ($i = 0; $i -lt 40; $i++) {
    Start-Sleep -Milliseconds 750
    $letters = @(Get-CdLetters | Where-Object { $beforeCd -notcontains $_ })
    if ($letters.Count -gt 0) { break }
}
Out-Result @{ ok = $true; letters = $letters; disk = $null; style = '' }
"#;

pub fn iscsi_connect(iqn: &str, port: u16, optical: bool) -> String {
    let tail = if optical { CONNECT_OPTICAL_TAIL } else { CONNECT_DISK_TAIL };
    format!("{}{}", CONNECT_COMMON, tail)
        .replace("__IQN__", &q(iqn))
        .replace("__PORT__", &port.to_string())
}

// Windows sometimes reports "The parameter is incorrect" even though the session
// was in fact logged out (seen in Phase 1 testing). Re-check before failing.
const ISCSI_DISCONNECT: &str = r#"
$iqn = __IQN__
Flush-Letters __LETTERS__
$sessions = @(Get-IscsiSession -ErrorAction SilentlyContinue | Where-Object TargetNodeAddress -eq $iqn)
foreach ($s in $sessions) {
    foreach ($d in @($s | Get-Disk -ErrorAction SilentlyContinue)) {
        Flush-Letters @(Get-Partition -DiskNumber $d.Number -ErrorAction SilentlyContinue | Where-Object { Is-Letter $_.DriveLetter } | ForEach-Object { [string]$_.DriveLetter })
        if (-not $d.IsOffline) { try { Set-Disk -Number $d.Number -IsOffline $true -ErrorAction Stop } catch {} }
    }
    Start-Sleep -Milliseconds 500
    if ($s.IsPersistent) { try { Unregister-IscsiSession -SessionIdentifier $s.SessionIdentifier -ErrorAction Stop } catch {} }
    try {
        Disconnect-IscsiTarget -SessionIdentifier $s.SessionIdentifier -Confirm:$false -ErrorAction Stop
    } catch {
        $msg = $_.Exception.Message
        Start-Sleep -Milliseconds 800
        $still = @(Get-IscsiSession -ErrorAction SilentlyContinue | Where-Object { $_.SessionIdentifier -eq $s.SessionIdentifier })
        if ($still.Count -gt 0) {
            throw ('Windows refused to release the drive - close any files or Explorer windows using it and try again. (' + $msg + ')')
        }
    }
}
Out-Result @{ ok = $true; sessions = $sessions.Count }
"#;

pub fn iscsi_disconnect(iqn: &str, letters: &[String]) -> String {
    ISCSI_DISCONNECT.replace("__IQN__", &q(iqn)).replace("__LETTERS__", &ps_array(letters))
}

const REMOVE_PORTAL: &str = r#"
$port = __PORT__
$portal = @(Get-IscsiTargetPortal -ErrorAction SilentlyContinue | Where-Object { $_.TargetPortalAddress -eq '127.0.0.1' -and [int]$_.TargetPortalPortNumber -eq $port })
if ($portal.Count -gt 0) { $portal | Remove-IscsiTargetPortal -Confirm:$false -ErrorAction SilentlyContinue }
Out-Result @{ ok = $true }
"#;

pub fn remove_portal(port: u16) -> String {
    REMOVE_PORTAL.replace("__PORT__", &port.to_string())
}

const STARTUP_CLEANUP: &str = r#"
$n = 0; $m = 0
foreach ($s in @(Get-IscsiSession -ErrorAction SilentlyContinue | Where-Object { $_.TargetNodeAddress -like '__PREFIX__*' })) {
    try { Disconnect-IscsiTarget -SessionIdentifier $s.SessionIdentifier -Confirm:$false -ErrorAction Stop; $n++ } catch {}
}
foreach ($item in __NATIVE__) {
    $parts = $item -split '\|', 2
    $p = $parts[0]; $st = $parts[1]
    try {
        if ($st) { $img = Get-DiskImage -ImagePath $p -StorageType $st -ErrorAction Stop } else { $img = Get-DiskImage -ImagePath $p -ErrorAction Stop }
        if ($img.Attached) {
            if ($st) { Dismount-DiskImage -ImagePath $p -StorageType $st | Out-Null } else { Dismount-DiskImage -ImagePath $p | Out-Null }
            $m++
        }
    } catch {}
}
Out-Result @{ ok = $true; sessions = $n; images = $m }
"#;

/// `native` items are "path|storageType" (storage type may be empty).
pub fn startup_cleanup(prefix: &str, native: &[String]) -> String {
    STARTUP_CLEANUP
        .replace("__PREFIX__", &prefix.replace('\'', "''"))
        .replace("__NATIVE__", &ps_array(native))
}

// --------------------------------------------------- initialize & format

/// Blank disk -> initialize (if RAW) -> one partition using all space -> format.
/// Refuses if the disk already holds data partitions (use Format instead).
const INIT_AND_FORMAT: &str = r#"
$num = __DISK__
$d = Get-Disk -Number $num
if ($d.IsReadOnly) { throw 'The disk is read-only. Unmount it and mount it again with "Mount read-only" unticked.' }
if ($d.IsOffline) { Set-Disk -Number $num -IsOffline $false; Start-Sleep -Seconds 1; $d = Get-Disk -Number $num }
if ([string]$d.PartitionStyle -eq 'RAW') {
    Initialize-Disk -Number $num -PartitionStyle __STYLE__ | Out-Null
    Start-Sleep -Milliseconds 800
} else {
    $data = @(Get-Partition -DiskNumber $num -ErrorAction SilentlyContinue | Where-Object { $_.Type -notin @('Reserved', 'Unknown') -and $_.Size -gt 0 })
    if ($data.Count -gt 0) { throw 'This disk already has partitions - use Format instead.' }
}
$part = New-Partition -DiskNumber $num -UseMaximumSize -AssignDriveLetter
Start-Sleep -Milliseconds 800
$part = Get-Partition -DiskNumber $num -PartitionNumber $part.PartitionNumber
$part | Format-Volume -FileSystem __FS__ -NewFileSystemLabel __LABEL__ -Confirm:$false -Force__FULL__ | Out-Null
Start-Sleep -Milliseconds 500
$letters = @(Get-DiskLetters $num)
Out-Result @{ ok = $true; letters = $letters; style = [string](Get-Disk -Number $num).PartitionStyle; skipped = @($global:CerbSkipped) }
"#;

pub fn init_and_format(disk: i64, style: &str, fs: &str, label: &str, full: bool) -> String {
    INIT_AND_FORMAT
        .replace("__DISK__", &disk.to_string())
        .replace("__STYLE__", style)
        .replace("__FS__", fs)
        .replace("__LABEL__", &q(label))
        .replace("__FULL__", if full { " -Full" } else { "" })
}

/// Reformat one existing volume (by drive letter). Erases everything on it.
const FORMAT_VOLUME: &str = r#"
$l = __LETTER__
$v = Get-Volume -DriveLetter $l -ErrorAction Stop
Flush-Letters @($l)
Format-Volume -DriveLetter $l -FileSystem __FS__ -NewFileSystemLabel __LABEL__ -Confirm:$false -Force__FULL__ | Out-Null
Out-Result @{ ok = $true; letters = @($l) }
"#;

pub fn format_volume(letter: &str, fs: &str, label: &str, full: bool) -> String {
    FORMAT_VOLUME
        .replace("__LETTER__", &q(letter))
        .replace("__FS__", fs)
        .replace("__LABEL__", &q(label))
        .replace("__FULL__", if full { " -Full" } else { "" })
}

// --------------------------------------------------- Phase 5: archives

/// Enable Windows' built-in Projected File System feature (used for archives).
const ENABLE_PROJFS: &str = r#"
$f = Get-WindowsOptionalFeature -Online -FeatureName Client-ProjFS
if ([string]$f.State -ne 'Enabled') {
    $r = Enable-WindowsOptionalFeature -Online -FeatureName Client-ProjFS -NoRestart -All
    Out-Result @{ ok = $true; changed = $true; restart = [bool]$r.RestartNeeded }
} else {
    Out-Result @{ ok = $true; changed = $false; restart = $false }
}
"#;

pub fn enable_projfs() -> String {
    ENABLE_PROJFS.to_string()
}

/// Create (or remove) a drive letter in the *signed-in user's* normal session.
/// CERBERUS runs elevated, and under UAC an elevated process has its own private
/// set of drive letters that Explorer can't see. A one-shot scheduled task runs
/// `subst` as the interactive user with a standard (non-elevated) token, so the
/// letter lands where Explorer looks. Returns subst's exit code (0 = success).
const USER_SUBST: &str = r#"
$l = __LETTER__
$p = __PATH__
$remove = __REMOVE__
$user = (Get-CimInstance Win32_ComputerSystem).UserName
if (-not $user) { throw 'No signed-in user was found to create the drive letter for.' }
$name = 'CERBERUS-subst-' + $l
if ($remove) { $argLine = "$($l): /D" } else { $argLine = "$($l): `"$p`"" }
$act = New-ScheduledTaskAction -Execute "$env:SystemRoot\System32\subst.exe" -Argument $argLine
$pr = New-ScheduledTaskPrincipal -UserId $user -LogonType Interactive -RunLevel Limited
Unregister-ScheduledTask -TaskName $name -Confirm:$false -ErrorAction SilentlyContinue
Register-ScheduledTask -TaskName $name -Action $act -Principal $pr -Force | Out-Null
$res = $null
try {
    $started = Get-Date
    Start-ScheduledTask -TaskName $name
    for ($i = 0; $i -lt 60; $i++) {
        Start-Sleep -Milliseconds 250
        $t = Get-ScheduledTask -TaskName $name
        if ([string]$t.State -eq 'Running') { continue }
        $info = Get-ScheduledTaskInfo -TaskName $name
        if ($info.LastRunTime -ge $started.AddSeconds(-5) -and $info.LastTaskResult -ne 267011 -and $info.LastTaskResult -ne 267009) {
            $res = [int]$info.LastTaskResult
            break
        }
    }
} finally {
    Unregister-ScheduledTask -TaskName $name -Confirm:$false -ErrorAction SilentlyContinue
}
if ($null -eq $res) { throw 'The drive-letter helper task did not run.' }
Out-Result @{ ok = $true; code = $res }
"#;

pub fn user_subst(letter: &str, path: &str, remove: bool) -> String {
    USER_SUBST
        .replace("__LETTER__", &q(letter))
        .replace("__PATH__", &q(path))
        .replace("__REMOVE__", if remove { "$true" } else { "$false" })
}

// --------------------------------------------------- Open in WSL

/// Attach Linux partitions of a mounted disk to WSL 2 (`wsl --mount`) and return
/// the Explorer paths (\\wsl.localhost\<distro>\mnt\wsl\PHYSICALDRIVE<n>p<p>).
const WSL_MOUNT: &str = r#"
$env:WSL_UTF8 = '1'
$wsl = Join-Path $env:SystemRoot 'System32\wsl.exe'
if (-not (Test-Path $wsl)) { throw 'WSL is not installed. Install it with "wsl --install", restart Windows, then try again.' }
$lx = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Lxss'
$distro = $null
try {
    $g = (Get-ItemProperty $lx -ErrorAction Stop).DefaultDistribution
    if ($g) { $distro = (Get-ItemProperty (Join-Path $lx $g) -ErrorAction Stop).DistributionName }
} catch {}
if (-not $distro) { throw 'WSL has no Linux distribution installed. Run "wsl --install -d Ubuntu", finish its setup, then try again.' }
$dev = '\\.\PHYSICALDRIVE' + __DISK__
$paths = @()
foreach ($n in __PARTS__) {
    $wargs = @('--mount', $dev, '--partition', [string]$n)
    __RO__
    $out = (& $wsl @wargs 2>&1 | Out-String)
    $code = $LASTEXITCODE
    if ($code -ne 0 -and $out -notmatch 'ALREADY_MOUNTED|already mounted|already attached') {
        throw ('wsl --mount failed for partition ' + $n + ': ' + $out.Trim())
    }
    $paths += ('\\wsl.localhost\' + $distro + '\mnt\wsl\PHYSICALDRIVE' + __DISK__ + 'p' + $n)
}
Out-Result @{ ok = $true; paths = $paths; distro = $distro }
"#;

pub fn wsl_mount(disk: i64, parts: &[u32], readonly: bool) -> String {
    let list: Vec<String> = parts.iter().map(|p| p.to_string()).collect();
    WSL_MOUNT
        .replace("__DISK__", &disk.to_string())
        .replace("__PARTS__", &format!("@({})", list.join(", ")))
        .replace("__RO__", if readonly { "$wargs += @('--options', 'ro')" } else { "" })
}

const WSL_UNMOUNT: &str = r#"
$env:WSL_UTF8 = '1'
$wsl = Join-Path $env:SystemRoot 'System32\wsl.exe'
if (Test-Path $wsl) { & $wsl --unmount ('\\.\PHYSICALDRIVE' + __DISK__) 2>&1 | Out-Null }
Out-Result @{ ok = $true }
"#;

pub fn wsl_unmount(disk: i64) -> String {
    WSL_UNMOUNT.replace("__DISK__", &disk.to_string())
}

/// Paths returned by `wsl_mount`.
pub fn paths(v: &Value) -> Vec<String> {
    string_list(v, "paths")
}
