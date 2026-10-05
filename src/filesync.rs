//! File sync: directory listing/hashing plus chunked, resumable single-file
//! upload (phone -> daemon) and download (daemon -> phone). Resumability is
//! disk-based, not in-memory — the client always asks how many bytes of a
//! `<name>.partial` sibling already exist before sending/requesting more, so
//! a run survives a daemon restart (unlike `pty::sessions()`, nothing here
//! needs a process-global registry).
//!
//! Blocking filesystem work runs on `spawn_blocking`, and anything that can
//! take long (listing, hashing, deleting, `sync.get.*` streaming) runs on its
//! own spawned task so it never stalls the connection's single frame-read
//! loop — a stalled loop stops answering pings and the phone drops the link.

use crate::proto::{self, SyncEntry};
use anyhow::{anyhow, Result};
use rmpv::Value;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;

pub const CHUNK_SIZE: usize = 256 * 1024;

fn mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn partial_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    path.with_file_name(name)
}

// --- fs.list: one directory's immediate children, for the remote folder picker

/// A path this can never collide with on any real filesystem — sent by the
/// client (never typed by anyone) to mean "list drives, not a directory".
/// The client walks up to this from a Windows drive root instead of dead-ending
/// there, so a phone can still reach a second drive without ever typing a path.
pub const DRIVES_SENTINEL: &str = "\u{0}drives";

/// An empty `path` means "start the picker somewhere sensible" — the user's
/// home directory, so a fresh wizard doesn't open on the filesystem root.
pub fn fs_list(ch: i64, path: String) -> Value {
    if path == DRIVES_SENTINEL {
        return proto::fs_list(ch, DRIVES_SENTINEL, list_drives());
    }
    let path = if path.is_empty() {
        dirs::home_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or(path)
    } else {
        path
    };
    let entries = fs::read_dir(&path)
        .map(|rd| {
            let mut out: Vec<(String, bool)> = rd
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                    Some((name, is_dir))
                })
                .collect();
            out.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()));
            out
        })
        .unwrap_or_default();
    proto::fs_list(ch, &path, entries)
}

#[cfg(windows)]
fn list_drives() -> Vec<(String, bool)> {
    (b'A'..=b'Z')
        .filter_map(|b| {
            let root = format!("{}:\\", b as char);
            Path::new(&root).exists().then(|| (root, true))
        })
        .collect()
}

#[cfg(not(windows))]
fn list_drives() -> Vec<(String, bool)> {
    vec![("/".to_string(), true)]
}

// --- sync.hash: content hashes of just the files the client asks about -----

/// Hashes `root/<path>` for each of `paths`, sending one `sync.hash` frame per
/// file as soon as it's done (so a big batch reports progress instead of
/// going silent until the end), then `sync.hash.end`. Blocking: run it on
/// `spawn_blocking`.
pub fn sync_hash(ch: i64, root: String, paths: Vec<String>, tx: mpsc::Sender<Value>) {
    let root_path = PathBuf::from(&root);
    for rel in paths {
        let sha = hash_file(&root_path.join(&rel)).ok();
        if tx.blocking_send(proto::sync_hash(ch, &rel, sha)).is_err() {
            return; // connection gone
        }
    }
    let _ = tx.blocking_send(proto::sync_hash_end(ch));
}

// --- sync.list: recursive listing of one root, optionally content-hashed

pub fn sync_list(root: String, hash: bool) -> Vec<SyncEntry> {
    let root_path = PathBuf::from(&root);
    walkdir::WalkDir::new(&root_path)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            let rel = e.path().strip_prefix(&root_path).ok()?;
            let sha256 = if hash { hash_file(e.path()).ok() } else { None };
            Some(SyncEntry {
                path: rel.to_string_lossy().replace('\\', "/"),
                size: meta.len(),
                mtime_ms: mtime_ms(&meta),
                sha256,
            })
        })
        .collect()
}

fn hash_file(path: &Path) -> Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

// --- sync.put.*: phone -> daemon, driven by the client's own frames --------

/// One in-flight upload, keyed by channel id in `session.rs`. Writes land in
/// `<name>.partial` next to the final path; `finish` renames it into place.
pub struct PutState {
    tmp: PathBuf,
    dest: PathBuf,
    file: File,
    /// The phone's modified time for the file, applied on `finish` so both
    /// sides agree and the next run's quick check sees it as unchanged.
    mtime_ms: Option<i64>,
}

