//! Format dispatcher: turns an image path into a backend + how to present it.

use std::path::{Path, PathBuf};

use crate::backend::{Backend, Raw, Src};
use crate::detect::{Detection, Kind};
use crate::{chdimg, optical, ova, vdi, vmdk};

pub struct Opened {
    pub backend: Box<dyn Backend>,
    pub block_size: u32,
    /// Present as a CD/DVD drive (always read-only).
    pub optical: bool,
    pub removable: bool,
    /// Container format: writes go to an overlay, never into the image.
    pub container: bool,
    pub kind: String,
    pub detail: String,
}

fn sibling(path: &Path, ext: &str) -> Option<PathBuf> {
    for v in [ext.to_lowercase(), ext.to_uppercase()] {
        let p = path.with_extension(&v);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn as_optical(t: optical::Track, kind: &str) -> Opened {
    let detail = format!("{} - {}-byte sectors, {} sectors", t.how, t.ss, t.sectors);
    Opened {
        backend: Box::new(t),
        block_size: 2048,
        optical: true,
        removable: true,
        container: false,
        kind: kind.to_string(),
        detail,
    }
}

fn container(b: Box<dyn Backend>, kind: &str, detail: String) -> Opened {
    Opened { backend: b, block_size: 512, optical: false, removable: false, container: true, kind: kind.to_string(), detail }
}

pub fn open(path: &Path, ext: &str, readonly: bool, det: Option<&Detection>) -> Result<Opened, String> {
    match ext {
        "vdi" => {
            let v = vdi::open(Src::open(path)?)?;
            let d = v.describe();
            Ok(container(Box::new(v), "VirtualBox VDI", d))
        }
        "vmdk" => {
            let v = vmdk::open_path(path)?;
            let d = v.describe();
            Ok(container(Box::new(v), "VMware VMDK", d))
        }
        "ova" => {
            let src = Src::open(path)?;
            let entries = ova::list(&src)?;
            let disks: Vec<&ova::Entry> = entries.iter().filter(|e| e.name.to_lowercase().ends_with(".vmdk")).collect();
            let first = disks.first().ok_or("this OVA contains no .vmdk disk")?;
            let v = vmdk::open_src(src.sub(first.off, first.size))?;
            let d = format!("{} (disk 1 of {}) - {}", first.name, disks.len(), v.describe());
            Ok(container(Box::new(v), "OVA appliance (VMDK)", d))
        }
        "chd" => chdimg::open(path),
        "cue" => Ok(as_optical(optical::from_cue(path)?, "CD image (BIN/CUE)")),
        "bin" => match sibling(path, "cue") {
            Some(c) => Ok(as_optical(optical::from_cue(&c)?, "CD image (BIN/CUE)")),
            None => Ok(as_optical(optical::guess(Src::open(path)?, "BIN without CUE")?, "CD image (BIN)")),
        },
        "mds" => {
            let mdf = sibling(path, "mdf").ok_or("the matching .mdf file was not found next to the .mds")?;
            Ok(as_optical(optical::guess(Src::open(&mdf)?, "MDS/MDF")?, "CD/DVD image (MDS/MDF)"))
        }
        "mdf" => Ok(as_optical(optical::guess(Src::open(path)?, "MDF")?, "CD/DVD image (MDF)")),
        "ccd" => {
            let img = sibling(path, "img").ok_or("the matching .img file was not found next to the .ccd")?;
            Ok(as_optical(optical::guess(Src::open(&img)?, "CloneCD")?, "CD image (CloneCD)"))
        }
        "cdi" => {
            let src = Src::open(path)?;
            Ok(as_optical(optical::scan(&src, "DiscJuggler CDI")?, "CD image (CDI)"))
        }
        "nrg" => Ok(as_optical(optical::from_nrg(Src::open(path)?)?, "CD/DVD image (Nero NRG)")),
        "img" | "ima" | "flp" | "raw" => {
            let kind = det.map(|d| d.kind);
            if ext == "img" && (sibling(path, "ccd").is_some() || kind == Some(Kind::CdRaw)) {
                return Ok(as_optical(optical::guess(Src::open(path)?, "raw CD image")?, "CD image (CloneCD / raw)"));
            }
            let removable = matches!(kind, Some(Kind::Floppy) | Some(Kind::Volume));
            let label = match kind {
                Some(Kind::Floppy) => "Floppy disk image",
                Some(Kind::Volume) => "Partitionless volume image",
                _ => "Raw disk image",
            };
            let r = Raw::open(path, !readonly).map_err(|e| format!("cannot open image: {}", e))?;
            Ok(Opened {
                backend: Box::new(r),
                block_size: 512,
                optical: false,
                removable,
                container: false,
                kind: label.to_string(),
                detail: det.map(|d| d.description.clone()).unwrap_or_default(),
            })
        }
        "" => Err("the file has no extension - cannot tell what it is".into()),
        other => Err(format!(".{} is not a supported image type", other)),
    }
}
