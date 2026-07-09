use crate::app::{group_digits, human_size, App, Overlay, PreviewContent};
use ratatui::layout::{Alignment, Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Clear, List, ListItem, ListState, Padding, Paragraph, Scrollbar,
    ScrollbarOrientation, ScrollbarState,
};
use ratatui::Frame;
use std::sync::atomic::Ordering;

const ACCENT: Color = Color::Cyan;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const BAR_WIDTH: usize = 16;

pub fn draw(f: &mut Frame, app: &mut App) {
    let [header, main, footer] =
        Layout::vertical([Constraint::Length(4), Constraint::Min(3), Constraint::Length(2)])
            .areas(f.area());

    draw_header(f, app, header);
    draw_list(f, app, main);
    draw_footer(f, app, footer);

    match app.overlay {
        Overlay::ConfirmTrash => draw_confirm(f, app, false),
        Overlay::ConfirmDelete => draw_confirm(f, app, true),
        Overlay::None => {}
    }

    if app.preview.is_some() {
        draw_preview(f, app);
    }
}

fn draw_preview(f: &mut Frame, app: &App) {
    let Some(preview) = app.preview.as_ref() else { return };
    let screen = f.area();

    let (width, height, detail) = match &preview.content {
        // The popup hugs the thumbnail; only a minimum width for the title.
        PreviewContent::Image { dims, cells } => {
            let img_cols = cells.first().map_or(0, |row| row.len()) as u16;
            let img_rows = cells.len() as u16;
            (
                (img_cols + 2).max(30).min(screen.width),
                (img_rows + 2).min(screen.height),
                format!("{}×{} ", dims.0, dims.1),
            )
        }
        PreviewContent::Text { lines, truncated } => (
            (screen.width * 5 / 6).max(30).min(screen.width),
            (screen.height * 5 / 6).max(10).min(screen.height),
            format!(
                "{}{} lines ",
                lines.len(),
                if *truncated { "+" } else { "" }
            ),
        ),
    };

    let area = centered_rect(screen, width, height);
    f.render_widget(Clear, area);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::from(vec![
            Span::styled(format!(" {} ", preview.name), Style::new().fg(ACCENT).bold()),
            Span::styled(detail, Style::new().dim()),
        ]))
        .title_bottom(
            Line::from(Span::styled(" any key to close ", Style::new().dim().italic()))
                .right_aligned(),
        );
    let inner = block.inner(area);
    f.render_widget(block, area);

    match &preview.content {
        PreviewContent::Image { cells, .. } => draw_thumbnail(f, inner, cells),
        PreviewContent::Text { lines, .. } => draw_text_preview(f, inner, lines),
    }
}

fn draw_thumbnail(f: &mut Frame, inner: Rect, cells: &[Vec<crate::app::ThumbCell>]) {
    let img_cols = cells.first().map_or(0, |row| row.len()) as u16;
    let img_rows = cells.len() as u16;
    let x0 = inner.x + inner.width.saturating_sub(img_cols) / 2;
    let y0 = inner.y + inner.height.saturating_sub(img_rows) / 2;
    let buf = f.buffer_mut();
    for (dy, row) in cells.iter().enumerate() {
        let y = y0 + dy as u16;
        if y >= inner.bottom() {
            break;
        }
        for (dx, cell) in row.iter().enumerate() {
            let x = x0 + dx as u16;
            if x >= inner.right() {
                break;
            }
            if let Some(c) = buf.cell_mut((x, y)) {
                c.set_char(cell.ch).set_fg(cell.fg).set_bg(cell.bg);
            }
        }
    }
}

