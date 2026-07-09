use crate::scanner::{scan_live, Node, Progress, ScanMsg};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

/// Cap on scan messages merged per frame so the UI stays responsive
/// even when the scanner floods the channel.
const MERGE_BUDGET: usize = 50_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Size,
    Name,
    Files,
}

impl SortMode {
    pub fn label(self) -> &'static str {
        match self {
            SortMode::Size => "size",
            SortMode::Name => "name",
            SortMode::Files => "files",
        }
    }

    fn next(self) -> Self {
        match self {
            SortMode::Size => SortMode::Name,
            SortMode::Name => SortMode::Files,
            SortMode::Files => SortMode::Size,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Overlay {
    None,
    ConfirmTrash,
    ConfirmDelete,
}

pub struct Status {
    pub text: String,
    pub is_error: bool,
}

/// One trashed entry, kept so it can be put back on undo.
struct TrashedItem {
    /// The removed subtree, for re-inserting into the in-memory tree.
    node: Node,
    /// Names from the root down to the directory it lived in.
    parent_names: Vec<String>,
    /// Where it lived on disk.
    original: PathBuf,
    /// Where it landed in the Trash, if we could locate it.
    trashed: Option<PathBuf>,
}

/// One `d` action's worth of trashed items — undone as a unit.
struct UndoBatch {
    items: Vec<TrashedItem>,
}

/// One character cell of a rendered thumbnail: a half-block glyph whose
/// foreground and background colors carry two vertically stacked pixels.
pub struct ThumbCell {
    pub ch: char,
    pub fg: Color,
    pub bg: Color,
}

pub enum PreviewContent {
    /// Pre-rendered to terminal cells so it works in any terminal
    /// without a graphics protocol. cells[row][col].
    Image {
        dims: (u32, u32),
        cells: Vec<Vec<ThumbCell>>,
    },
    /// First lines of a text-based file, pre-styled with syntax
    /// highlighting where the language is recognized.
    Text {
        lines: Vec<Line<'static>>,
        truncated: bool,
    },
}

/// An open preview popup.
pub struct Preview {
    pub name: String,
    pub content: PreviewContent,
}

/// Decoding a huge image would freeze the UI and eat memory.
const MAX_PREVIEW_BYTES: u64 = 80 * 1024 * 1024;

/// Text previews read at most this much and show at most this many lines.
const MAX_TEXT_PREVIEW_BYTES: u64 = 256 * 1024;
const MAX_TEXT_PREVIEW_LINES: usize = 400;

const IMAGE_EXTENSIONS: [&str; 11] = [
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "tif", "tiff", "tga", "qoi",
];

pub struct App {
    pub root_path: PathBuf,
    /// Children stay in arrival order; `view_order` provides sorting.
    pub root: Node,
    pub scanning: bool,
    pub progress: Arc<Progress>,
    scan_rx: Receiver<ScanMsg>,
    scan_started: Instant,
    /// Index of the child we descended into, per level below the root.
    /// Stable because children are only ever appended, never reordered.
    pub stack: Vec<usize>,
    /// Selection (display position) remembered per ancestor level.
    saved_selection: Vec<usize>,
    /// Display position of the cursor within `view_order`.
    pub selected: usize,
    /// Sorted indices into the current directory's children.
    pub view_order: Vec<usize>,
    /// First visible row of the list viewport.
    pub list_offset: usize,
    /// Tree indices of marked entries in the *current* directory. Cleared
    /// on navigation, since indices are only meaningful within one dir.
    pub marked: HashSet<usize>,
    /// Stack of trash actions, most recent last; `u` pops one.
    undo: Vec<UndoBatch>,
    pub sort: SortMode,
    pub overlay: Overlay,
    /// True after a first Esc press; a second Esc quits.
    pub pending_quit: bool,
    /// Terminal background detected at startup; themes adapt to it.
    pub light_bg: bool,
    pub preview: Option<Preview>,
    pub status: Option<Status>,
    /// (total, available) bytes of the volume holding root_path.
    pub disk: Option<(u64, u64)>,
    pub tick: u64,
}

impl App {
    pub fn new(root_path: PathBuf, light_bg: bool) -> Self {
        // Building the syntax set takes a moment; do it off-thread now so
        // the first text preview opens instantly.
        std::thread::spawn(move || {
            let _ = syntax_assets(light_bg);
        });
        let progress = Arc::new(Progress::default());
        let scan_rx = start_scan(root_path.clone(), Arc::clone(&progress));
        let root_name = root_path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| root_path.to_string_lossy().into_owned());
        App {
            disk: disk_stats(&root_path),
            root_path,
            root: Node {
                name: root_name,
                size: 0,
                is_dir: true,
                n_files: 0,
                children: Vec::new(),
            },
            scanning: true,
            progress,
            scan_rx,
            scan_started: Instant::now(),
            stack: Vec::new(),
            saved_selection: Vec::new(),
            selected: 0,
            view_order: Vec::new(),
            list_offset: 0,
            marked: HashSet::new(),
            undo: Vec::new(),
            sort: SortMode::Size,
            overlay: Overlay::None,
            pending_quit: false,
            light_bg,
            preview: None,
            status: None,
            tick: 0,
        }
    }

    /// Merge pending scan results into the tree, then re-sort the view.
    /// Called once per frame.
    pub fn poll_scan(&mut self) {
        if !self.scanning {
            return; // tree is final; the view only changes through actions
        }
        let mut changed = false;
        for _ in 0..MERGE_BUDGET {
            match self.scan_rx.try_recv() {
                Ok(ScanMsg::Entries { path, entries }) => {
                    self.apply_entries(&path, entries);
                    changed = true;
                }
                Ok(ScanMsg::Done) => {
                    self.scanning = false;
                    self.status = Some(Status {
                        text: format!(
                            "Scanned {} files ({}) in {:.1}s",
                            group_digits(self.root.n_files),
                            human_size(self.root.size),
                            self.scan_started.elapsed().as_secs_f64()
                        ),
                        is_error: false,
                    });
                    changed = true;
                    break;
                }
                Err(_) => break,
            }
        }
        if changed {
            self.refresh_view();
        }
    }

    /// Attach a directory's freshly enumerated children and grow every
    /// ancestor by the new file bytes so totals tick up live.
    fn apply_entries(&mut self, path: &[String], entries: Vec<Node>) {
        let mut indices = Vec::with_capacity(path.len());
        let mut node = &self.root;
        for name in path {
            match node.children.iter().position(|c| c.name == *name) {
                Some(i) => {
                    indices.push(i);
                    node = &node.children[i];
                }
                None => return, // parent vanished (deleted mid-merge)
            }
        }

        let add_size: u64 = entries.iter().map(|e| e.size).sum();
        let add_files: u64 = entries.iter().map(|e| e.n_files).sum();
        let mut node = &mut self.root;
        node.size += add_size;
        node.n_files += add_files;
        for &i in &indices {
            node = &mut node.children[i];
            node.size += add_size;
            node.n_files += add_files;
        }
        node.children = entries;
    }

    /// Recompute the sorted view of the current directory, keeping the
    /// cursor on the same entry even as rows shift underneath it.
    fn refresh_view(&mut self) {
        let followed = self.view_order.get(self.selected).copied();
        let order = sorted_indices(self.current(), self.sort);
        self.selected = followed
            .and_then(|tree_idx| order.iter().position(|&i| i == tree_idx))
            .unwrap_or_else(|| self.selected.min(order.len().saturating_sub(1)));
        self.view_order = order;
    }

    /// The directory node currently being viewed.
    pub fn current(&self) -> &Node {
        let mut node = &self.root;
        for &i in &self.stack {
            node = &node.children[i];
        }
        node
    }

    /// Filesystem path of the directory currently being viewed.
    pub fn current_path(&self) -> PathBuf {
        let mut path = self.root_path.clone();
        let mut node = &self.root;
        for &i in &self.stack {
            node = &node.children[i];
            path.push(&node.name);
        }
        path
    }

    fn current_dir_mut(&mut self) -> &mut Node {
        let stack = self.stack.clone();
        let mut node = &mut self.root;
        for &i in &stack {
            node = &mut node.children[i];
        }
        node
    }

    /// Names from the root down to the directory currently being viewed.
    fn current_names(&self) -> Vec<String> {
        let mut names = Vec::with_capacity(self.stack.len());
        let mut node = &self.root;
        for &i in &self.stack {
            node = &node.children[i];
            names.push(node.name.clone());
        }
        names
    }

    /// Resolve a chain of directory names to tree indices from the root,
    /// or None if any link no longer exists.
    fn resolve_names(&self, names: &[String]) -> Option<Vec<usize>> {
        let mut idxs = Vec::with_capacity(names.len());
        let mut node = &self.root;
        for name in names {
            let i = node
                .children
                .iter()
                .position(|c| c.is_dir && c.name == *name)?;
            idxs.push(i);
            node = &node.children[i];
        }
        Some(idxs)
    }

    /// Scroll the viewport so the selection stays visible, and clamp the
    /// offset when the list shrinks. Called by the renderer each frame
    /// with the actual viewport height.
    pub fn ensure_visible(&mut self, height: usize) {
        if height == 0 {
            return;
        }
        if self.selected < self.list_offset {
            self.list_offset = self.selected;
        } else if self.selected >= self.list_offset + height {
            self.list_offset = self.selected + 1 - height;
        }
        let max_offset = self.view_order.len().saturating_sub(height);
        if self.list_offset > max_offset {
            self.list_offset = max_offset;
        }
    }

    pub fn selected_node(&self) -> Option<&Node> {
        let tree_idx = *self.view_order.get(self.selected)?;
        self.current().children.get(tree_idx)
    }

    pub fn move_selection(&mut self, delta: isize) {
        let len = self.view_order.len();
        if len == 0 {
            return;
        }
        self.selected = (self.selected as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    pub fn select_last(&mut self) {
        self.selected = self.view_order.len().saturating_sub(1);
    }

    /// Toggle the mark on the current entry and advance, so holding Space
    /// (or tapping it down a run) marks several quickly.
    pub fn toggle_mark(&mut self) {
        if let Some(&tree_idx) = self.view_order.get(self.selected) {
            if !self.marked.remove(&tree_idx) {
                self.marked.insert(tree_idx);
            }
            self.move_selection(1);
        }
    }

    /// Mark every entry in the directory, or clear all if already fully
    /// marked — a quick "select all to wipe this folder" toggle.
    pub fn toggle_mark_all(&mut self) {
        let all_marked = self
            .view_order
            .iter()
            .all(|tree_idx| self.marked.contains(tree_idx));
        if all_marked {
            self.marked.clear();
        } else {
            self.marked.extend(self.view_order.iter().copied());
        }
    }

    pub fn marked_size(&self) -> u64 {
        self.marked
            .iter()
            .filter_map(|&i| self.current().children.get(i))
            .map(|c| c.size)
            .sum()
    }

    pub fn enter(&mut self) {
        let Some(&tree_idx) = self.view_order.get(self.selected) else {
            return;
        };
        if !self.current().children[tree_idx].is_dir {
            return;
        }
        self.stack.push(tree_idx);
        self.saved_selection.push(self.selected);
        self.selected = 0;
        self.list_offset = 0;
        self.marked.clear();
        self.view_order.clear();
        self.refresh_view();
    }

    pub fn go_up(&mut self) {
        if self.stack.pop().is_some() {
            self.selected = self.saved_selection.pop().unwrap_or(0);
            self.marked.clear();
            self.view_order.clear();
            self.refresh_view();
        }
    }

    pub fn cycle_sort(&mut self) {
        self.sort = self.sort.next();
        self.refresh_view();
    }

    pub fn rescan(&mut self) {
        let root_name = std::mem::take(&mut self.root.name);
        self.root = Node {
            name: root_name,
            size: 0,
            is_dir: true,
            n_files: 0,
            children: Vec::new(),
        };
        self.scanning = true;
        self.stack.clear();
        self.saved_selection.clear();
        self.selected = 0;
        self.list_offset = 0;
        self.marked.clear();
        self.undo.clear();
        self.view_order.clear();
        self.overlay = Overlay::None;
        self.status = None;
        self.progress = Arc::new(Progress::default());
        self.scan_started = Instant::now();
        // Dropping the old receiver makes the previous scan thread's
        // sends fail, so it unwinds and exits on its own.
        self.scan_rx = start_scan(self.root_path.clone(), Arc::clone(&self.progress));
        self.disk = disk_stats(&self.root_path);
    }

    pub fn request_delete(&mut self, permanent: bool) {
        if self.scanning {
            self.status = Some(Status {
                text: "Deleting is disabled while scanning — wait or press r to rescan later"
                    .into(),
                is_error: true,
            });
            return;
        }
        if self.delete_summary().is_some() {
            self.overlay = if permanent {
                Overlay::ConfirmDelete
            } else {
                Overlay::ConfirmTrash
            };
        }
    }

    /// What a delete would act on: (count, total bytes, single name if the
    /// action targets exactly one entry). Marks take priority over the
    /// cursor; None means there is nothing to delete.
    pub fn delete_summary(&self) -> Option<(usize, u64, Option<String>)> {
        if !self.marked.is_empty() {
            let count = self
                .marked
                .iter()
                .filter(|&&i| i < self.current().children.len())
                .count();
            if count == 0 {
                return None;
            }
            Some((count, self.marked_size(), None))
        } else {
            let child = self.selected_node()?;
            Some((1, child.size, Some(child.name.clone())))
        }
    }

    /// Tree indices the next delete will act on, ascending.
    fn delete_targets(&self) -> Vec<usize> {
        if !self.marked.is_empty() {
            let mut v: Vec<usize> = self
                .marked
                .iter()
                .copied()
                .filter(|&i| i < self.current().children.len())
                .collect();
            v.sort_unstable();
            v
        } else if let Some(&tree_idx) = self.view_order.get(self.selected) {
            vec![tree_idx]
        } else {
            Vec::new()
        }
    }

    pub fn confirm_delete(&mut self) {
        let permanent = self.overlay == Overlay::ConfirmDelete;
        self.overlay = Overlay::None;
        let targets = self.delete_targets();
        if targets.is_empty() {
            return;
        }

        let dir_path = self.current_path();
        let parent_names = self.current_names();

        // Phase 1: do the filesystem work, recording which entries went.
        struct Done {
            tree_idx: usize,
            trashed: Option<PathBuf>,
            original: PathBuf,
        }
        let mut done: Vec<Done> = Vec::new();
        let mut n_fail = 0u64;
        let mut last_err: Option<String> = None;

        for &tree_idx in &targets {
            let Some(child) = self.current().children.get(tree_idx) else {
                continue;
            };
            let name = child.name.clone();
            let path = dir_path.join(&name);

            // Gone on disk already (removed outside wims): just drop it
            // from the view, nothing to trash or undo.
            if path.symlink_metadata().is_err() {
                done.push(Done { tree_idx, trashed: None, original: path });
                continue;
            }

            let outcome = if permanent {
                let r = if path.is_dir() && !path.is_symlink() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                r.map(|()| None)
            } else {
                trash_and_track(&path)
            };
            match outcome {
                Ok(trashed) => done.push(Done { tree_idx, trashed, original: path }),
                Err(e) => {
                    n_fail += 1;
                    last_err = Some(format!("{name}: {e}"));
                }
            }
        }

        // Phase 2: extract the removed subtrees (highest index first so
        // earlier indices stay valid), then shrink the ancestor totals.
        let mut removed: std::collections::HashMap<usize, Node> = std::collections::HashMap::new();
        {
            let mut desc: Vec<usize> = done.iter().map(|d| d.tree_idx).collect();
            desc.sort_unstable_by(|a, b| b.cmp(a));
            let node = self.current_dir_mut();
            for tree_idx in desc {
                if tree_idx < node.children.len() {
                    removed.insert(tree_idx, node.children.remove(tree_idx));
                }
            }
        }
        let freed: u64 = removed.values().map(|n| n.size).sum();
        let freed_files: u64 = removed.values().map(|n| n.n_files).sum();
        self.shrink_current_path(freed, freed_files);

        // Phase 3: build the undo batch (trash only) in original order.
        let mut undo_items: Vec<TrashedItem> = Vec::new();
        let mut single_name = String::new();
        for d in &done {
            if let Some(node) = removed.remove(&d.tree_idx) {
                single_name = node.name.clone();
                if !permanent {
                    if let Some(trashed) = &d.trashed {
                        undo_items.push(TrashedItem {
                            node,
                            parent_names: parent_names.clone(),
                            original: d.original.clone(),
                            trashed: Some(trashed.clone()),
                        });
                    }
                }
            }
        }
        let undoable = !undo_items.is_empty();
        if undoable {
            self.undo.push(UndoBatch { items: undo_items });
        }

        // Finish: clear marks, keep the cursor in range, refresh totals.
        self.marked.clear();
        self.view_order.clear();
        self.refresh_view();
        self.selected = self.selected.min(self.view_order.len().saturating_sub(1));
        self.disk = disk_stats(&self.root_path);

        // Status line.
        let ok = done.len();
        self.status = Some(if ok == 0 {
            Status {
                text: last_err.map_or_else(
                    || "Nothing was removed".into(),
                    |e| format!("Failed to remove {e}"),
                ),
                is_error: true,
            }
        } else {
            let verb = if permanent { "Deleted" } else { "Trashed" };
            let mut text = if ok == 1 {
                format!("{verb} {single_name} ({})", human_size(freed))
            } else {
                format!("{verb} {ok} items ({})", human_size(freed))
            };
            if undoable {
                text.push_str(" — press u to undo");
            }
            if n_fail > 0 {
                text.push_str(&format!("; {n_fail} failed"));
            }
            Status { text, is_error: false }
        });
    }

    /// Restore the most recent trash action.
    pub fn undo(&mut self) {
        let Some(batch) = self.undo.pop() else {
            self.status = Some(Status {
                text: "Nothing to undo".into(),
                is_error: false,
            });
            return;
        };

        let mut restored = 0u64;
        let mut failed = 0u64;
        let mut bytes = 0u64;
        let mut last_name = String::new();
        for item in batch.items {
            let ok = match &item.trashed {
                Some(trashed) => restore_from_trash(trashed, &item.original).is_ok(),
                None => false,
            };
            if ok {
                restored += 1;
                bytes += item.node.size;
                last_name = item.node.name.clone();
                self.reinsert(&item.parent_names, item.node);
            } else {
                failed += 1;
            }
        }

        self.view_order.clear();
        self.refresh_view();
        self.disk = disk_stats(&self.root_path);

        self.status = Some(if restored == 0 {
            Status {
                text: "Could not restore — the items may have been emptied from the Trash".into(),
                is_error: true,
            }
        } else {
            let mut text = if restored == 1 {
                format!("Restored {last_name} ({})", human_size(bytes))
            } else {
                format!("Restored {restored} items ({})", human_size(bytes))
            };
            if failed > 0 {
                text.push_str(&format!("; {failed} could not be restored"));
            }
            Status { text, is_error: false }
        });
    }

    /// Subtract a size/file delta from the root down to the current dir.
    fn shrink_current_path(&mut self, size: u64, files: u64) {
        let stack = self.stack.clone();
        let mut node = &mut self.root;
        node.size = node.size.saturating_sub(size);
        node.n_files = node.n_files.saturating_sub(files);
        for &i in &stack {
            node = &mut node.children[i];
            node.size = node.size.saturating_sub(size);
            node.n_files = node.n_files.saturating_sub(files);
        }
    }

    /// Put a restored subtree back under its original parent directory and
    /// grow the ancestor totals. No-op in the tree if the parent is gone
    /// (the file is back on disk; a rescan will pick it up).
    fn reinsert(&mut self, parent_names: &[String], node: Node) {
        let Some(idxs) = self.resolve_names(parent_names) else {
            return;
        };
        let (size, files) = (node.size, node.n_files);
        let mut cur = &mut self.root;
        cur.size += size;
        cur.n_files += files;
        for &i in &idxs {
            cur = &mut cur.children[i];
            cur.size += size;
            cur.n_files += files;
        }
        cur.children.push(node);
    }

    pub fn open_preview(&mut self) {
        let Some(child) = self.selected_node() else { return };
        if child.is_dir {
            self.status = Some(Status {
                text: "Preview works on files — open the folder instead".into(),
                is_error: true,
            });
            return;
        }
        let name = child.name.clone();
        let path = self.current_path().join(&name);

        let result = if is_image_name(&name) {
            if child.size > MAX_PREVIEW_BYTES {
                Err("too large to preview".to_string())
            } else {
                image_preview(&path)
            }
        } else {
            text_preview(&path, self.light_bg)
        };
        match result {
            Ok(content) => self.preview = Some(Preview { name, content }),
            Err(e) => {
                self.status = Some(Status {
                    text: format!("Cannot preview {name}: {e}"),
                    is_error: true,
                });
            }
        }
    }

    pub fn reveal_in_finder(&mut self) {
        let Some(child) = self.selected_node() else { return };
        let path = self.current_path().join(&child.name);
        let opened = std::process::Command::new("open")
            .arg("-R")
            .arg(&path)
            .status();
        if opened.is_err() {
            self.status = Some(Status {
                text: "Could not open Finder".into(),
                is_error: true,
            });
        }
    }
}

fn image_preview(path: &std::path::Path) -> Result<PreviewContent, String> {
    let img = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .decode()
        .map_err(|e| e.to_string())?;
    let dims = (img.width(), img.height());
    // Size the thumbnail to what fits on screen right now; the popup is
    // then sized to the thumbnail, not the other way around.
    let (term_w, term_h) = ratatui::crossterm::terminal::size().unwrap_or((100, 40));
    let max_cols = (term_w as u32 * 5 / 6).saturating_sub(6).max(16);
    let max_rows = (term_h as u32 * 5 / 6).saturating_sub(4).max(8);
    let cells = render_thumbnail(&img, max_cols, max_rows);
    Ok(PreviewContent::Image { dims, cells })
}

fn text_preview(path: &std::path::Path, light_bg: bool) -> Result<PreviewContent, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    file.take(MAX_TEXT_PREVIEW_BYTES)
        .read_to_end(&mut buf)
        .map_err(|e| e.to_string())?;
    // NUL bytes near the start mean this isn't text (pdf, docx, zip, …).
    if buf[..buf.len().min(8192)].contains(&0) {
        return Err("binary file".into());
    }
    let text = String::from_utf8_lossy(&buf);
    let lines = highlight_lines(path, &text, light_bg);
    let truncated =
        buf.len() as u64 >= MAX_TEXT_PREVIEW_BYTES || text.lines().count() > lines.len();
    Ok(PreviewContent::Text { lines, truncated })
}

/// Syntax assets are loaded once; App::new warms this in the background
/// so the first preview doesn't stall while the syntax set links. The
/// highlight theme is picked to be readable on the detected background.
pub fn syntax_assets(light_bg: bool) -> &'static (SyntaxSet, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        // The bundled Sublime packages predate Swift; add our own.
        let mut builder = SyntaxSet::load_defaults_newlines().into_builder();
        if let Ok(swift) = syntect::parsing::SyntaxDefinition::load_from_str(
            include_str!("../assets/Swift.sublime-syntax"),
            true,
            None,
        ) {
            builder.add(swift);
        }
        let syntaxes = builder.build();
        let mut themes = ThemeSet::load_defaults();
        let name = if light_bg {
            "InspiredGitHub"
        } else {
            "base16-eighties.dark"
        };
        let theme = themes
            .themes
            .remove(name)
            .or_else(|| themes.themes.into_values().next())
            .unwrap_or_default();
        (syntaxes, theme)
    })
}

