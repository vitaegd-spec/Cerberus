//! The in-process iSCSI target: one TCP listener on 127.0.0.1, many targets.
//! Each mounted image is registered under its own IQN.

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::conn::{Conn, TargetConfig};
use crate::disk::Disk;

pub struct Registry {
    map: Mutex<HashMap<String, Arc<Disk>>>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry { map: Mutex::new(HashMap::new()) }
    }
    pub fn add(&self, iqn: &str, disk: Arc<Disk>) {
        if let Ok(mut m) = self.map.lock() {
            m.insert(iqn.to_string(), disk);
        }
    }
    pub fn remove(&self, iqn: &str) -> Option<Arc<Disk>> {
        self.map.lock().ok().and_then(|mut m| m.remove(iqn))
    }
    pub fn get(&self, iqn: &str) -> Option<Arc<Disk>> {
        self.map.lock().ok().and_then(|m| m.get(iqn).cloned())
    }
    pub fn list(&self) -> Vec<String> {
        let mut v: Vec<String> = self.map.lock().map(|m| m.keys().cloned().collect()).unwrap_or_default();
        v.sort();
        v
    }
}

pub fn start(registry: Arc<Registry>, port: u16) -> Result<(), String> {
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| {
        format!(
            "cannot listen on 127.0.0.1:{} - {} (is the Phase 0 cerberus-poc or another iSCSI target still running?)",
            port, e
        )
    })?;
    let cfg = Arc::new(TargetConfig { registry, portal_tag: 1 });
    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(s) => {
                    let c = cfg.clone();
                    thread::spawn(move || Conn::new(s, c).run());
                }
                Err(e) => warn!("accept failed: {}", e),
            }
        }
    });
    Ok(())
}
