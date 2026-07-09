# wims — where is my space

A fast, keyboard-driven terminal UI for finding out what's eating your disk
and cleaning it up.

```
╭ ◆ wims · where is my space ────────────────────────────────────────────────────────╮
│ ▸ /Users/you/Library/Caches                                                        │
│ volume ██████████████████████████████████░░░░░ 15.0 GB free of 228 GB              │
╰────────────────────────────────────────────────────────────────────────────────────╯
╭ 2.2 GB · 133 entries · 46,987 files ───────────────────────────────────────────────╮
│ ▸ com.spotify.client/               █████████░░░░░░░   1.3 GB   57.7%    37,282    │
│ ▸ pypoetry/                         ███░░░░░░░░░░░░░   361 MB   15.8%     2,421    │
│ ▸ Google/                           █░░░░░░░░░░░░░░░   160 MB    7.0%     3,050    │
│ ▸ Homebrew/                         █░░░░░░░░░░░░░░░   147 MB    6.5%     1,753    │
│ ...                                                                                │
╰──────────────────────────────────────────────────────────────────────── sort: size ╯
 Scanned 46,987 files (2.2 GB) in 0.2s
 ↑↓ move  ⏎ open  ⌫ back  d trash  D delete  s sort  r rescan  q quit
```

## Features

- **Live-updating scan** — the tree is browsable the instant the app opens;
  entries appear and folder sizes tick upward while the parallel scan
  (powered by rayon) is still running.
- **Visual size breakdown** — every entry gets a usage bar and its share of
  the parent folder, heat-colored green → yellow → red so the big offenders
  jump out.
- **Volume gauge** — the header shows how full the disk holding the scanned
  folder is, updated after every deletion.
- **Batch cleanup with undo** — mark several entries with `Space` (or `a`
  to mark the whole folder) and clear them in one confirmed action. `d`
  moves to the Trash, `D` deletes permanently; both ask first. Trashed a
  folder by mistake? Press `u` to put it and everything in it back exactly
  where it was. On macOS, trashing uses the native `NSFileManager` API —
  no AppleScript, no automation prompts — and captures each item's Trash
  location so undo is reliable.
- **File preview** — press `p` to peek at a file before deciding its
  fate. Images are rendered as color thumbnails (works in any terminal,
  including the VS Code integrated terminal — no graphics protocol
  needed); text files (code, markdown, json, logs, …) show their first
  lines with line numbers. Binary formats are politely refused.
- **Adapts to your terminal's colors** — at startup wims asks the
  terminal for its background color (OSC 11, with `COLORFGBG` as
  fallback) and picks a light or dark syntax theme and selection color
  accordingly, so previews stay readable everywhere. Override with
  `--light` / `--dark` if the guess is wrong.
- **Real disk usage** — sizes are allocated blocks (like `du`), not
  apparent file length, so sparse files don't lie to you.
- **Symlink-safe** — links are never followed, so nothing outside the
  scanned folder is counted or deleted through a link.

## Install

Requires a Rust toolchain ([rustup.rs](https://rustup.rs)).

```sh
cargo install wims
```

## Usage

```sh
wims [--light|--dark] [directory]
```

With no argument, wims scans the current directory. Scanning starts
immediately and the view fills in live; the footer shows progress and
switches to a summary when the scan completes.

### Keys

| Key | Action |
| --- | --- |
| `↑` `↓` or `k` `j` | move selection |
| `⏎` / `→` / `l` | open the selected folder |
| `⌫` / `←` / `h` | back to parent folder |
| `g` / `G` | jump to top / bottom |
| `f` / `b` or `PgDn` / `PgUp` | page down / up |
| `Space` | mark / unmark the selected entry |
| `a` | mark / unmark every entry in the folder |
| `p` | preview the selected file — image or text (any key closes) |
| `d` | move marked (or selected) to Trash (confirm with `y`) |
| `D` | delete marked (or selected) permanently (confirm with `y`) |
| `u` | undo the last trash (restores files from the Trash) |
| `s` | cycle sort: size → name → files |
| `o` | reveal the selected entry in Finder |
| `r` | rescan from scratch |
| `q` | quit |
| `Esc` | quit (press twice to confirm) |

## How it works

- A background thread walks the directory tree in parallel with rayon.
  As each directory is enumerated, its entries are streamed over a channel
  to the UI, which merges them into the tree between frames and propagates
  the new bytes up the ancestor chain — that's what makes totals grow live.
- Children are kept in arrival order; sorting is a per-frame index over the
  current directory, and the cursor follows the entry it's on rather than
  its row, so the selection doesn't jump while entries reorder mid-scan.
- Unreadable directories (permissions) are shown as empty rather than
  aborting the scan.
- Input is drained in batches (a trackpad flick lands in one redraw, not
  hundreds) and only the visible rows are built each frame, so directories
  with tens of thousands of entries stay smooth to scroll.

## Notes

- Deleting is disabled while a scan is in progress; navigation and sorting
  work live. Wait for the footer summary, or press `r` later to rescan.
- Marks are per-folder: they belong to the directory you're in and clear
  when you move into or out of it. `u` can be pressed repeatedly to undo
  earlier trash actions in turn; a permanent delete (`D`) can't be undone.
- The view is a snapshot from scan time. If something was removed outside
  wims, deleting it just updates the view instead of erroring.
- Hard links are counted once per link (like `du` without `-l`
  deduplication), so totals can slightly overstate multi-linked content.