/// Languages missing from the bundled syntaxes, mapped to close-enough
/// grammars that are present.
fn alias_extension(ext: &str) -> Option<&'static str> {
    match ext {
        "ts" | "mts" | "cts" | "jsx" | "tsx" => Some("js"),
        "kt" | "kts" => Some("java"),
        "vue" | "svelte" => Some("html"),
        "zsh" | "fish" => Some("sh"),
        "toml" | "ini" | "conf" => Some("yaml"),
        _ => None,
    }
}

/// Style the file's first lines. Language is picked by extension, then by
/// first line (shebangs); unrecognized files stay plain.
fn highlight_lines(path: &std::path::Path, text: &str, light_bg: bool) -> Vec<Line<'static>> {
    let (syntaxes, theme) = syntax_assets(light_bg);
    let ext = path.extension().and_then(|e| e.to_str());
    let syntax = ext
        .and_then(|e| syntaxes.find_syntax_by_extension(e))
        .or_else(|| {
            ext.and_then(alias_extension)
                .and_then(|e| syntaxes.find_syntax_by_extension(e))
        })
        .or_else(|| {
            text.lines()
                .next()
                .and_then(|first| syntaxes.find_syntax_by_first_line(first))
        });

    let Some(syntax) = syntax else {
        return text
            .lines()
            .take(MAX_TEXT_PREVIEW_LINES)
            .map(|l| Line::raw(l.replace('\t', "    ")))
            .collect();
    };

    let mut highlighter = HighlightLines::new(syntax, theme);
    text.lines()
        .take(MAX_TEXT_PREVIEW_LINES)
        .map(|line| {
            let expanded = line.replace('\t', "    ");
            match highlighter.highlight_line(&expanded, syntaxes) {
                Ok(regions) => Line::from(
                    regions
                        .iter()
                        .map(|(style, chunk)| {
                            Span::styled(chunk.to_string(), convert_style(*style))
                        })
                        .collect::<Vec<_>>(),
                ),
                Err(_) => Line::raw(expanded),
            }
        })
        .collect()
}

