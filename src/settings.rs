//! User settings (data\settings.json) and the Windows integration they control:
//!   * Start with Windows      - HKCU\...\Run  ("cerberus.exe" --tray)
//!   * Explorer context menu   - "Mount with CERBERUS" on every supported type
//!   * File associations       - double-click opens / mounts with CERBERUS
//! Everything is per-user (HKCU): no administrator rights are needed.

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::registry as reg;

pub const PROGID: &str = "CERBERUS.Image";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "CERBERUS";
const VERB: &str = "CERBERUS.Mount";
const PREVIOUS: &str = "CERBERUS.Previous";

/// Extensions offered for the context menu and file associations.
pub const EXTENSIONS: &[&str] = &[
    "iso", "vhd", "vhdx", "img", "ima", "flp", "raw", "vdi", "vmdk", "ova", "chd", "cue", "bin", "mds", "mdf", "ccd",
    "cdi", "nrg", "zip", "7z", "rar", "tar", "tgz", "gz", "cab", "001",
];

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub start_with_windows: bool,
    pub restore_mounts: bool,
    pub context_menu: bool,
    /// Extensions CERBERUS is registered to open on double-click.
    pub associations: Vec<String>,
    pub readonly_default: bool,
    /// After mounting from Explorer (right-click / double-click), open the drive.
    pub open_after_external_mount: bool,
    /// Archive drive letters are taken from this letter downwards.
    pub archive_letter: String,
    /// 0 = normal, 1 = debug, 2 = trace.
    pub log_level: u8,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            start_with_windows: false,
            restore_mounts: false,
            context_menu: false,
            associations: Vec::new(),
            readonly_default: true,
            open_after_external_mount: true,
            archive_letter: "Z".into(),
            log_level: 0,
        }
    }
}

impl Settings {
    /// Clean up values coming from the UI.
    pub fn normalized(mut self) -> Settings {
        let mut a: Vec<String> = self
            .associations
            .iter()
            .map(|e| e.trim().trim_start_matches('.').to_lowercase())
            .filter(|e| EXTENSIONS.contains(&e.as_str()))
            .collect();
        a.sort();
        a.dedup();
        self.associations = a;
        let l = self.archive_letter.trim().to_uppercase();
        self.archive_letter = match l.chars().next() {
            Some(c) if ('D'..='Z').contains(&c) => c.to_string(),
            _ => "Z".into(),
        };
        self.log_level = self.log_level.min(2);
        self
    }

    pub fn letter(&self) -> char {
        self.archive_letter.chars().next().unwrap_or('Z')
    }
}

fn file(base: &Path) -> std::path::PathBuf {
    base.join("data").join("settings.json")
}

pub fn load(base: &Path) -> Settings {
    fs::read_to_string(file(base))
        .ok()
        .and_then(|s| serde_json::from_str::<Settings>(&s).ok())
        .unwrap_or_default()
        .normalized()
}

pub fn save(base: &Path, s: &Settings) -> Result<(), String> {
    let _ = fs::create_dir_all(base.join("data"));
    let txt = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    fs::write(file(base), txt).map_err(|e| format!("cannot save settings: {}", e))
}

fn exe() -> String {
    std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "cerberus.exe".into())
}

fn mount_command() -> String {
    format!("\"{}\" --mount \"%1\"", exe())
}

fn icon() -> String {
    format!("\"{}\",0", exe())
}

// ------------------------------------------------------------ start with Windows

fn set_autostart(on: bool) -> Result<(), String> {
    if on {
        reg::set_str(RUN_KEY, Some(RUN_VALUE), &format!("\"{}\" --tray", exe()))
    } else {
        reg::delete_value(RUN_KEY, Some(RUN_VALUE))
    }
}

// ------------------------------------------------------------ context menu

fn verb_key(ext: &str) -> String {
    format!(r"Software\Classes\SystemFileAssociations\.{}\shell\{}", ext, VERB)
}

fn set_context_menu(on: bool) -> Result<(), String> {
    for ext in EXTENSIONS {
        let k = verb_key(ext);
        if on {
            reg::set_str(&k, None, "Mount with CERBERUS")?;
            reg::set_str(&k, Some("Icon"), &icon())?;
            reg::set_str(&format!(r"{}\command", k), None, &mount_command())?;
        } else {
            reg::delete_tree(&k)?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------ file associations

fn write_progid() -> Result<(), String> {
    let base = format!(r"Software\Classes\{}", PROGID);
    reg::set_str(&base, None, "Image or archive (mounted by CERBERUS)")?;
    reg::set_str(&format!(r"{}\DefaultIcon", base), None, &icon())?;
    reg::set_str(&format!(r"{}\shell", base), None, "open")?;
    reg::set_str(&format!(r"{}\shell\open", base), None, "Mount with CERBERUS")?;
    reg::set_str(&format!(r"{}\shell\open\command", base), None, &mount_command())?;
    Ok(())
}

fn associate(ext: &str) -> Result<(), String> {
    let k = format!(r"Software\Classes\.{}", ext);
    reg::set_none(&format!(r"{}\OpenWithProgids", k), PROGID)?;
    let current = reg::get_str(&k, None);
    if current.as_deref() != Some(PROGID) {
        if let Some(prev) = current {
            reg::set_str(&k, Some(PREVIOUS), &prev)?;
        }
        reg::set_str(&k, None, PROGID)?;
    }
    Ok(())
}

fn dissociate(ext: &str) -> Result<(), String> {
    let k = format!(r"Software\Classes\.{}", ext);
    reg::delete_value(&format!(r"{}\OpenWithProgids", k), Some(PROGID))?;
    if reg::get_str(&k, None).as_deref() == Some(PROGID) {
        match reg::get_str(&k, Some(PREVIOUS)) {
            Some(prev) => reg::set_str(&k, None, &prev)?,
            None => reg::delete_value(&k, None)?,
        }
    }
    reg::delete_value(&k, Some(PREVIOUS))?;
    Ok(())
}

/// Bring the registry in line with `new` (only what changed relative to `old`;
/// `force` rewrites everything that is enabled - used at startup so the
/// entries follow the exe if the CERBERUS folder was moved).
pub fn apply(old: &Settings, new: &Settings, force: bool) -> Result<(), String> {
    if force || old.start_with_windows != new.start_with_windows {
        if force && !new.start_with_windows {
            // nothing to refresh
        } else {
            set_autostart(new.start_with_windows)?;
        }
    }
    if force || old.context_menu != new.context_menu {
        if !(force && !new.context_menu) {
            set_context_menu(new.context_menu)?;
        }
    }
    let mut assoc_changed = false;
    if !new.associations.is_empty() && (force || old.associations.is_empty() || old.associations != new.associations) {
        write_progid()?;
    }
    for ext in &old.associations {
        if !new.associations.contains(ext) {
            dissociate(ext)?;
            assoc_changed = true;
        }
    }
    for ext in &new.associations {
        if force || !old.associations.contains(ext) {
            associate(ext)?;
            assoc_changed = true;
        }
    }
    if new.associations.is_empty() && !old.associations.is_empty() {
        reg::delete_tree(&format!(r"Software\Classes\{}", PROGID))?;
    }
    if assoc_changed || old.context_menu != new.context_menu {
        reg::notify_assoc();
    }
    Ok(())
}
