//! All rendering. One screen: a header strip, the port table, a help bar.
//! Kept deliberately flat - build a few `Line`s, hand them to widgets, done.

use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::App;
use crate::worker::{ForwardState, Status};

const ACCENT: Color = Color::Cyan;
const ON: Color = Color::Green;
const PENDING: Color = Color::Yellow;
const FAIL: Color = Color::Red;
const DIM: Color = Color::DarkGray;
const FLOW: Color = Color::Rgb(80, 220, 160); // live-throughput accent

// Column widths, shared by the header row and every list row so they line up.
// (The list adds a 2-char highlight symbol in front, hence the leading pad.)
const W_PORT: usize = 7;
const W_PROC: usize = 15;
const W_ADDR: usize = 15;
const W_LOCAL: usize = 8;
const W_RATE: usize = 11;

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Length(1), // column titles
            Constraint::Min(1),    // port list
            Constraint::Length(3), // help / status bar
        ])
        .split(f.area());

    header(f, chunks[0], app);
    column_titles(f, chunks[1]);
    port_list(f, chunks[2], app);
    footer(f, chunks[3], app);
}

fn header(f: &mut Frame, area: Rect, app: &App) {
    let (dot, label, color) = match &app.status {
        Status::Connected => ("\u{25CF}", "connected".to_string(), ON),
        Status::Reconnecting => ("\u{25CF}", "reconnecting\u{2026}".to_string(), PENDING),
        Status::Disconnected(e) => ("\u{25CF}", format!("disconnected ({e})"), FAIL),
    };

    let auto = if app.auto {
        Span::styled(
            " AUTO ON ",
            Style::default()
                .fg(Color::Black)
                .bg(ON)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled(" auto off ", Style::default().fg(DIM))
    };

    let sep = || Span::styled("  \u{2502}  ", Style::default().fg(DIM));
    let total_rate = app.total_rate();
    let total_rate = if total_rate == 0 {
        "0 B/s".to_string()
    } else {
        human_rate(total_rate)
    };

    let line = Line::from(vec![
        Span::styled(
            " autoport ",
            Style::default()
                .fg(Color::Black)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            &app.host,
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
        sep(),
        Span::styled(dot, Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(label, Style::default().fg(color)),
        sep(),
        auto,
        sep(),
        Span::styled(
            format!("{} forwarded", app.forwarded_count()),
            Style::default().fg(ACCENT),
        ),
        sep(),
        Span::styled("\u{21C5} ", Style::default().fg(FLOW)),
        Span::styled(
            total_rate,
            Style::default().fg(FLOW).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" total", Style::default().fg(DIM)),
    ]);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM));
    f.render_widget(Paragraph::new(line).block(block), area);
}

fn column_titles(f: &mut Frame, area: Rect) {
    let header = format!(
        "  {:<pw$}{:<cw$}{:<aw$}{:<lw$}{:>rw$}  {}",
        "PORT",
        "PROCESS",
        "ADDRESS",
        "LOCAL",
        "RATE",
        "STATE",
        pw = W_PORT,
        cw = W_PROC,
        aw = W_ADDR,
        lw = W_LOCAL,
        rw = W_RATE,
    );
    let p = Paragraph::new(Line::from(Span::styled(
        header,
        Style::default().fg(DIM).add_modifier(Modifier::BOLD),
    )));
    f.render_widget(p, area);
}

fn port_list(f: &mut Frame, area: Rect, app: &App) {
    let rows = app.rows();

    if rows.is_empty() {
        let msg = Paragraph::new(Line::from(Span::styled(
            "  no listening ports in range yet \u{2014} waiting\u{2026}",
            Style::default().fg(DIM),
        )));
        f.render_widget(msg, area);
        return;
    }

    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let (badge, badge_color, local) = match &r.state {
                ForwardState::Active(l) => ("\u{25CF} on", ON, l.to_string()),
                ForwardState::Pending => ("\u{2026} \u{2026}", PENDING, "\u{2026}".into()),
                ForwardState::Failed(_) => ("\u{2715} err", FAIL, "-".into()),
                ForwardState::Idle => ("\u{00B7} off", DIM, "-".into()),
            };
            let active = matches!(r.state, ForwardState::Active(_));
            let base = if active {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Gray)
            };

            // Rate cell: bright when bytes are actually flowing, dim "-" idle.
            let rate_style = if r.rate > 0 {
                Style::default().fg(FLOW).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(DIM)
            };

            let line = Line::from(vec![
                Span::styled(pad(&r.port.to_string(), W_PORT), base.fg(ACCENT)),
                Span::styled(pad(&trunc(&r.process, W_PROC - 1), W_PROC), base),
                Span::styled(pad(&trunc(&r.addr, W_ADDR - 1), W_ADDR), base.fg(DIM)),
                Span::styled(pad(&local, W_LOCAL), base),
                Span::styled(rpad(&human_rate(r.rate), W_RATE), rate_style),
                Span::raw("  "),
                Span::styled(badge.to_string(), Style::default().fg(badge_color)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let list = List::new(items)
        .highlight_symbol("\u{25B8} ")
        .highlight_style(
            Style::default()
                .bg(Color::Rgb(30, 40, 50))
                .add_modifier(Modifier::BOLD),
        );

    let mut state = ListState::default();
    state.select(Some(app.selected));
    f.render_stateful_widget(list, area, &mut state);
}

fn footer(f: &mut Frame, area: Rect, app: &App) {
    let keys: Vec<(&str, &str)> = vec![
        ("\u{2191}\u{2193}", "move"),
        ("space", "forward"),
        ("a", "auto"),
        ("ctrl+a", "stop all"),
        ("r", "refresh"),
        ("o", "open"),
        ("q", "quit"),
    ];

    let mut spans: Vec<Span> = Vec::new();
    for (i, (k, v)) in keys.iter().enumerate() {
        if i > 0 {
            spans.push(Span::styled("   ", Style::default().fg(DIM)));
        }
        spans.push(Span::styled(
            (*k).to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            (*v).to_string(),
            Style::default().fg(Color::Gray),
        ));
    }

    if let Some(msg) = &app.flash {
        spans.push(Span::styled("    \u{2502}  ", Style::default().fg(DIM)));
        spans.push(Span::styled(
            msg.clone(),
            Style::default().fg(PENDING).add_modifier(Modifier::ITALIC),
        ));
    }

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM));
    f.render_widget(
        Paragraph::new(Line::from(spans))
            .block(block)
            .alignment(Alignment::Left),
        area,
    );
}

/// Left-pad to width with spaces (width in chars).
fn pad(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - len))
    }
}

/// Right-align within width (pad on the left) - used for the rate column.
fn rpad(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(width - len))
    }
}

/// Human-readable byte rate, e.g. `1.4 MB/s`. Zero renders as a dim dash.
fn human_rate(bps: u64) -> String {
    const K: f64 = 1024.0;
    let b = bps as f64;
    if bps == 0 {
        "\u{2014}".to_string()
    } else if b < K {
        format!("{bps} B/s")
    } else if b < K * K {
        format!("{:.0} KB/s", b / K)
    } else if b < K * K * K {
        format!("{:.1} MB/s", b / (K * K))
    } else {
        format!("{:.2} GB/s", b / (K * K * K))
    }
}

/// Truncate with an ellipsis so long names never break the columns.
fn trunc(s: &str, max: usize) -> String {
    let len = s.chars().count();
    if len <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let keep: String = s.chars().take(max - 1).collect();
        format!("{keep}\u{2026}")
    }
}