/// Map a syntect style to a ratatui one. Only the foreground is kept so
/// the preview sits on the terminal's own background.
fn convert_style(style: syntect::highlighting::Style) -> Style {
    let fg = style.foreground;
    let mut out = Style::new().fg(Color::Rgb(fg.r, fg.g, fg.b));
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

/// Downscale the image to fit max_cols × max_rows character cells and
/// convert it to half-block cells (two pixels per cell, one via the
/// foreground color and one via the background). Transparent pixels fall
/// back to the terminal's default background.
fn render_thumbnail(img: &image::DynamicImage, max_cols: u32, max_rows: u32) -> Vec<Vec<ThumbCell>> {
    let rgba = img.thumbnail(max_cols, max_rows * 2).to_rgba8();
    let (w, h) = rgba.dimensions();
    let mut cells = Vec::with_capacity(h.div_ceil(2) as usize);
    for cell_y in 0..h.div_ceil(2) {
        let mut row = Vec::with_capacity(w as usize);
        for x in 0..w {
            let top = opaque_color(rgba.get_pixel(x, cell_y * 2));
            let bottom = if cell_y * 2 + 1 < h {
                opaque_color(rgba.get_pixel(x, cell_y * 2 + 1))
            } else {
                None
            };
            row.push(match (top, bottom) {
                (Some(t), Some(b)) => ThumbCell { ch: '▀', fg: t, bg: b },
                (Some(t), None) => ThumbCell { ch: '▀', fg: t, bg: Color::Reset },
                (None, Some(b)) => ThumbCell { ch: '▄', fg: b, bg: Color::Reset },
                (None, None) => ThumbCell { ch: ' ', fg: Color::Reset, bg: Color::Reset },
            });
        }
        cells.push(row);
    }
    cells
}

fn opaque_color(px: &image::Rgba<u8>) -> Option<Color> {
    (px[3] >= 128).then(|| Color::Rgb(px[0], px[1], px[2]))
}

fn is_image_name(name: &str) -> bool {
    std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| IMAGE_EXTENSIONS.contains(&e.to_lowercase().as_str()))
}

