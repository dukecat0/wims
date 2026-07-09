use rayon::prelude::*;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Mutex;

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

/// Records which physical objects `(dev, ino)` have already been counted,
/// so a single object reached by more than one path isn't counted twice.
///
/// On macOS this matters a lot: scanning `/` reaches the whole data volume
/// both through the top-level firmlinks (`/Users`, `/Applications`, …) and
/// again through `/System/Volumes/Data/…`. Both paths share one inode, so
/// without this the total roughly doubles. Hard-linked files are handled
/// the same way. Sharded to keep lock contention low during the parallel
/// walk.
struct Visited {
    shards: Vec<Mutex<HashSet<(u64, u64)>>>,
}

impl Visited {
    fn new() -> Self {
        Visited {
            shards: (0..64).map(|_| Mutex::new(HashSet::new())).collect(),
        }
    }

    /// Claim an object; true the first time it is seen, false thereafter.
    fn claim(&self, id: (u64, u64)) -> bool {
        let shard = &self.shards[(id.1 as usize) & (self.shards.len() - 1)];
        shard.lock().unwrap().insert(id)
    }
}

pub fn scan_live(root: &Path, tx: Sender<ScanMsg>, progress: &Progress) {
    let visited = Visited::new();
    // Claim the root so a firmlink pointing back at it can't re-descend.
    if let Some(id) = object_id(root) {
        visited.claim(id);
    }
    scan_dir(root, Vec::new(), &tx, progress, &visited);
    let _ = tx.send(ScanMsg::Done);
}

fn scan_dir(
    path: &Path,
    rel: Vec<String>,
    tx: &Sender<ScanMsg>,
    progress: &Progress,
    visited: &Visited,
) {
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
                // A directory already counted elsewhere (firmlink or bind
                // mount) is still shown, but not descended into, so its
                // bytes aren't counted a second time.
                let first_visit = match dir_id(entry) {
                    Some(id) => visited.claim(id),
                    None => true,
                };
                if first_visit {
                    subdirs.push((name.clone(), entry.path()));
                }
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
                let size = match entry.metadata() {
                    // A hard link to a file already counted adds no bytes.
                    Ok(md) if hardlinked(&md) && !visited.claim(object_ids(&md)) => 0,
                    Ok(md) => allocated_size(&md),
                    Err(_) => 0,
                };
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
            scan_dir(&sub_path, child_rel, tx, progress, visited);
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

#[cfg(unix)]
fn object_ids(md: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (md.dev(), md.ino())
}

#[cfg(unix)]
fn hardlinked(md: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    md.nlink() > 1
}

/// `(dev, ino)` of a directory entry, via a non-following stat.
#[cfg(unix)]
fn dir_id(entry: &fs::DirEntry) -> Option<(u64, u64)> {
    entry.metadata().ok().map(|md| object_ids(&md))
}

/// `(dev, ino)` of a path (the scan root).
#[cfg(unix)]
fn object_id(path: &Path) -> Option<(u64, u64)> {
    fs::symlink_metadata(path).ok().map(|md| object_ids(&md))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::mpsc::channel;

    /// Sum the root's total by collecting the streamed entries.
    fn scan_root_bytes(dir: &Path) -> u64 {
        let (tx, rx) = channel();
        let progress = Progress::default();
        scan_live(dir, tx, &progress);
        // Root's own children arrive as the entry with an empty path.
        let mut total = 0;
        for msg in rx {
            if let ScanMsg::Entries { path, entries } = msg {
                if path.is_empty() {
                    total = entries.iter().map(|e| e.size).sum();
                }
            }
        }
        total
    }

    #[test]
    fn hard_links_counted_once() {
        let dir = std::env::temp_dir().join(format!("wims-hl-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("original.bin"), vec![0u8; 4 * 1024 * 1024]).unwrap();
        fs::hard_link(dir.join("original.bin"), dir.join("link1.bin")).unwrap();
        fs::hard_link(dir.join("original.bin"), dir.join("link2.bin")).unwrap();
        fs::write(dir.join("separate.bin"), vec![0u8; 2 * 1024 * 1024]).unwrap();

        let total = scan_root_bytes(&dir);
        fs::remove_dir_all(&dir).unwrap();

        // 4 MB (one of the three links) + 2 MB, not 14 MB.
        let mb = total / (1024 * 1024);
        assert!(
            (5..=7).contains(&mb),
            "expected ~6 MB after hard-link dedup, got {mb} MB ({total} bytes)"
        );
    }
}

// Dedup is a Unix (inode) concept; elsewhere every entry is counted once.
#[cfg(not(unix))]
fn object_ids(_md: &fs::Metadata) -> (u64, u64) {
    (0, 0)
}

#[cfg(not(unix))]
fn hardlinked(_md: &fs::Metadata) -> bool {
    false
}

#[cfg(not(unix))]
fn dir_id(_entry: &fs::DirEntry) -> Option<(u64, u64)> {
    None
}

#[cfg(not(unix))]
fn object_id(_path: &Path) -> Option<(u64, u64)> {
    None
}
