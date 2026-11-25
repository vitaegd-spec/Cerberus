//! Archive readers (ZIP, 7Z, RAR, TAR, TAR.GZ/TGZ, GZ, CAB) including split /
//! multi-volume archives:
//!   * x.7z.001, x.zip.001, x.tar.001 ... (plain byte-split volumes, e.g. from 7-Zip)
//!   * x.part1.rar, x.part2.rar ... and old-style x.rar, x.r00, x.r01 ...
//! `list` returns the archive's table of contents; `stream` delivers the bytes
//! of one entry (from a given offset) to a sink. Nothing is ever extracted to
//! disk by CERBERUS - Windows' Projected File System pulls file data on demand.

use std::collections::HashSet;
use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Zip,
    SevenZ,
    Rar,
    Tar,
    TarGz,
    Gz,
    Cab,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Zip => "ZIP archive",
            Kind::SevenZ => "7-Zip archive",
            Kind::Rar => "RAR archive",
            Kind::Tar => "TAR archive",
            Kind::TarGz => "TAR.GZ archive",
            Kind::Gz => "GZip file",
            Kind::Cab => "Cabinet archive",
        }
    }
}

fn lower_ext(p: &Path) -> String {
    p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn is_volume_number(ext: &str) -> bool {
    ext.len() == 3 && ext.chars().all(|c| c.is_ascii_digit())
}

/// Archive type from the file name (.tar.gz is recognised from the stem,
/// x.7z.001 from the extension before the volume number).
pub fn kind_for(path: &Path, ext: &str) -> Option<Kind> {
    match ext {
        "zip" => Some(Kind::Zip),
        "7z" => Some(Kind::SevenZ),
        "rar" => Some(Kind::Rar),
        "tar" => Some(Kind::Tar),
        "tgz" => Some(Kind::TarGz),
        "cab" => Some(Kind::Cab),
        "gz" => {
            let stem = path.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
            if stem.ends_with(".tar") {
                Some(Kind::TarGz)
            } else {
                Some(Kind::Gz)
            }
        }
        "001" => {
            let inner = path.file_stem().map(|s| lower_ext(Path::new(s))).unwrap_or_default();
            match inner.as_str() {
                "7z" => Some(Kind::SevenZ),
                "zip" => Some(Kind::Zip),
                "tar" => Some(Kind::Tar),
                "tgz" => Some(Kind::TarGz),
                _ => None,
            }
        }
        _ => None,
    }
}

// ------------------------------------------------------------------ volumes

/// If `path` is a later volume of a split archive, return the first volume
/// (x.7z.003 -> x.7z.001, x.part2.rar -> x.part1.rar, x.r05 -> x.rar).
pub fn first_volume(path: &Path) -> Option<PathBuf> {
    let ext = lower_ext(path);
    let name = path.file_name()?.to_string_lossy().to_string();
    if is_volume_number(&ext) && ext != "001" {
        let p = path.with_extension("001");
        return if p.exists() { Some(p) } else { None };
    }
    if ext.len() == 3 && ext.starts_with('r') && ext[1..].chars().all(|c| c.is_ascii_digit()) {
        for e in ["rar", "RAR"] {
            let p = path.with_extension(e);
            if p.exists() {
                return Some(p);
            }
        }
        return None;
    }
    if ext == "rar" {
        let lower = name.to_lowercase();
        let stem_end = lower.len() - 4; // without ".rar"
        let stem = &lower[..stem_end];
        if let Some(pos) = stem.rfind(".part") {
            let digits = &stem[pos + 5..];
            if !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
                let n: u32 = digits.parse().ok()?;
                if n > 1 {
                    let width = digits.len();
                    let first = format!("{}{:0width$}{}", &name[..pos + 5], 1, &name[stem_end..], width = width);
                    let p = path.with_file_name(first);
                    return if p.exists() { Some(p) } else { None };
                }
            }
        }
    }
    None
}

/// All volumes of a byte-split archive (x.7z.001, x.7z.002, ...). Single file otherwise.
pub fn volumes(path: &Path) -> Vec<PathBuf> {
    if lower_ext(path) != "001" {
        return vec![path.to_path_buf()];
    }
    let full = path.to_string_lossy().to_string();
    let base = &full[..full.len() - 4];
    let mut v = Vec::new();
    for i in 1..=9999u32 {
        let p = PathBuf::from(format!("{}.{:03}", base, i));
        if !p.exists() {
            break;
        }
        v.push(p);
    }
    if v.is_empty() {
        v.push(path.to_path_buf());
    }
    v
}