/// Move a path to the Trash and report where it landed, so undo can put it
/// back.
///
/// On macOS we call `NSFileManager` directly rather than through the
/// `trash` crate: its default method drives Finder over AppleScript (slow,
/// needs automation permission, and errors on some paths), and crucially
/// only the raw API hands back the resulting in-Trash URL — which is the
/// one reliable way to locate the item afterward for undo.
#[cfg(target_os = "macos")]
fn trash_and_track(path: &Path) -> Result<Option<PathBuf>, std::io::Error> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};
    let path_str = path
        .to_str()
        .ok_or_else(|| std::io::Error::other("path is not valid UTF-8"))?;
    let ns_path = NSString::from_str(path_str);
    let url = NSURL::fileURLWithPath(&ns_path);
    let manager = NSFileManager::defaultManager();
    // Filled in with the item's new location inside the Trash.
    let mut resulting = None;
    manager
        .trashItemAtURL_resultingItemURL_error(&url, Some(&mut resulting))
        .map_err(|e| std::io::Error::other(e.localizedDescription().to_string()))?;
    let trashed = resulting
        .and_then(|u| u.path())
        .map(|p| PathBuf::from(p.to_string()));
    Ok(trashed)
}

#[cfg(not(target_os = "macos"))]
fn trash_and_track(path: &Path) -> Result<Option<PathBuf>, std::io::Error> {
    trash::delete(path).map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(None)
}

