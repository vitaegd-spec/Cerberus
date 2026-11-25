//! Text key=value helpers used by Login and Text PDUs.

pub fn parse(data: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for part in data.split(|b| *b == 0) {
        if part.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(part).to_string();
        match s.find('=') {
            Some(i) => out.push((s[..i].to_string(), s[i + 1..].to_string())),
            None => out.push((s, String::new())),
        }
    }
    out
}

pub fn encode(keys: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (k, v) in keys {
        out.extend_from_slice(k.as_bytes());
        out.push(b'=');
        out.extend_from_slice(v.as_bytes());
        out.push(0);
    }
    out
}

pub fn render(keys: &[(String, String)]) -> String {
    if keys.is_empty() {
        return "(no keys)".to_string();
    }
    keys.iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect::<Vec<_>>()
        .join("  ")
}

pub fn kv(k: &str, v: &str) -> (String, String) {
    (k.to_string(), v.to_string())
}