/// Number of volumes (byte-split or RAR multi-volume) - for display only.
pub fn volume_count(path: &Path) -> usize {
    let ext = lower_ext(path);
    if ext == "001" {
        return volumes(path).len();
    }
    if ext != "rar" {
        return 1;
    }
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    if let Some(pos) = name.rfind(".part") {
        let prefix = name[..pos + 5].to_string();
        return std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| {
                        let n = e.file_name().to_string_lossy().to_lowercase();
                        n.starts_with(&prefix) && n.ends_with(".rar")
                    })
                    .count()
            })
            .unwrap_or(1)
            .max(1);
    }
    let mut n = 1;
    for i in 0..100 {
        if path.with_extension(format!("r{:02}", i)).exists() || path.with_extension(format!("R{:02}", i)).exists() {
            n += 1;
        } else {
            break;
        }
    }
    n
}

/// Total size of all volumes.
pub fn volume_set_size(path: &Path) -> u64 {
    volumes(path).iter().filter_map(|p| std::fs::metadata(p).ok()).map(|m| m.len()).sum()
}

/// Byte-split volumes presented as one continuous file.
pub struct MultiFile {
    parts: Vec<(File, u64, u64)>, // file, start offset, length
    total: u64,
    pos: u64,
}

impl MultiFile {
    pub fn open(paths: &[PathBuf]) -> io::Result<MultiFile> {
        let mut parts = Vec::with_capacity(paths.len());
        let mut start = 0u64;
        for p in paths {
            let f = File::open(p)?;
            let len = f.metadata()?.len();
            parts.push((f, start, len));
            start += len;
        }
        Ok(MultiFile { parts, total: start, pos: 0 })
    }

    pub fn len(&self) -> u64 {
        self.total
    }
}

impl Read for MultiFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() || self.pos >= self.total {
            return Ok(0);
        }
        let pos = self.pos;
        let part = self.parts.iter_mut().find(|(_, s, l)| pos >= *s && pos < *s + *l);
        let (f, s, l) = match part {
            Some((f, s, l)) => (f, *s, *l),
            None => return Ok(0),
        };
        let within = pos - s;
        let n = ((l - within) as usize).min(buf.len());
        f.seek(SeekFrom::Start(within))?;
        let got = f.read(&mut buf[..n])?;
        self.pos += got as u64;
        Ok(got)
    }
}

impl Seek for MultiFile {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(o) => o as i128,
            SeekFrom::End(o) => self.total as i128 + o as i128,
            SeekFrom::Current(o) => self.pos as i128 + o as i128,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

trait ReadSeek: Read + Seek {}
impl<T: Read + Seek> ReadSeek for T {}

/// The archive's bytes: one file, or all byte-split volumes joined.
fn open_any(path: &Path) -> Result<(Box<dyn ReadSeek>, u64), String> {
    if lower_ext(path) == "001" {
        let mf = MultiFile::open(&volumes(path)).map_err(|e| format!("cannot open the archive volumes: {}", e))?;
        let len = mf.len();
        return Ok((Box::new(BufReader::with_capacity(1 << 20, mf)), len));
    }
    let f = File::open(path).map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
    let len = f.metadata().map_err(es)?.len();
    Ok((Box::new(BufReader::with_capacity(1 << 20, f)), len))
}

/// One entry as stored in the archive.
pub struct RawEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    /// Position of the entry in the archive's own order (used to find it again).
    pub idx: usize,
}

fn es<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn open_buf(path: &Path) -> Result<BufReader<File>, String> {
    let f = File::open(path).map_err(|e| format!("cannot open {}: {}", path.display(), e))?;
    Ok(BufReader::with_capacity(1 << 20, f))
}

// ------------------------------------------------------------------ listing

pub fn list(path: &Path, kind: Kind) -> Result<Vec<RawEntry>, String> {
    match kind {
        Kind::Zip => list_zip(path),
        Kind::SevenZ => list_7z(path),
        Kind::Rar => list_rar(path),
        Kind::Tar => tar_list(open_any(path)?.0),
        Kind::TarGz => tar_list(MultiGzDecoder::new(open_any(path)?.0)),
        Kind::Gz => list_gz(path),
        Kind::Cab => list_cab(path),
    }
}

fn list_zip(path: &Path) -> Result<Vec<RawEntry>, String> {
    let (r, _) = open_any(path)?;
    let mut z = zip::ZipArchive::new(r).map_err(|e| format!("not a valid ZIP archive: {}", e))?;
    let mut v = Vec::with_capacity(z.len());
    for i in 0..z.len() {
        let f = z.by_index_raw(i).map_err(|e| format!("ZIP entry {}: {}", i, e))?;
        v.push(RawEntry { name: f.name().to_string(), size: f.size(), is_dir: f.is_dir(), idx: i });
    }
    Ok(v)
}

