use rayon::prelude::*;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

/// Shared counters the scan thread bumps while the UI shows progress.
#[derive(Default)]
pub struct Progress {
    pub files: AtomicU64,
    pub bytes: AtomicU64,
}

/// One entry in the scanned tree. Paths are reconstructed from the name
/// chain instead of stored per-node to keep memory small on huge trees.
pub struct Node {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub n_files: u64,
    pub children: Vec<Node>,
}

/// Streamed from the scan thread to the UI so the tree fills in live.
pub enum ScanMsg {
    /// The immediate children of the directory at `path` (names relative
    /// to the scan root). Directories arrive as empty placeholders and
    /// fill in through later messages.
    Entries { path: Vec<String>, entries: Vec<Node> },
    Done,
}

pub fn scan_live(root: &Path, tx: Sender<ScanMsg>, progress: &Progress) {
    scan_dir(root, Vec::new(), &tx, progress);
    let _ = tx.send(ScanMsg::Done);
}

fn scan_dir(path: &Path, rel: Vec<String>, tx: &Sender<ScanMsg>, progress: &Progress) {
    let dir_entries: Vec<fs::DirEntry> = match fs::read_dir(path) {
        Ok(rd) => rd.flatten().collect(),
        Err(_) => Vec::new(), // unreadable (permissions) — show as empty
    };

    let mut nodes = Vec::with_capacity(dir_entries.len());
    let mut subdirs: Vec<(String, PathBuf)> = Vec::new();
    for entry in &dir_entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        match entry.file_type() {
            Ok(ft) if ft.is_dir() => {
                subdirs.push((name.clone(), entry.path()));
                nodes.push(Node {
                    name,
                    size: 0,
                    is_dir: true,
                    n_files: 0,
                    children: Vec::new(),
                });
            }
            _ => {
                // DirEntry::metadata does not follow symlinks on Unix,
                // so links are counted as themselves, never traversed.
                let size = entry.metadata().map(|md| allocated_size(&md)).unwrap_or(0);
                progress.files.fetch_add(1, Ordering::Relaxed);
                progress.bytes.fetch_add(size, Ordering::Relaxed);
                nodes.push(Node {
                    name,
                    size,
                    is_dir: false,
                    n_files: 1,
                    children: Vec::new(),
                });
            }
        }
    }

    if tx
        .send(ScanMsg::Entries {
            path: rel.clone(),
            entries: nodes,
        })
        .is_err()
    {
        return; // UI is gone (quit or rescan) — stop descending
    }

    subdirs
        .into_par_iter()
        .for_each_with(tx.clone(), |tx, (name, sub_path)| {
            let mut child_rel = rel.clone();
            child_rel.push(name);
            scan_dir(&sub_path, child_rel, tx, progress);
        });
}

/// Actual disk usage (like `du`), not apparent file length.
#[cfg(unix)]
fn allocated_size(md: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    md.blocks() * 512
}

#[cfg(not(unix))]
fn allocated_size(md: &fs::Metadata) -> u64 {
    md.len()
}
