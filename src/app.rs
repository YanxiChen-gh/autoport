//! Pure UI state. Holds everything the renderer needs and nothing else; all
//! network work happens in the worker thread and arrives as [`Event`]s.

use std::collections::HashMap;

use crate::ssh::RemotePort;
use crate::worker::{Event, ForwardState, Status};

/// One row in the table: a remote port joined with its forward state.
pub struct Row {
    pub port: u16,
    pub addr: String,
    pub process: String,
    pub state: ForwardState,
    pub rate: u64,
}

pub struct App {
    pub host: String,
    pub status: Status,
    pub auto: bool,
    pub ports: Vec<RemotePort>,
    pub forwards: HashMap<u16, ForwardState>,
    pub rates: HashMap<u16, u64>,
    pub selected: usize,
    pub flash: Option<String>,
    pub should_quit: bool,
}

impl App {
    pub fn new(host: String) -> Self {
        App {
            host,
            status: Status::Connected,
            auto: false,
            ports: Vec::new(),
            forwards: HashMap::new(),
            rates: HashMap::new(),
            selected: 0,
            flash: None,
            should_quit: false,
        }
    }

    pub fn apply(&mut self, ev: Event) {
        match ev {
            Event::Status(s) => self.status = s,
            Event::Auto(a) => self.auto = a,
            Event::Ports(p) => {
                self.ports = p;
                self.clamp_selection();
            }
            Event::Forward(port, ForwardState::Idle) => {
                self.forwards.remove(&port);
            }
            Event::Forward(port, state) => {
                self.forwards.insert(port, state);
            }
            Event::Rates(r) => self.rates = r,
            Event::Log(m) => self.flash = Some(m),
        }
    }

    /// Join ports + forward state into render-ready rows.
    pub fn rows(&self) -> Vec<Row> {
        self.ports
            .iter()
            .map(|p| Row {
                port: p.port,
                addr: p.addr.clone(),
                process: p.process.clone(),
                state: self
                    .forwards
                    .get(&p.port)
                    .cloned()
                    .unwrap_or(ForwardState::Idle),
                rate: self.rates.get(&p.port).copied().unwrap_or(0),
            })
            .collect()
    }

    /// Total live throughput across all forwards, in bytes/second.
    pub fn total_rate(&self) -> u64 {
        self.rates.values().sum()
    }

    pub fn forwarded_count(&self) -> usize {
        self.forwards
            .values()
            .filter(|s| matches!(s, ForwardState::Active(_)))
            .count()
    }

    fn clamp_selection(&mut self) {
        if self.selected >= self.ports.len() {
            self.selected = self.ports.len().saturating_sub(1);
        }
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        if self.selected + 1 < self.ports.len() {
            self.selected += 1;
        }
    }

    pub fn selected_port(&self) -> Option<u16> {
        self.ports.get(self.selected).map(|p| p.port)
    }

    pub fn selected_local(&self) -> Option<u16> {
        match self.forwards.get(&self.selected_port()?) {
            Some(ForwardState::Active(l)) => Some(*l),
            _ => None,
        }
    }

    /// Is the selected port already forwarded or on its way there?
    pub fn is_forwarding(&self, port: u16) -> bool {
        matches!(
            self.forwards.get(&port),
            Some(ForwardState::Active(_) | ForwardState::Pending)
        )
    }

    pub fn flash(&mut self, msg: &str) {
        self.flash = Some(msg.to_string());
    }
}
