mod app;
mod scanner;
mod ui;

use app::{App, Overlay};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use std::path::PathBuf;
use std::time::Duration;

fn main() -> std::io::Result<()> {
    let mut forced_light: Option<bool> = None;
    let mut dir: Option<String> = None;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!(
                    "wims — where is my space\n\n\
                     usage: wims [--light|--dark] [directory]\n\n\
                     Scans the directory (default: current) and opens an interactive\n\
                     view of what is using disk space.\n\n\
                     --light / --dark   override terminal background detection"
                );
                return Ok(());
            }
            "--light" => forced_light = Some(true),
            "--dark" => forced_light = Some(false),
            other => dir = Some(other.to_string()),
        }
    }
    let arg = dir.unwrap_or_else(|| ".".into());
    let path = match PathBuf::from(&arg).canonicalize() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("wims: {arg}: {e}");
            std::process::exit(1);
        }
    };
    if !path.is_dir() {
        eprintln!("wims: {}: not a directory", path.display());
        std::process::exit(1);
    }

    // Must happen before entering the alternate screen: the terminal is
    // asked for its background color so themes stay readable on both
    // light and dark terminals.
    let light_bg =
        forced_light.unwrap_or_else(|| detect_light_background().unwrap_or(false));

    let mut terminal = ratatui::init();
    let mut app = App::new(path, light_bg);
    let result = run(&mut terminal, &mut app);
    ratatui::restore();
    result
}

/// Ask the terminal whether its background is light. OSC 11 first (most
/// modern terminals answer), COLORFGBG as fallback, None when unknown.
fn detect_light_background() -> Option<bool> {
    if unsafe { libc::isatty(0) } == 0 {
        return None;
    }
    query_osc11_background().or_else(colorfgbg_background)
}

fn query_osc11_background() -> Option<bool> {
    use ratatui::crossterm::terminal;
    use std::io::Write;

    terminal::enable_raw_mode().ok()?;
    let result = (|| {
        let mut out = std::io::stdout();
        // OSC 11 background query, then DA1 as a fence: every terminal
        // answers DA1, so a DA1-only reply means OSC 11 is unsupported.
        out.write_all(b"\x1b]11;?\x1b\\\x1b[c").ok()?;
        out.flush().ok()?;

        let mut buf = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
            let mut pfd = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as i32) } <= 0 {
                return None;
            }
            let mut chunk = [0u8; 256];
            let n = unsafe { libc::read(0, chunk.as_mut_ptr().cast(), chunk.len()) };
            if n <= 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n as usize]);
            if let Some(light) = parse_osc11(&buf) {
                return Some(light);
            }
            // DA1 reply arrived without an OSC 11 reply before it.
            if buf.ends_with(b"c") && !buf.contains(&b']') {
                return None;
            }
        }
    })();
    let _ = terminal::disable_raw_mode();
    result
}

/// Extract "rgb:RRRR/GGGG/BBBB" from an OSC 11 reply and judge luminance.
fn parse_osc11(buf: &[u8]) -> Option<bool> {
    let s = String::from_utf8_lossy(buf);
    let after = &s[s.find("]11;")? + 4..];
    let rgb = &after[after.find("rgb:")? + 4..];
    let mut channels = [0u8; 3];
    let mut parts = rgb.split(['/', '\x07', '\x1b']);
    for value in channels.iter_mut() {
        let part = parts.next()?;
        if part.len() < 2 || !part.is_char_boundary(2) {
            return None;
        }
        *value = u8::from_str_radix(&part[..2], 16).ok()?;
    }
    let luminance = 0.2126 * channels[0] as f64
        + 0.7152 * channels[1] as f64
        + 0.0722 * channels[2] as f64;
    Some(luminance > 127.5)
}

fn colorfgbg_background() -> Option<bool> {
    let var = std::env::var("COLORFGBG").ok()?;
    let bg: u8 = var.rsplit(';').next()?.parse().ok()?;
    Some(bg == 7 || bg == 15)
}

fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> std::io::Result<()> {
    loop {
        app.poll_scan();
        terminal.draw(|f| ui::draw(f, app))?;

        app.tick += 1;
        if !event::poll(Duration::from_millis(80))? {
            continue;
        }
        // Drain everything queued before redrawing: trackpad scrolling
        // floods the input with arrow events, and one frame per event
        // makes large directories feel laggy.
        loop {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && handle_key(app, key.code, key.modifiers) {
                    return Ok(());
                }
            }
            if !event::poll(Duration::ZERO)? {
                break;
            }
        }
    }
}

/// Returns true when the app should quit.
fn handle_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
    let ctrl_c = code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL);

    // An open image preview closes on any key.
    if app.preview.is_some() {
        app.preview = None;
        if !ctrl_c {
            return false;
        }
    }

    // Confirmation dialog captures all input.
    if app.overlay != Overlay::None {
        match code {
            KeyCode::Char('y') | KeyCode::Enter => app.confirm_delete(),
            _ => app.overlay = Overlay::None,
        }
        return false;
    }

    // Any key other than Esc cancels a pending Esc-to-quit.
    if code != KeyCode::Esc {
        app.pending_quit = false;
    }

    match code {
        _ if ctrl_c => return true,
        KeyCode::Char('q') => return true,
        KeyCode::Esc => {
            if app.pending_quit {
                return true;
            }
            app.pending_quit = true;
        }
        KeyCode::Down | KeyCode::Char('j') => app.move_selection(1),
        KeyCode::Up | KeyCode::Char('k') => app.move_selection(-1),
        KeyCode::PageDown | KeyCode::Char('f') => app.move_selection(15),
        KeyCode::PageUp | KeyCode::Char('b') => app.move_selection(-15),
        KeyCode::Home | KeyCode::Char('g') => app.select_first(),
        KeyCode::End | KeyCode::Char('G') => app.select_last(),
        KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => app.enter(),
        KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => app.go_up(),
        KeyCode::Char(' ') => app.toggle_mark(),
        KeyCode::Char('a') => app.toggle_mark_all(),
        KeyCode::Char('d') => app.request_delete(false),
        KeyCode::Char('D') => app.request_delete(true),
        KeyCode::Char('u') => app.undo(),
        KeyCode::Char('s') => app.cycle_sort(),
        KeyCode::Char('r') => app.rescan(),
        KeyCode::Char('o') => app.reveal_in_finder(),
        KeyCode::Char('p') => app.open_preview(),
        _ => {}
    }
    false
}
