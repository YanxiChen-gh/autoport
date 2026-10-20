//! autoport - dynamic SSH port forwarder with a tiny TUI.
//!
//! Point it at a host you can already `ssh` into; it watches which TCP ports
//! are listening over there and forwards them to the same port on localhost,
//! either automatically or one at a time. Think VS Code's port forwarding,
//! minus VS Code.

mod app;
mod ssh;
mod throughput;
mod ui;
mod worker;

use std::io::{self, Write};
use std::process::Stdio;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::Terminal;

use app::App;
use ssh::Ssh;
use worker::Command;

struct Args {
    host: String,
    interval: u64,
    min_port: u16,
    max_port: u16,
}

fn parse_args() -> Args {
    let mut host: Option<String> = None;
    let mut interval = 4u64;
    let mut min_port = 1024u16;
    let mut max_port = 65535u16;

    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => usage(0),
            "--interval" => interval = next_num(&mut it),
            "--min-port" => min_port = next_num(&mut it),
            "--max-port" => max_port = next_num(&mut it),
            s if !s.starts_with('-') && host.is_none() => host = Some(s.to_string()),
            _ => usage(2),
        }
    }

    if min_port > max_port {
        usage(2);
    }

    Args {
        host: host.unwrap_or_else(|| usage(2)),
        interval: interval.max(1),
        min_port: min_port.max(1),
        max_port,
    }
}

fn next_num<T: std::str::FromStr>(it: &mut impl Iterator<Item = String>) -> T {
    it.next()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| usage(2))
}

fn usage(code: i32) -> ! {
    let text = "autoport - dynamic SSH port forwarder

USAGE:
    autoport [USER@]HOST [options]

OPTIONS:
    --interval SECS   how often to rescan remote ports (default 4)
    --min-port N      lowest remote port to show (default 1024)
    --max-port N      highest remote port to show (default 65535)
    -h, --help        show this help

KEYS:
    \u{2191}/\u{2193}, j/k   navigate          space   forward / unforward (manual)
    a             toggle auto-forward   r       rescan now
    Ctrl+A        stop all forwards
    o             open in browser       q/Esc   quit

HOST is anything your ssh understands (uses ~/.ssh/config, keys and agent).";
    if code == 0 {
        println!("{text}");
    } else {
        eprintln!("{text}");
    }
    std::process::exit(code);
}

fn main() {
    let args = parse_args();
    let ssh = Ssh::new(args.host.clone(), args.min_port, args.max_port);

    // Connect BEFORE touching the terminal so any password / passphrase prompt
    // shows up normally rather than fighting the TUI for stdin.
    eprint!("autoport: connecting to {} \u{2026} ", args.host);
    let _ = io::stderr().flush();
    if let Err(e) = ssh.start_master(true) {
        eprintln!("\nautoport: could not connect: {e}");
        std::process::exit(1);
    }
    eprintln!("ok");

    if let Err(e) = run(ssh, args) {
        eprintln!("autoport: {e}");
        std::process::exit(1);
    }
}

fn run(ssh: Ssh, args: Args) -> io::Result<()> {
    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let (ev_tx, ev_rx) = mpsc::channel();

    let interval = Duration::from_secs(args.interval);
    let worker = thread::spawn(move || worker::run(ssh, interval, cmd_rx, ev_tx));

    // Terminal setup, with a panic hook that restores it no matter what.
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        original_hook(info);
    }));

    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut app = App::new(args.host);

    let loop_result = event_loop(&mut terminal, &mut app, &cmd_tx, &ev_rx);

    // Always restore the terminal, then shut the worker down cleanly.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    let _ = cmd_tx.send(Command::Shutdown);
    let _ = worker.join();

    loop_result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    cmd_tx: &mpsc::Sender<Command>,
    ev_rx: &mpsc::Receiver<worker::Event>,
) -> io::Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;

        // Apply everything the worker has sent since the last frame.
        while let Ok(ev) = ev_rx.try_recv() {
            app.apply(ev);
        }

        if event::poll(Duration::from_millis(120))? {
            if let CEvent::Key(key) = event::read()? {
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                app.flash = None; // any keypress clears a transient message

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => app.should_quit = true,
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.should_quit = true
                    }
                    KeyCode::Char('a') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        clear_forwards(app, cmd_tx)
                    }
                    KeyCode::Up | KeyCode::Char('k') => app.move_up(),
                    KeyCode::Down | KeyCode::Char('j') => app.move_down(),
                    KeyCode::Char('a') => {
                        app.auto = !app.auto;
                        let _ = cmd_tx.send(Command::ToggleAuto(app.auto));
                    }
                    KeyCode::Char('r') => {
                        let _ = cmd_tx.send(Command::Refresh);
                        app.flash("rescanning\u{2026}");
                    }
                    KeyCode::Char(' ') => toggle_selected(app, cmd_tx),
                    KeyCode::Char('o') => open_selected(app),
                    _ => {}
                }
            }
        }

        if app.should_quit {
            return Ok(());
        }
    }
}

fn clear_forwards(app: &mut App, cmd_tx: &mpsc::Sender<Command>) {
    app.auto = false;
    let _ = cmd_tx.send(Command::ClearForwards);
    app.flash("auto-forward off; stopping all forwards");
}

fn toggle_selected(app: &mut App, cmd_tx: &mpsc::Sender<Command>) {
    if app.auto {
        app.flash("auto-forward is on - press 'a' to prune, Ctrl+A to stop all");
        return;
    }
    if let Some(port) = app.selected_port() {
        if app.is_forwarding(port) {
            let _ = cmd_tx.send(Command::CancelPort(port));
        } else {
            let _ = cmd_tx.send(Command::ForwardPort(port));
        }
    }
}

fn open_selected(app: &mut App) {
    match app.selected_local() {
        Some(local) => {
            let url = format!("http://localhost:{local}");
            let opener = if cfg!(target_os = "macos") {
                "open"
            } else {
                "xdg-open"
            };
            let _ = std::process::Command::new(opener)
                .arg(&url)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
            app.flash(&format!("opening {url}"));
        }
        None => app.flash("forward this port first (space), then open it"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_forwards_turns_auto_off_and_sends_command() {
        let (tx, rx) = mpsc::channel();
        let mut app = App::new("devbox".to_string());
        app.auto = true;

        clear_forwards(&mut app, &tx);

        assert!(!app.auto);
        assert_eq!(rx.try_recv(), Ok(Command::ClearForwards));
        assert_eq!(
            app.flash.as_deref(),
            Some("auto-forward off; stopping all forwards")
        );
    }
}