fn sz_entries<R: Read + Seek>(r: &sevenz_rust::SevenZReader<R>) -> Vec<RawEntry> {
    r.archive()
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| RawEntry { name: f.name().to_string(), size: f.size(), is_dir: f.is_directory(), idx: i })
        .collect()
}

fn list_7z(path: &Path) -> Result<Vec<RawEntry>, String> {
    if lower_ext(path) == "001" {
        let mf = MultiFile::open(&volumes(path)).map_err(es)?;
        let len = mf.len();
        let r = sevenz_rust::SevenZReader::new(BufReader::with_capacity(1 << 20, mf), len, sevenz_rust::Password::empty())
            .map_err(|e| format!("cannot open split 7z archive (encrypted archives are not supported): {:?}", e))?;
        return Ok(sz_entries(&r));
    }
    let r = sevenz_rust::SevenZReader::open(path, sevenz_rust::Password::empty())
        .map_err(|e| format!("cannot open 7z archive (encrypted archives are not supported): {:?}", e))?;
    Ok(sz_entries(&r))
}

fn list_rar(path: &Path) -> Result<Vec<RawEntry>, String> {
    // Opening the first volume lets UnRAR follow the other volumes itself.
    // Files that span volumes are reported once per volume - keep the first.
    let a = unrar::Archive::new(path)
        .open_for_listing()
        .map_err(|e| format!("cannot open RAR archive: {:?}", e))?;
    let mut v = Vec::new();
    let mut seen = HashSet::new();
    for (i, h) in a.enumerate() {
        let h = h.map_err(|e| format!("RAR entry {}: {:?}", i, e))?;
        let name = h.filename.to_string_lossy().to_string();
        if !seen.insert(name.clone()) {
            continue;
        }
        v.push(RawEntry { name, size: h.unpacked_size, is_dir: h.is_directory(), idx: i });
    }
    Ok(v)
}

fn tar_list<R: Read>(r: R) -> Result<Vec<RawEntry>, String> {
    let mut a = tar::Archive::new(r);
    let mut v = Vec::new();
    for (i, ent) in a.entries().map_err(|e| format!("not a valid TAR archive: {}", e))?.enumerate() {
        let ent = ent.map_err(|e| format!("TAR entry {}: {}", i, e))?;
        let ty = ent.header().entry_type();
        if !(ty.is_file() || ty.is_dir()) {
            continue; // links, devices, PAX headers...
        }
        let name = ent.path().map_err(es)?.to_string_lossy().to_string();
        v.push(RawEntry { name, size: ent.size(), is_dir: ty.is_dir(), idx: i });
    }
    Ok(v)
}

fn list_gz(path: &Path) -> Result<Vec<RawEntry>, String> {
    // A plain .gz holds one file; its size (mod 4 GB) is the ISIZE trailer.
    let mut f = File::open(path).map_err(es)?;
    let len = f.metadata().map_err(es)?.len();
    if len < 18 {
        return Err("not a valid GZip file".into());
    }
    let mut magic = [0u8; 2];
    f.read_exact(&mut magic).map_err(es)?;
    if magic != [0x1f, 0x8b] {
        return Err("not a valid GZip file".into());
    }
    f.seek(SeekFrom::End(-4)).map_err(es)?;
    let mut t = [0u8; 4];
    f.read_exact(&mut t).map_err(es)?;
    let size = u32::from_le_bytes(t) as u64;
    let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "data".into());
    Ok(vec![RawEntry { name, size, is_dir: false, idx: 0 }])
}

fn list_cab(path: &Path) -> Result<Vec<RawEntry>, String> {
    let cab = cab::Cabinet::new(open_buf(path)?).map_err(|e| format!("not a valid Cabinet archive: {}", e))?;
    let mut v = Vec::new();
    let mut i = 0;
    for folder in cab.folder_entries() {
        for file in folder.file_entries() {
            v.push(RawEntry { name: file.name().to_string(), size: file.uncompressed_size() as u64, is_dir: false, idx: i });
            i += 1;
        }
    }
    Ok(v)
}

// ------------------------------------------------------------------ streaming

/// Copy `len` bytes starting at `offset` of the entry's data into `sink`.
fn pump(r: &mut dyn Read, offset: u64, len: u64, sink: &mut dyn FnMut(&[u8]) -> Result<(), String>) -> Result<(), String> {
    let mut buf = vec![0u8; 1 << 20];
    let mut skip = offset;
    while skip > 0 {
        let want = skip.min(buf.len() as u64) as usize;
        let n = read_some(r, &mut buf[..want])?;
        if n == 0 {
            return Err("archive data ended early".into());
        }
        skip -= n as u64;
    }
    let mut left = len;
    while left > 0 {
        let want = left.min(buf.len() as u64) as usize;
        let n = read_some(r, &mut buf[..want])?;
        if n == 0 {
            return Err("archive data ended early".into());
        }
        sink(&buf[..n])?;
        left -= n as u64;
    }
    Ok(())
}