fn draw_text_preview(f: &mut Frame, inner: Rect, lines: &[Line<'static>]) {
    if lines.is_empty() {
        let msg = Paragraph::new(Line::styled("· empty file ·", Style::new().dim()))
            .alignment(Alignment::Center);
        f.render_widget(msg, center_vertically(inner, 1));
        return;
    }
    let gutter = lines.len().to_string().len().max(3);
    let text: Vec<Line> = lines
        .iter()
        .take(inner.height as usize)
        .enumerate()
        .map(|(i, line)| {
            let mut spans = vec![Span::styled(
                format!("{:>gutter$} │ ", i + 1),
                Style::new().dim(),
            )];
            spans.extend(line.spans.iter().cloned());
            Line::from(spans)
        })
        .collect();
    f.render_widget(Paragraph::new(text), inner);
}

fn draw_header(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(ACCENT))
        .title(Line::from(vec![
            Span::styled(" ◆ wims ", Style::new().fg(ACCENT).bold()),
            Span::styled("· where is my space ", Style::new().dim()),
        ]))
        .padding(Padding::horizontal(1));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Line 1: breadcrumb of the directory being viewed.
    let path = app.current_path().display().to_string();
    let path = truncate_left(&path, inner.width.saturating_sub(2) as usize);
    let breadcrumb = Line::from(vec![
        Span::styled("▸ ", Style::new().fg(ACCENT)),
        Span::styled(path, Style::new().bold()),
    ]);

    // Line 2: volume usage gauge.
    let gauge = match app.disk {
        Some((total, avail)) if total > 0 => {
            let used = total - avail;
            let ratio = used as f64 / total as f64;
            let width = inner.width.saturating_sub(34).max(10) as usize;
            let filled = (ratio * width as f64).round() as usize;
            let color = if ratio > 0.9 {
                Color::Red
            } else if ratio > 0.75 {
                Color::Yellow
            } else {
                Color::Green
            };
            Line::from(vec![
                Span::styled("volume ", Style::new().dim()),
                Span::styled("█".repeat(filled.min(width)), Style::new().fg(color)),
                Span::styled("░".repeat(width - filled.min(width)), Style::new().dim()),
                Span::raw(" "),
                Span::styled(human_size(avail), Style::new().fg(color).bold()),
                Span::styled(format!(" free of {}", human_size(total)), Style::new().dim()),
            ])
        }
        _ => Line::from(Span::styled("volume stats unavailable", Style::new().dim())),
    };

    f.render_widget(Paragraph::new(vec![breadcrumb, gauge]), inner);
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let current = app.current();
    let mut title = vec![
        Span::raw(" "),
        Span::styled(human_size(current.size), Style::new().fg(ACCENT).bold()),
        Span::styled(
            format!(
                " · {} entries · {} files ",
                group_digits(current.children.len() as u64),
                group_digits(current.n_files)
            ),
            Style::new().dim(),
        ),
    ];
    if !app.marked.is_empty() {
        title.push(Span::styled(
            format!("· {} marked ({}) ", app.marked.len(), human_size(app.marked_size())),
            Style::new().fg(Color::Green).bold(),
        ));
    }
    let title = Line::from(title);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().dim())
        .title(title)
        .title_bottom(
            Line::from(Span::styled(
                format!(" sort: {} ", app.sort.label()),
                Style::new().dim().italic(),
            ))
            .right_aligned(),
        );
    let inner = block.inner(area);
    f.render_widget(block, area);

    if app.view_order.is_empty() {
        let text = if app.scanning { "· scanning… ·" } else { "· empty ·" };
        let msg = Paragraph::new(Line::styled(text, Style::new().dim()))
            .alignment(Alignment::Center);
        f.render_widget(msg, center_vertically(inner, 1));
        return;
    }

    // Build rows only for the visible window; large directories would
    // otherwise pay for formatting every entry on every frame.
    let height = inner.height as usize;
    app.ensure_visible(height);
    let current = app.current();
    let total = app.view_order.len();
    let offset = app.list_offset;
    let end = (offset + height).min(total);

    let parent_size = current.size.max(1);
    let items: Vec<ListItem> = app.view_order[offset..end]
        .iter()
        .map(|&i| {
            let marked = app.marked.contains(&i);
            ListItem::new(entry_line(&current.children[i], parent_size, inner.width, marked))
        })
        .collect();

    // A dark-gray bar is invisible text-on-dark on light terminals.
    let highlight_bg = if app.light_bg {
        Color::Rgb(208, 208, 208)
    } else {
        Color::DarkGray
    };
    let list = List::new(items)
        .highlight_style(Style::new().bg(highlight_bg).add_modifier(Modifier::BOLD));
    let mut state = ListState::default().with_selected(Some(app.selected - offset));
    f.render_stateful_widget(list, inner, &mut state);

    if total > height {
        let mut scrollbar = ScrollbarState::new(total).position(app.selected);
        f.render_stateful_widget(
            Scrollbar::new(ScrollbarOrientation::VerticalRight),
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut scrollbar,
        );
    }
}