impl PutState {
    /// Opens (or resumes) the `.partial` sibling of `dest_path` and reports how
    /// many bytes of it already exist on disk — that's the resume point, read
    /// straight off the filesystem rather than any state kept in memory.
    pub fn begin(dest_path: &str, mtime_ms: Option<i64>) -> Result<(Self, u64)> {
        let dest = PathBuf::from(dest_path);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = partial_path(&dest);
        let resume_offset = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        let file = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&tmp)?;
        Ok((Self { tmp, dest, file, mtime_ms }, resume_offset))
    }

    /// Writes one chunk at `offset` (the client always sends the offset it
    /// believes the file is at — usually just the running total — so a
    /// retried chunk after a dropped connection overwrites in place instead
    /// of appending a duplicate).
    pub fn write_chunk(&mut self, offset: u64, data: &[u8]) -> Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)?;
        Ok(())
    }

    /// Flushes, then atomically installs the finished upload at its real path.
    pub fn finish(self) -> Result<()> {
        if let Some(ms) = self.mtime_ms.filter(|ms| *ms > 0) {
            let t = UNIX_EPOCH + std::time::Duration::from_millis(ms as u64);
            if let Err(e) = self.file.set_modified(t) {
                crate::plog!("sync.put: couldn't set mtime on {}: {e}", self.dest.display());
            }
        }
        drop(self.file);
        fs::rename(&self.tmp, &self.dest)?;
        Ok(())
    }
}

// --- sync.delete: mirror cleanup, phone-requested -------------------------

pub fn delete(path: &str) -> bool {
    fs::remove_file(path).is_ok()
}

// --- sync.get.*: daemon -> phone, streamed from a spawned task -------------

/// Reads `path` from `resume_offset` onward in `CHUNK_SIZE` pieces, pushing
/// `sync.get.chunk` frames through `tx`, then a final `sync.get.end`. Runs on
/// its own task so a large download never blocks the connection's frame loop
/// (and so a `screen`/`pty` frame for the same connection keeps flowing
/// alongside it).
pub async fn send_file(ch: i64, path: String, resume_offset: u64, tx: mpsc::Sender<Value>) {
    let result = send_file_inner(ch, &path, resume_offset, &tx).await;
    let _ = tx.send(proto::sync_get_end(ch, result.is_ok())).await;
    if let Err(e) = result {
        crate::plog!("sync.get ch={ch} {path}: {e}");
    }
}

async fn send_file_inner(ch: i64, path: &str, resume_offset: u64, tx: &mpsc::Sender<Value>) -> Result<()> {
    let meta = tokio::fs::metadata(path).await?;
    if !meta.is_file() {
        return Err(anyhow!("not a file"));
    }
    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    tx.send(proto::sync_get_meta(ch, size, mtime)).await.ok();

    let mut file = tokio::fs::File::open(path).await?;
    let start = resume_offset.min(size);
    file.seek(std::io::SeekFrom::Start(start)).await?;

    let mut offset = start;
    let mut buf = vec![0u8; CHUNK_SIZE];
    let t0 = std::time::Instant::now();
    let (mut read_t, mut send_t) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    loop {
        let tr = std::time::Instant::now();
        let n = file.read(&mut buf).await?;
        read_t += tr.elapsed();
        if n == 0 {
            break;
        }
        let ts = std::time::Instant::now();
        if tx.send(proto::sync_get_chunk(ch, offset, &buf[..n])).await.is_err() {
            return Err(anyhow!("connection closed"));
        }
        send_t += ts.elapsed(); // waiting for room in the send queue = the link is the limit
        offset += n as u64;
    }
    let bytes = offset - start;
    if bytes >= 256 * 1024 {
        let secs = t0.elapsed().as_secs_f64().max(0.001);
        crate::latstat::record("d.sync.get_mbps", bytes as f64 / 1e6 / secs);
        crate::latstat::record("d.sync.get_disk_read_ms", crate::latstat::ms(read_t));
        crate::latstat::record("d.sync.get_link_wait_ms", crate::latstat::ms(send_t));
    }
    Ok(())
}