/// Move a tracked item out of the Trash back to where it came from.
fn restore_from_trash(trashed: &Path, original: &Path) -> std::io::Result<()> {
    if original.symlink_metadata().is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "something already exists at the original path",
        ));
    }
    if let Some(parent) = original.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
        }
    }
    // The Trash lives on the same volume as the original, so rename works;
    // fall back to a copy across volumes just in case.
    match std::fs::rename(trashed, original) {
        Ok(()) => Ok(()),
        Err(_) => {
            if trashed.is_dir() {
                copy_dir_all(trashed, original)?;
                std::fs::remove_dir_all(trashed)
            } else {
                std::fs::copy(trashed, original)?;
                std::fs::remove_file(trashed)
            }
        }
    }
}

fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

fn start_scan(path: PathBuf, progress: Arc<Progress>) -> Receiver<ScanMsg> {
    let (tx, rx) = channel();
    std::thread::spawn(move || scan_live(&path, tx, &progress));
    rx
}

fn sorted_indices(node: &Node, mode: SortMode) -> Vec<usize> {
    let c = &node.children;
    let mut idx: Vec<usize> = (0..c.len()).collect();
    match mode {
        SortMode::Size => {
            idx.sort_by(|&a, &b| c[b].size.cmp(&c[a].size).then_with(|| c[a].name.cmp(&c[b].name)))
        }
        SortMode::Name => idx.sort_by(|&a, &b| {
            c[b].is_dir
                .cmp(&c[a].is_dir)
                .then_with(|| c[a].name.to_lowercase().cmp(&c[b].name.to_lowercase()))
        }),
        SortMode::Files => idx.sort_by(|&a, &b| {
            c[b].n_files
                .cmp(&c[a].n_files)
                .then_with(|| c[b].size.cmp(&c[a].size))
        }),
    }
    idx
}

