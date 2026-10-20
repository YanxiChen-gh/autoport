//! Background thread that owns the SSH session.
//!
//! The UI never blocks on the network: it sends [`Command`]s here and receives
//! [`Event`]s back over channels. This thread polls the remote for listening
//! ports, reconciles the set of active forwards against what's desired, and
//! transparently re-establishes everything if the connection drops.

use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::ssh::{RemotePort, Ssh};
use crate::throughput::Meter;

/// UI -> worker.
#[derive(Debug, PartialEq)]
pub enum Command {
    ToggleAuto(bool),
    ForwardPort(u16),
    CancelPort(u16),
    ClearForwards,
    Refresh,
    Shutdown,
}

/// worker -> UI.
pub enum Event {
    Status(Status),
    Ports(Vec<RemotePort>),
    Forward(u16, ForwardState),
    Auto(bool),
    /// Live throughput per remote port, in bytes/second.
    Rates(HashMap<u16, u64>),
    Log(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    Connected,
    Reconnecting,
    Disconnected(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum ForwardState {
    Idle,
    Pending,
    Active(u16),
    Failed(String),
}

/// A live forward: the local port it listens on and the remote target it dials
/// (e.g. `127.0.0.1` or `[::1]`). We keep the target so we can cancel with the
/// exact same spec we forwarded with.
struct Fwd {
    local: u16,
    target: String,
}

pub fn run(ssh: Ssh, interval: Duration, rx: Receiver<Command>, tx: Sender<Event>) {
    let mut auto = false;
    let mut desired: HashSet<u16> = HashSet::new();
    let mut active: HashMap<u16, Fwd> = HashMap::new(); // remote -> live forward
    let mut failed: HashSet<u16> = HashSet::new();
    let mut last_ports: Vec<RemotePort> = Vec::new();
    let mut addrs: HashMap<u16, String> = HashMap::new(); // remote port -> bind addr
    let mut next_poll = Instant::now();
    let mut meter = Meter::new();
    let mut next_sample = Instant::now() + Duration::from_secs(1);

    let _ = tx.send(Event::Status(Status::Connected));

    loop {
        // Wait briefly for a command so the UI feels instant, then fall through
        // to the periodic poll. This is the whole event loop of the worker.
        match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(Command::ToggleAuto(v)) => {
                auto = v;
                let _ = tx.send(Event::Auto(auto));
                if auto {
                    failed.clear();
                    for p in &last_ports {
                        desired.insert(p.port);
                    }
                }
                reconcile(&ssh, &mut desired, &mut active, &mut failed, &addrs, &tx);
            }
            Ok(Command::ForwardPort(p)) => {
                failed.remove(&p);
                desired.insert(p);
                reconcile(&ssh, &mut desired, &mut active, &mut failed, &addrs, &tx);
            }
            Ok(Command::CancelPort(p)) => {
                desired.remove(&p);
                reconcile(&ssh, &mut desired, &mut active, &mut failed, &addrs, &tx);
            }
            Ok(Command::ClearForwards) => {
                auto = false;
                desired.clear();
                failed.clear();
                let _ = tx.send(Event::Auto(false));
                reconcile(&ssh, &mut desired, &mut active, &mut failed, &addrs, &tx);
                let _ = tx.send(Event::Rates(HashMap::new()));
            }
            Ok(Command::Refresh) => next_poll = Instant::now(),
            Err(RecvTimeoutError::Timeout) => {}
        }

        // Live throughput sampling, every second, independent of the rescan.
        if Instant::now() >= next_sample {
            next_sample = Instant::now() + Duration::from_secs(1);
            let locals: HashSet<u16> = active.values().map(|f| f.local).collect();
            let by_local = meter.rates(&locals);
            let by_remote: HashMap<u16, u64> = active
                .iter()
                .map(|(remote, fwd)| (*remote, by_local.get(&fwd.local).copied().unwrap_or(0)))
                .collect();
            let _ = tx.send(Event::Rates(by_remote));
        }

        if Instant::now() < next_poll {
            continue;
        }
        next_poll = Instant::now() + interval;

        // Make sure the master is alive; rebuild it (and all forwards) if not.
        if !ssh.check() {
            let _ = tx.send(Event::Status(Status::Reconnecting));
            active.clear();
            for p in &desired {
                let _ = tx.send(Event::Forward(*p, ForwardState::Pending));
            }
            match ssh.start_master(false) {
                Ok(()) => {
                    let _ = tx.send(Event::Status(Status::Connected));
                    reconcile(&ssh, &mut desired, &mut active, &mut failed, &addrs, &tx);
                }
                Err(e) => {
                    let _ = tx.send(Event::Status(Status::Disconnected(e)));
                    continue;
                }
            }
        }

        match ssh.poll_ports() {
            Ok(ports) => {
                let live: HashSet<u16> = ports.iter().map(|p| p.port).collect();

                // Drop forwards whose remote service has gone away.
                let gone: Vec<u16> = desired
                    .iter()
                    .copied()
                    .filter(|p| !live.contains(p))
                    .collect();
                for p in gone {
                    desired.remove(&p);
                    failed.remove(&p);
                }

                if auto {
                    for p in &ports {
                        if !failed.contains(&p.port) {
                            desired.insert(p.port);
                        }
                    }
                }

                addrs = ports.iter().map(|p| (p.port, p.addr.clone())).collect();
                last_ports = ports.clone();
                let _ = tx.send(Event::Ports(ports));
                reconcile(&ssh, &mut desired, &mut active, &mut failed, &addrs, &tx);
            }
            Err(e) => {
                let _ = tx.send(Event::Log(format!("poll failed: {e}")));
            }
        }
    }

    let _ = ssh.exit_master();
}

/// Make the live forward set match `desired`: cancel what's no longer wanted,
/// add what's missing. Failures are recorded so auto-mode won't thrash on them.
fn reconcile(
    ssh: &Ssh,
    desired: &mut HashSet<u16>,
    active: &mut HashMap<u16, Fwd>,
    failed: &mut HashSet<u16>,
    addrs: &HashMap<u16, String>,
    tx: &Sender<Event>,
) {
    let drop: Vec<u16> = active
        .keys()
        .copied()
        .filter(|p| !desired.contains(p))
        .collect();
    for remote in drop {
        if let Some(fwd) = active.remove(&remote) {
            let _ = ssh.cancel(fwd.local, remote, &fwd.target);
            let _ = tx.send(Event::Forward(remote, ForwardState::Idle));
        }
    }

    let add: Vec<u16> = desired
        .iter()
        .copied()
        .filter(|p| !active.contains_key(p))
        .collect();
    for remote in add {
        let local = free_local_port(remote, active);
        // Dial the remote loopback that matches how the service is bound, so
        // IPv6-only (`[::1]`) services forward correctly, not just IPv4 ones.
        let target = crate::ssh::remote_target(addrs.get(&remote).map_or("0.0.0.0", |s| s));
        let _ = tx.send(Event::Forward(remote, ForwardState::Pending));
        match ssh.forward(local, remote, &target) {
            Ok(()) => {
                active.insert(remote, Fwd { local, target });
                let _ = tx.send(Event::Forward(remote, ForwardState::Active(local)));
            }
            Err(e) => {
                desired.remove(&remote);
                failed.insert(remote);
                let _ = tx.send(Event::Forward(remote, ForwardState::Failed(e)));
            }
        }
    }
}

/// Prefer mapping the remote port to the same number locally; if that's taken,
/// scan for the next free one so the forward never collides.
fn free_local_port(preferred: u16, active: &HashMap<u16, Fwd>) -> u16 {
    let used: HashSet<u16> = active.values().map(|f| f.local).collect();
    let is_ok = |p: u16| p >= 1024 && !used.contains(&p) && bindable(p);

    if is_ok(preferred) {
        return preferred;
    }
    for p in preferred.saturating_add(1)..=u16::MAX {
        if is_ok(p) {
            return p;
        }
    }
    for p in 1024..preferred {
        if is_ok(p) {
            return p;
        }
    }
    preferred
}

fn bindable(port: u16) -> bool {
    TcpListener::bind(("127.0.0.1", port)).is_ok()
}
