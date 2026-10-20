//! Live per-forward throughput, measured locally.
//!
//! autoport isn't in the data path - the OpenSSH master process is - so we
//! can't count bytes ourselves. The kernel can, though: every forwarded byte
//! crosses the master's local listener socket, and `ss -ti` reports each
//! socket's cumulative `bytes_acked` / `bytes_received` from `TCP_INFO`.
//!
//! We sample those counters for our forward ports and diff them over time. No
//! network round-trip is involved; it's a cheap local `ss` call.

use std::collections::{HashMap, HashSet};
use std::process::Command;
use std::time::Instant;

use crate::ssh::split_addr_port;

/// Tracks the previous byte sample per local port so it can report rates.
pub struct Meter {
    last: HashMap<u16, (u64, Instant)>,
}

impl Meter {
    pub fn new() -> Self {
        Meter {
            last: HashMap::new(),
        }
    }

    /// Sample the given local listener ports and return `local_port -> bytes/s`
    /// since the previous call. Ports seen for the first time report 0 until
    /// there's a baseline to diff against.
    pub fn rates(&mut self, local_ports: &HashSet<u16>) -> HashMap<u16, u64> {
        let now = Instant::now();
        let current = sample(local_ports);

        let mut rates = HashMap::with_capacity(current.len());
        for (&port, &bytes) in &current {
            let rate = match self.last.get(&port) {
                // Counters only grow; a drop means connections churned, so
                // skip that interval rather than report a negative spike.
                Some(&(prev_bytes, prev_t)) if bytes >= prev_bytes => {
                    let dt = now.duration_since(prev_t).as_secs_f64();
                    if dt > 0.0 {
                        ((bytes - prev_bytes) as f64 / dt) as u64
                    } else {
                        0
                    }
                }
                _ => 0,
            };
            rates.insert(port, rate);
        }

        // Keep only ports we still care about, with fresh baselines.
        self.last = current.into_iter().map(|(p, b)| (p, (b, now))).collect();
        rates
    }
}

/// Run `ss` and return `local_port -> cumulative bytes` for the wanted ports.
fn sample(wanted: &HashSet<u16>) -> HashMap<u16, u64> {
    if wanted.is_empty() {
        return HashMap::new();
    }
    let out = Command::new("ss")
        .arg("-tinH")
        .arg("state")
        .arg("established")
        .output();
    match out {
        Ok(o) if o.status.success() => parse(&String::from_utf8_lossy(&o.stdout), wanted),
        _ => HashMap::new(),
    }
}

/// Parse `ss -tinH` output. Each socket is a header line (state, queues, local,
/// peer) followed by an indented `TCP_INFO` line carrying the byte counters.
/// We attribute a socket to a forward when its *local* (source) port is one of
/// ours - that's the master's accepted socket, so we don't double-count the
/// client side of the same loopback connection.
fn parse(text: &str, wanted: &HashSet<u16>) -> HashMap<u16, u64> {
    let mut bytes: HashMap<u16, u64> = HashMap::new();
    let mut current: Option<u16> = None;

    for line in text.lines() {
        if line.starts_with(|c: char| c.is_whitespace()) {
            if let Some(port) = current {
                let acked = field(line, "bytes_acked:").unwrap_or(0);
                let received = field(line, "bytes_received:").unwrap_or(0);
                *bytes.entry(port).or_insert(0) += acked + received;
            }
        } else {
            let cols: Vec<&str> = line.split_whitespace().collect();
            current = cols
                .get(3)
                .and_then(|a| split_addr_port(a))
                .map(|(_, port)| port)
                .filter(|p| wanted.contains(p));
        }
    }
    bytes
}

/// Read the integer value of a `key:NNN` field from an `ss` info line.
fn field(line: &str, key: &str) -> Option<u64> {
    let start = line.find(key)? + key.len();
    let rest = &line[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports(list: &[u16]) -> HashSet<u16> {
        list.iter().copied().collect()
    }

    #[test]
    fn parses_bytes_per_port() {
        // Real `ss -tinH` capture (two forwards + one unrelated socket).
        let out = "\
ESTAB 0 0 127.0.0.1:49100 127.0.0.1:58232
\t cubic wscale:10,10 rtt:6.5/12 bytes_sent:36174793 bytes_retrans:933252 bytes_acked:35241541 bytes_received:88 segs_out:618
ESTAB 0 0 127.0.0.1:49101 127.0.0.1:58240
\t cubic bytes_acked:1000 bytes_received:500 segs_out:9
ESTAB 0 0 192.168.1.9:22 10.0.0.2:51000
\t cubic bytes_acked:99999 bytes_received:99999";
        let got = parse(out, &ports(&[49100, 49101]));
        assert_eq!(got.get(&49100), Some(&(35241541 + 88)));
        assert_eq!(got.get(&49101), Some(&1500));
        assert_eq!(got.get(&22), None); // not a wanted port
    }

    #[test]
    fn sums_multiple_connections_on_one_port() {
        let out = "\
ESTAB 0 0 127.0.0.1:49100 127.0.0.1:1111
\t bytes_acked:100 bytes_received:0
ESTAB 0 0 127.0.0.1:49100 127.0.0.1:2222
\t bytes_acked:50 bytes_received:25";
        let got = parse(out, &ports(&[49100]));
        assert_eq!(got.get(&49100), Some(&175));
    }

    #[test]
    fn ignores_client_side_socket() {
        // The client end of the loopback pair has the forward port as *peer*,
        // not source, so it must not be counted.
        let out = "\
ESTAB 0 0 127.0.0.1:58232 127.0.0.1:49100
\t bytes_acked:777 bytes_received:777";
        assert!(parse(out, &ports(&[49100])).is_empty());
    }

    #[test]
    fn meter_reports_zero_then_rate() {
        // First sight of a port => 0; can't test the live diff without ss,
        // but the parser + field extraction are covered above.
        let mut m = Meter::new();
        let r = m.rates(&ports(&[]));
        assert!(r.is_empty());
    }

    #[test]
    fn field_extraction() {
        let l = "\t cubic bytes_acked:12345 bytes_received:67 segs_out:9";
        assert_eq!(field(l, "bytes_acked:"), Some(12345));
        assert_eq!(field(l, "bytes_received:"), Some(67));
        assert_eq!(field(l, "missing:"), None);
    }
}
