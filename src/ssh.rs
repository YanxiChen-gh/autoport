//! Thin wrapper around the system `ssh` binary.
//!
//! We drive a single multiplexed connection (a ControlMaster) and then add /
//! remove forwards on it at runtime with `ssh -O forward|cancel`. This means we
//! reuse the user's existing `~/.ssh/config`, keys, agent and known_hosts for
//! free - exactly like VS Code's remote tunnelling does under the hood.

use std::collections::{hash_map::DefaultHasher, HashSet};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// A listening socket discovered on the remote host.
#[derive(Debug, Clone)]
pub struct RemotePort {
    pub port: u16,
    pub addr: String,
    pub process: String,
}

/// Everything needed to talk to one remote host over a shared control socket.
#[derive(Clone)]
pub struct Ssh {
    pub host: String,
    socket: PathBuf,
    min_port: u16,
    max_port: u16,
    compression: bool,
}

/// Remote command used to enumerate listening TCP sockets. Falls back from the
/// richest form (with process names) down to plain `ss`, then `netstat`.
const PORT_CMD: &str =
    "ss -H -t -l -n -p 2>/dev/null || ss -H -t -l -n 2>/dev/null || netstat -tln 2>/dev/null";

impl Ssh {
    pub fn new(host: String, min_port: u16, max_port: u16, compression: bool) -> Self {
        let mut socket = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        // OpenSSH adds a 17-byte temporary suffix when creating the control socket.
        let mut hash = DefaultHasher::new();
        host.hash(&mut hash);
        socket.push(format!("ap-{:016x}", hash.finish()));
        Ssh {
            host,
            socket,
            min_port,
            max_port,
            compression,
        }
    }

    fn control(&self) -> Command {
        let mut c = Command::new("ssh");
        c.arg("-S").arg(&self.socket);
        c
    }

    /// Open the master connection. `interactive` lets the very first connect
    /// prompt for a password / passphrase on the normal terminal; reconnects
    /// run with BatchMode so they never hang the TUI waiting on stdin.
    pub fn start_master(&self, interactive: bool) -> Result<(), String> {
        let _ = self.exit_master(); // clear any stale socket from a previous run

        let mut c = Command::new("ssh");
        c.arg("-f") // background once authenticated
            .arg("-N") // no remote command
            .arg("-M") // become the master
            .arg("-S")
            .arg(&self.socket)
            .arg("-o")
            .arg("ControlMaster=yes")
            .arg("-o")
            .arg("ConnectTimeout=10")
            .arg("-o")
            .arg("ServerAliveInterval=15")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            .arg("-o")
            .arg("ExitOnForwardFailure=yes");
        if self.compression {
            c.arg("-C");
        }
        if !interactive {
            c.arg("-o").arg("BatchMode=yes");
        }
        c.arg(&self.host);

        let status = c.status().map_err(|e| format!("could not run ssh: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!(
                "ssh exited with status {}",
                status.code().unwrap_or(-1)
            ))
        }
    }

    /// Is the master still alive?
    pub fn check(&self) -> bool {
        self.control()
            .arg("-O")
            .arg("check")
            .arg(&self.host)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// Tear the master down (also closes every forward it carries).
    pub fn exit_master(&self) -> Result<(), String> {
        let ok = self
            .control()
            .arg("-O")
            .arg("exit")
            .arg(&self.host)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            Ok(())
        } else {
            Err("no active master".into())
        }
    }

    pub fn forward(&self, local: u16, remote: u16, target: &str) -> Result<(), String> {
        self.forward_op("forward", local, remote, target)
    }

    pub fn cancel(&self, local: u16, remote: u16, target: &str) -> Result<(), String> {
        self.forward_op("cancel", local, remote, target)
    }

    fn forward_op(&self, op: &str, local: u16, remote: u16, target: &str) -> Result<(), String> {
        // Local side always binds 127.0.0.1; the remote side connects to
        // `target` (see `remote_target`) so IPv6-only services still work.
        let spec = format!("127.0.0.1:{local}:{target}:{remote}");
        let out = self
            .control()
            .arg("-O")
            .arg(op)
            .arg("-L")
            .arg(&spec)
            .arg(&self.host)
            .output()
            .map_err(|e| e.to_string())?;
        if out.status.success() {
            Ok(())
        } else {
            let msg = String::from_utf8_lossy(&out.stderr);
            Err(msg
                .trim()
                .lines()
                .last()
                .unwrap_or("forward failed")
                .to_string())
        }
    }

    /// Ask the remote which TCP ports are listening.
    pub fn poll_ports(&self) -> Result<Vec<RemotePort>, String> {
        let out = self
            .control()
            .arg(&self.host)
            .arg(PORT_CMD)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr);
            return Err(msg
                .trim()
                .lines()
                .last()
                .unwrap_or("poll failed")
                .to_string());
        }
        Ok(self.parse(&String::from_utf8_lossy(&out.stdout)))
    }

    fn parse(&self, text: &str) -> Vec<RemotePort> {
        let mut seen: HashSet<u16> = HashSet::new();
        let mut out = Vec::new();
        for line in text.lines() {
            // Both `ss` and `netstat` keep the local address in column index 3.
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 4 {
                continue;
            }
            let Some((addr, port)) = split_addr_port(cols[3]) else {
                continue;
            };
            if port < self.min_port || port > self.max_port {
                continue;
            }
            if !seen.insert(port) {
                continue; // dedup IPv4/IPv6 duplicates of the same port
            }
            out.push(RemotePort {
                port,
                addr,
                process: extract_process(line),
            });
        }
        out.sort_by_key(|p| p.port);
        out
    }
}