fn read_some(r: &mut dyn Read, buf: &mut [u8]) -> Result<usize, String> {
    loop {
        match r.read(buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("decompression failed: {}", e)),
        }
    }
}

fn sz_stream<R: Read + Seek>(
    r: &mut sevenz_rust::SevenZReader<R>,
    name: &str,
    offset: u64,
    len: u64,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    // 7z decodes in solid-block order, which can differ from the file table
    // order - so the entry is matched by name.
    let mut outcome: Option<Result<(), String>> = None;
    r.for_each_entries(|entry, reader| {
        if outcome.is_none() && entry.name() == name {
            outcome = Some(pump(reader, offset, len, &mut *sink));
            return Ok(false);
        }
        Ok(true)
    })
    .map_err(|e| format!("7z read failed: {:?}", e))?;
    outcome.unwrap_or_else(|| Err(format!("{} was not found in the 7z archive", name)))
}

/// Stream one entry. `name` is the entry's original name, `idx` its position.
pub fn stream(
    path: &Path,
    kind: Kind,
    name: &str,
    idx: usize,
    offset: u64,
    len: u64,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    match kind {
        Kind::Zip => {
            let (r, _) = open_any(path)?;
            let mut z = zip::ZipArchive::new(r).map_err(es)?;
            let mut f = z
                .by_index(idx)
                .map_err(|e| format!("cannot read {} (password-protected ZIP entries are not supported): {}", name, e))?;
            pump(&mut f, offset, len, sink)
        }
        Kind::Tar => tar_stream(open_any(path)?.0, idx, offset, len, sink),
        Kind::TarGz => tar_stream(MultiGzDecoder::new(open_any(path)?.0), idx, offset, len, sink),
        Kind::Gz => {
            let mut d = MultiGzDecoder::new(open_buf(path)?);
            pump(&mut d, offset, len, sink)
        }
        Kind::Cab => {
            let mut cab = cab::Cabinet::new(open_buf(path)?).map_err(es)?;
            let mut f = cab.read_file(name).map_err(|e| format!("cannot read {}: {}", name, e))?;
            pump(&mut f, offset, len, sink)
        }
        Kind::SevenZ => {
            if lower_ext(path) == "001" {
                let mf = MultiFile::open(&volumes(path)).map_err(es)?;
                let total = mf.len();
                let mut r = sevenz_rust::SevenZReader::new(BufReader::with_capacity(1 << 20, mf), total, sevenz_rust::Password::empty())
                    .map_err(|e| format!("cannot open split 7z archive: {:?}", e))?;
                return sz_stream(&mut r, name, offset, len, sink);
            }
            let mut r = sevenz_rust::SevenZReader::open(path, sevenz_rust::Password::empty())
                .map_err(|e| format!("cannot open 7z archive: {:?}", e))?;
            sz_stream(&mut r, name, offset, len, sink)
        }
        Kind::Rar => {
            // Matched by name: with multi-volume archives the header order is
            // not a stable index. The first matching header is the file's start.
            let _ = idx;
            let mut a = unrar::Archive::new(path)
                .open_for_processing()
                .map_err(|e| format!("cannot open RAR archive: {:?}", e))?;
            loop {
                let h = match a.read_header().map_err(|e| format!("RAR read failed: {:?}", e))? {
                    Some(h) => h,
                    None => break,
                };
                if h.entry().filename.to_string_lossy() == name {
                    let (data, _rest) = h.read().map_err(|e| format!("cannot read {}: {:?}", name, e))?;
                    return pump(&mut io::Cursor::new(data), offset, len, sink);
                }
                a = h.skip().map_err(|e| format!("RAR read failed: {:?}", e))?;
            }
            Err(format!("{} was not found in the RAR archive (is a volume missing?)", name))
        }
    }
}

fn tar_stream<R: Read>(
    r: R,
    idx: usize,
    offset: u64,
    len: u64,
    sink: &mut dyn FnMut(&[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut a = tar::Archive::new(r);
    for (i, ent) in a.entries().map_err(es)?.enumerate() {
        let mut ent = ent.map_err(es)?;
        if i == idx {
            return pump(&mut ent, offset, len, sink);
        }
    }
    Err("entry not found in the TAR archive".into())
}