/// One row: mark, name, usage bar, size, share of parent, file count.
fn entry_line(
    child: &crate::scanner::Node,
    parent_size: u64,
    width: u16,
    marked: bool,
) -> Line<'static> {
    let pct = child.size as f64 / parent_size as f64 * 100.0;
    let heat = if pct >= 40.0 {
        Color::Red
    } else if pct >= 15.0 {
        Color::Yellow
    } else {
        Color::Green
    };

    // Fixed-width right side: bar + size + percent + files.
    let bar_filled = ((pct / 100.0) * BAR_WIDTH as f64).round() as usize;
    let files_col = if child.is_dir {
        format!("{:>10}", group_digits(child.n_files))
    } else {
        format!("{:>10}", "")
    };
    let right_width = BAR_WIDTH + 2 + 9 + 8 + 10 + 2;
    let name_width = (width as usize).saturating_sub(right_width + 3).max(8);

    let (icon, name_style) = if child.is_dir {
        ("▸ ", Style::new().fg(ACCENT).bold())
    } else {
        ("  ", Style::new())
    };
    let mut name = child.name.clone();
    if child.is_dir {
        name.push('/');
    }
    let name = format!("{:<name_width$}", truncate(&name, name_width));

    let mark = if marked {
        Span::styled("▌", Style::new().fg(Color::Green).bold())
    } else {
        Span::raw(" ")
    };

    Line::from(vec![
        mark,
        Span::styled(icon.to_string(), Style::new().fg(ACCENT)),
        Span::styled(name, name_style),
        Span::styled("█".repeat(bar_filled.min(BAR_WIDTH)), Style::new().fg(heat)),
        Span::styled(
            "░".repeat(BAR_WIDTH - bar_filled.min(BAR_WIDTH)),
            Style::new().dim(),
        ),
        Span::styled(format!("{:>9}", human_size(child.size)), Style::new().bold()),
        Span::styled(format!("{:>7.1}%", pct), Style::new().fg(heat)),
        Span::styled(files_col, Style::new().dim()),
        Span::raw(" "),
    ])
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let status = if app.pending_quit {
        Line::from(Span::styled(
            " Press Esc again to quit",
            Style::new().fg(Color::Yellow).bold(),
        ))
    } else if app.scanning {
        let spinner = SPINNER[(app.tick as usize) % SPINNER.len()];
        let files = app.progress.files.load(Ordering::Relaxed);
        let bytes = app.progress.bytes.load(Ordering::Relaxed);
        Line::from(vec![
            Span::styled(format!(" {spinner} scanning "), Style::new().fg(ACCENT).bold()),
            Span::styled(
                format!(
                    "· {} files · {} discovered",
                    group_digits(files),
                    human_size(bytes)
                ),
                Style::new().dim(),
            ),
        ])
    } else {
        match &app.status {
            Some(s) => Line::from(Span::styled(
                format!(" {}", s.text),
                if s.is_error {
                    Style::new().fg(Color::Red)
                } else {
                    Style::new().fg(Color::Green)
                },
            )),
            None => Line::raw(""),
        }
    };

    let keys: [(&str, &str); 11] = [
        ("↑↓", "move"),
        ("␣", "mark"),
        ("⏎", "open"),
        ("⌫", "back"),
        ("p", "preview"),
        ("d", "trash"),
        ("D", "delete"),
        ("u", "undo"),
        ("s", "sort"),
        ("r", "rescan"),
        ("q", "quit"),
    ];
    let mut spans = vec![Span::raw(" ")];
    for (key, label) in keys {
        spans.push(Span::styled(key, Style::new().fg(ACCENT).bold()));
        spans.push(Span::styled(format!(" {label}  "), Style::new().dim()));
    }

    let [status_area, keys_area] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    f.render_widget(Paragraph::new(status), status_area);
    f.render_widget(Paragraph::new(Line::from(spans)), keys_area);
}

fn draw_confirm(f: &mut Frame, app: &App, permanent: bool) {
    let Some((count, total, single)) = app.delete_summary() else {
        return;
    };

    let (title, color, verb) = if permanent {
        (" Delete permanently ", Color::Red, "permanently delete")
    } else {
        (" Move to Trash ", Color::Yellow, "move to Trash")
    };

    let area = centered_rect(f.area(), 56, 7);
    f.render_widget(Clear, area);

    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(color))
        .title(Span::styled(title, Style::new().fg(color).bold()))
        .padding(Padding::horizontal(2));
    let inner = block.inner(area);
    f.render_widget(block, area);

    // A single target names it; a batch shows the count.
    let subject = match &single {
        Some(name) => Span::styled(
            truncate(name, inner.width.saturating_sub(14) as usize),
            Style::new().bold(),
        ),
        None => Span::styled(format!("{count} items"), Style::new().bold()),
    };
    let lines = vec![
        Line::raw(""),
        Line::from(vec![
            Span::raw(format!("{verb} ")),
            subject,
            Span::styled(format!(" ({})?", human_size(total)), Style::new().dim()),
        ]),
        Line::raw(""),
        Line::from(vec![
            Span::styled("y", Style::new().fg(color).bold()),
            Span::styled(" confirm    ", Style::new().dim()),
            Span::styled("n", Style::new().fg(ACCENT).bold()),
            Span::styled(" cancel", Style::new().dim()),
        ]),
    ];
    f.render_widget(Paragraph::new(lines).alignment(Alignment::Center), inner);
}

fn center_vertically(area: Rect, content_height: u16) -> Rect {
    let pad = area.height.saturating_sub(content_height) / 2;
    Rect {
        y: area.y + pad,
        height: content_height.min(area.height),
        ..area
    }
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let cut: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{cut}…")
    }
}

fn truncate_left(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        s.to_string()
    } else {
        let tail: String = s.chars().skip(count - max.saturating_sub(1)).collect();
        format!("…{tail}")
    }
}