/// Pick the address the *remote* sshd should connect to for a service `ss`
/// reported on `addr`. Wildcards collapse to the matching loopback; specific
/// addresses are used verbatim (IPv6 bracketed).
///
/// This is the bit that makes IPv6-only localhost binds work: Node / bun /
/// Vite default `localhost` to `[::1]`, so a forward whose remote end is
/// hardwired to `127.0.0.1` connects to nothing. Matching the family fixes it.
pub fn remote_target(addr: &str) -> String {
    match addr {
        "0.0.0.0" | "*" | "" => "127.0.0.1".to_string(),
        "::" => "[::1]".to_string(),
        a if a.contains(':') => format!("[{a}]"), // specific IPv6, incl. ::1
        a => a.to_string(),                       // specific IPv4, incl. 127.0.0.1
    }
}

/// Split a `host:port` token, coping with IPv6 (`[::1]:5432`) and wildcards
/// (`*:111`, `:::80`).
pub(crate) fn split_addr_port(s: &str) -> Option<(String, u16)> {
    let idx = s.rfind(':')?;
    let (addr, port) = s.split_at(idx);
    let port: u16 = port[1..].parse().ok()?;
    let addr = addr.trim_matches(|c| c == '[' || c == ']');
    let addr = if addr.is_empty() { "*" } else { addr };
    Some((addr.to_string(), port))
}