#[cfg(unix)]
fn disk_stats(path: &std::path::Path) -> Option<(u64, u64)> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    unsafe {
        let mut fs: libc::statfs = std::mem::zeroed();
        if libc::statfs(c_path.as_ptr(), &mut fs) != 0 {
            return None;
        }
        let bsize = fs.f_bsize as u64;
        Some((fs.f_blocks * bsize, fs.f_bavail * bsize))
    }
}

#[cfg(not(unix))]
fn disk_stats(_path: &std::path::Path) -> Option<(u64, u64)> {
    None
}

pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swift_syntax_loads_and_highlights() {
        let (syntaxes, _) = syntax_assets(false);
        assert!(
            syntaxes.find_syntax_by_extension("swift").is_some(),
            "embedded Swift grammar failed to load"
        );

        let code = "func greet(name: String) -> String {\n    // say hi\n    return \"hello \\(name)\"\n}\n";
        let lines = highlight_lines(std::path::Path::new("test.swift"), code, false);
        assert_eq!(lines.len(), 4);
        // The `func` keyword must come out styled, not plain.
        let styled_spans = lines[0]
            .spans
            .iter()
            .filter(|s| s.style.fg.is_some())
            .count();
        assert!(styled_spans > 0, "Swift code was not highlighted");
    }

    #[test]
    fn aliased_extensions_highlight() {
        let (syntaxes, _) = syntax_assets(false);
        for ext in ["ts", "kt", "toml"] {
            let alias = alias_extension(ext).unwrap();
            assert!(
                syntaxes.find_syntax_by_extension(alias).is_some(),
                "alias target {alias} for .{ext} is missing"
            );
        }
    }
}

pub fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}
