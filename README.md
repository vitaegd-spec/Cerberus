# CERBERUS

![License: Apache 2.0](https://img.shields.io/badge/License-Apache_2.0-blue.svg) ![Platform: Windows 10/11 x64](https://img.shields.io/badge/Platform-Windows_10%2F11_x64-0078D6.svg) ![Built with Rust + Tauri](https://img.shields.io/badge/Built_with-Rust_%2B_Tauri-orange.svg)

**Mount disk images, CD/DVD images and archives as Windows drives – using only what is already built into Windows.**

CERBERUS is a small Windows 10/11 desktop app (Rust + Tauri) that turns image files into drive letters. It installs **no drivers**, uses **no third-party mounting software**, and never converts or extracts anything: Windows' own Virtual Disk service, iSCSI Initiator and Projected File System do the work, and CERBERUS feeds them.

---

## Features

- **28 file extensions** – disks, CD/DVD images and archives (see below)
- **Drag & drop**, file picker, **Explorer right-click → "Mount with CERBERUS"**, and optional **double-click to mount**
- Lives in the **system tray** – closing the window keeps drives mounted; the tray menu lists every drive (Open / Unmount)
- **Start with Windows** and **re-mount the last session**
- **Changes files** for VirtualBox/VMware/CHD disks – write to them without ever modifying the original image; keep or discard on unmount, manage them in Settings
- **Initialize & Format** for blank VHD/VHDX/IMG disks, **Format** for existing volumes
- **Linux disks**: unreadable partitions (ext4, swap, LVM) are detected and listed; **Open in WSL** attaches ext partitions to WSL 2 so Explorer can browse them
- **Split archives** – open any volume of a `.7z.001` / `.part1.rar` / `.r00` set
- Runs as a normal user; asks for administrator permission (UAC) only the first time a disk or disc image is mounted

## Supported formats

| Type | Extensions | Shows up as | Writable |
|---|---|---|---|
| ISO | `.iso` | DVD drive | No |
| Hyper-V | `.vhd`, `.vhdx` | Disk | Yes (directly) |
| Raw disk / floppy | `.img`, `.ima`, `.flp`, `.raw` | Disk / floppy | Yes (directly) |
| VirtualBox | `.vdi` | Disk | Yes (changes file) |
| VMware | `.vmdk` (sparse, stream-optimized, flat, split), `.ova` | Disk | Yes (changes file) |
| MAME | `.chd` (hard disk) | Disk | Yes (changes file) |
| CD/DVD | `.cue`/`.bin`, `.mds`/`.mdf`, `.ccd`/`.img`, `.cdi`, `.nrg`, `.chd` (CD/DVD) | CD/DVD drive | No |
| Archives | `.zip`, `.7z`, `.rar`, `.tar`, `.tar.gz`/`.tgz`, `.gz`, `.cab` | Drive letter | No |
| Split archives | `.7z.001`, `.zip.001`, `.tar.001`, `.part1.rar`, `.rar` + `.r00` | Drive letter | No |

`.img` files are identified by their **contents**, not the extension (hard disk, floppy, ISO or CloneCD).

**Not supported:** snapshot/child disks (differencing VDI, VMDK snapshots, CHDs with a parent), audio CD tracks, multi-session discs whose files are in a later session, password-protected archives, WinZip `.z01` split sets.

## Requirements

- Windows 10 or 11, 64-bit
- WebView2 runtime (built into Windows 11)
- Built-in Windows features used on demand: iSCSI Initiator service, Projected File System (`Client-ProjFS`, turned on automatically the first time an archive is mounted)
- Optional: WSL 2 with a Linux distribution, for **Open in WSL**

## Install (portable)

1. Download `CERBERUS-v<version>-win-x64.zip` from **Releases** and check it against the `.sha256` file if you like.
2. Extract it to a folder you can write to (e.g. `D:\Apps\CERBERUS`) – **not** `C:\Program Files`; settings, logs and changes files are stored next to `cerberus.exe`.
3. Run `CERBERUS.bat` (or `cerberus.exe`). Optional: `Create-Desktop-Shortcut.bat`.

The exe is not code-signed, so on first run Windows SmartScreen may show *"Windows protected your PC"* → **More info** → **Run anyway**.

## Using it

- Drop files on the window or click **＋ Mount image…**. Each mount gets a card with **Open**, **File**, **Unmount** and, where relevant, **Initialize & Format**, **Format**, **Folder** or **Open in WSL**.
- **Mount read-only** is on by default; untick it to write to VHD/VHDX/raw images or to changes files.
- **⚙ Settings**: start with Windows, re-mount last session, Explorer right-click menu, double-click file types, read-only default, the drive letter archives start from, log detail, and **Saved changes** (list and delete kept changes files).
- Closing the window hides CERBERUS to the tray. **Tray → Quit CERBERUS** unmounts everything (changes are kept) and exits.

## How it works

```
Tauri UI (normal user, tray) ──► mounts.rs ─┬─► Windows Virtual Disk (Mount-DiskImage)        ISO, VHD, VHDX
                                            │
                                            ├─► formats.rs ─► vdi / vmdk / ova / chdimg / optical readers
                                            │       disk.rs (+ overlay.rs changes file)
                                            │       target / conn / scsi / mmc.rs  ── in-process iSCSI target on 127.0.0.1:3260
                                            │       └─► Windows iSCSI Initiator (loopback) ──► real disk / CD-ROM device
                                            │
                                            ├─► archmount.rs ─► projfs.rs (Projected File System provider)
                                            │                    └─► archive.rs (ZIP / 7Z / RAR / TAR / GZ / CAB, split volumes)
                                            │
                                            └─► ps.rs ─► broker.rs ══ 127.0.0.1 ══► elevated helper (cerberus.exe --broker)
                                                                                     runs the admin PowerShell (mount, iSCSI, disks, WSL)
```

- The window runs **non-elevated** so Explorer drag & drop works; a token-authenticated elevated helper copy of `cerberus.exe` runs the few administrator operations.
- Disk/disc images that Windows can't mount itself are served by CERBERUS's own **iSCSI target** on the loopback address; Windows' iSCSI Initiator connects to it and sees a normal disk or CD/DVD drive.
- Archives are projected into `data\archives\<id>` with the **Projected File System** – only the table of contents is read at mount time; file data is read from the archive when something opens it.
- Windows integration uses **per-user registry keys only** (`HKCU`): `…\CurrentVersion\Run`, `SystemFileAssociations\.<ext>\shell\CERBERUS.Mount`, ProgID `CERBERUS.Image`.

## Building from source

Requirements: [Rust](https://rustup.rs) (MSVC toolchain) and the Visual Studio C++ build tools (needed to compile the bundled UnRAR source). Node.js is **not** needed – the `ui\` folder is embedded directly.

| Script | What it does |
|---|---|
| `BUILD.bat` | `cargo build --release` and copies `cerberus.exe` next to the scripts |
| `CERBERUS.bat` | Starts CERBERUS (builds first if `cerberus.exe` is missing) |
| `MAKE-RELEASE.bat` | Builds, then creates `release\CERBERUS-v<version>-win-x64.zip` (exe, launcher, icon, README, QUICKSTART, notices) and a `.sha256` file |

Quit CERBERUS from the tray before building (closing the window only hides it). The test checklist is in [`docs/TESTING.md`](docs/TESTING.md).

### Project layout

| Path | Contents |
|---|---|
| `src/` | Rust source (one module per engine / format, see *How it works*) |
| `ui/` | HTML / CSS / JS for the window |
| `icons/` | App icons |
| `capabilities/` | Tauri capability file |
| `packaging/` | Files that go only into the release ZIP (portable launcher, QUICKSTART) |
| `docs/` | Test checklist |

## Third-party components

Compiled into the exe: Tauri, serde, rfd, flate2, chd, zip, sevenz-rust, unrar (official UnRAR source – extraction only), tar, cab, libloading. See [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md).

## License

Copyright 2026 Henry Murphy

Licensed under the **Apache License, Version 2.0** – see [`LICENSE`](LICENSE) and [`NOTICE`](NOTICE).
You may use, modify and redistribute CERBERUS (including commercially) as long as you keep the licence and notices and mark files you changed. It is provided **"as is", without warranty** – always keep backups of important images.

Third-party components keep their own licences (see [`THIRD-PARTY-NOTICES.md`](THIRD-PARTY-NOTICES.md)); in particular the bundled UnRAR code may only be used to extract RAR archives.