/// Pull the first process name out of an `ss -p` users field, e.g.
/// `users:(("nginx",pid=1234,fd=6))` -> `nginx`.
fn extract_process(line: &str) -> String {
    const TAG: &str = "users:((\"";
    let Some(start) = line.find(TAG) else {
        return String::new();
    };
    let rest = &line[start + TAG.len()..];
    match rest.find('"') {
        Some(end) => rest[..end].to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ssh() -> Ssh {
        Ssh::new("test".into(), 1024, 65535, false)
    }

    #[test]
    fn control_sockets_preserve_host_identity() {
        let first = Ssh::new("dev-one".into(), 1024, 65535, false);
        let same = Ssh::new("dev-one".into(), 1024, 65535, false);
        let different = Ssh::new("dev.one".into(), 1024, 65535, false);
        assert_eq!(first.socket, same.socket);
        assert_ne!(first.socket, different.socket);
    }

    #[test]
    fn parses_real_ss_output() {
        // Captured verbatim from a live VM (ss -H -t -l -n -p).
        let out = "\
LISTEN 0      4096         0.0.0.0:3300  0.0.0.0:*
LISTEN 0      4096       127.0.0.1:19000 0.0.0.0:*
LISTEN 0      4096   127.0.0.53%lo:53    0.0.0.0:*
LISTEN 0      4096         0.0.0.0:22    0.0.0.0:*
LISTEN 0      512        127.0.0.1:35193 0.0.0.0:* users:((\"opencode\",pid=2248800,fd=22))
LISTEN 0      2048         0.0.0.0:8008  0.0.0.0:* users:((\"uvicorn\",pid=2829320,fd=11))";
        let ports = ssh().parse(out);

        // 53 and 22 are below the 1024 floor and must be dropped.
        let nums: Vec<u16> = ports.iter().map(|p| p.port).collect();
        assert_eq!(nums, vec![3300, 8008, 19000, 35193]);

        let uvicorn = ports.iter().find(|p| p.port == 8008).unwrap();
        assert_eq!(uvicorn.process, "uvicorn");
        assert_eq!(uvicorn.addr, "0.0.0.0");

        let opencode = ports.iter().find(|p| p.port == 35193).unwrap();
        assert_eq!(opencode.process, "opencode");
        assert_eq!(opencode.addr, "127.0.0.1");
    }

    #[test]
    fn dedups_ipv4_and_ipv6_same_port() {
        let out = "\
LISTEN 0 4096 0.0.0.0:8080 0.0.0.0:* users:((\"app\",pid=1,fd=1))
LISTEN 0 4096    [::]:8080    [::]:*";
        let ports = ssh().parse(out);
        assert_eq!(ports.len(), 1);
        assert_eq!(ports[0].port, 8080);
        assert_eq!(ports[0].process, "app"); // keeps the first (IPv4) row
    }

    #[test]
    fn parses_netstat_fallback() {
        let out = "\
tcp        0      0 0.0.0.0:8080            0.0.0.0:*               LISTEN
tcp6       0      0 :::9090                 :::*                    LISTEN
tcp        0      0 127.0.0.1:631           0.0.0.0:*               LISTEN";
        let ports = ssh().parse(out);
        let nums: Vec<u16> = ports.iter().map(|p| p.port).collect();
        assert_eq!(nums, vec![8080, 9090]); // 631 < 1024 dropped
    }

    #[test]
    fn skips_garbage_and_headers() {
        let out = "\
Active Internet connections (only servers)
Proto Recv-Q Send-Q Local Address Foreign Address State
short line";
        assert!(ssh().parse(out).is_empty());
    }

    #[test]
    fn port_range_is_respected() {
        let s = Ssh::new("t".into(), 8000, 8999, false);
        let out = "\
LISTEN 0 1 0.0.0.0:80 0.0.0.0:*
LISTEN 0 1 0.0.0.0:8080 0.0.0.0:*
LISTEN 0 1 0.0.0.0:9000 0.0.0.0:*";
        let nums: Vec<u16> = s.parse(out).iter().map(|p| p.port).collect();
        assert_eq!(nums, vec![8080]);
    }

    #[test]
    fn address_port_splitting() {
        assert_eq!(
            split_addr_port("127.0.0.1:3000"),
            Some(("127.0.0.1".into(), 3000))
        );
        assert_eq!(split_addr_port("[::1]:5432"), Some(("::1".into(), 5432)));
        assert_eq!(split_addr_port("[::]:443"), Some(("::".into(), 443)));
        assert_eq!(split_addr_port(":::80"), Some(("::".into(), 80)));
        assert_eq!(split_addr_port("*:111"), Some(("*".into(), 111)));
        assert_eq!(split_addr_port("0.0.0.0:0"), Some(("0.0.0.0".into(), 0)));
        assert_eq!(split_addr_port("no-port-here"), None);
        assert_eq!(split_addr_port("1.2.3.4:99999"), None); // overflows u16
    }

    #[test]
    fn remote_target_matches_family() {
        // IPv4 wildcard / loopback / specific -> plain IPv4.
        assert_eq!(remote_target("0.0.0.0"), "127.0.0.1");
        assert_eq!(remote_target("127.0.0.1"), "127.0.0.1");
        assert_eq!(remote_target("192.168.1.5"), "192.168.1.5");
        assert_eq!(remote_target("*"), "127.0.0.1");
        // IPv6 -> bracketed loopback (the Vite/bun case) or verbatim.
        assert_eq!(remote_target("::1"), "[::1]");
        assert_eq!(remote_target("::"), "[::1]");
        assert_eq!(remote_target("fe80::1"), "[fe80::1]");
    }

    #[test]
    fn process_extraction() {
        assert_eq!(
            extract_process("... users:((\"nginx\",pid=1234,fd=6))"),
            "nginx"
        );
        assert_eq!(
            extract_process("... users:((\"python3\",pid=1,fd=4),(\"python3\",pid=2,fd=5))"),
            "python3"
        );
        assert_eq!(extract_process("LISTEN 0 1 0.0.0.0:80 0.0.0.0:*"), "");
    }
}
