# Changelog

## [0.1.1] - 2026-06-05

### Added

- `Ctrl+A` stops all active forwards and turns auto-forwarding off.

### Changed

- Turning auto-forwarding off with `a` now leaves current forwards in place so
  individual ports can be pruned manually with `space`.
- The TUI footer, help text, README, and showcase screenshot document the new
  stop-all shortcut.

## [0.1.0] - 2026-06-05

Initial public release.

### Added

- Terminal UI for discovering and managing remote SSH port forwards.
- Remote TCP listener discovery via `ss`, with `netstat` fallback.
- Manual forwarding and automatic forwarding modes.
- OpenSSH ControlMaster reuse with runtime `forward` and `cancel` operations.
- IPv4 and IPv6 loopback handling, including services bound to `::1`.
- Live per-forward and total throughput display when local TCP counters are available.
- Linux release binaries for x86_64 and ARM64/aarch64.
- One-line Linux installer and SHA-256 checksums for release assets.

### Notes

- Requires an existing working SSH connection to the target host.
- Local listeners bind to `127.0.0.1` only.
- Throughput display depends on local `ss`; forwarding still works without it.
