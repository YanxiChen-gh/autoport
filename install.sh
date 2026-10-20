#!/bin/sh
set -eu

repo="YanxiChen-gh/autoport"
bindir="${BINDIR:-$HOME/.local/bin}"

case "$(uname -s)" in
    Linux) platform="linux" ;;
    Darwin) platform="macos" ;;
    *)
        echo "unsupported operating system: $(uname -s)" >&2
        exit 1
        ;;
esac

case "$(uname -m)" in
    x86_64 | amd64)
        arch="x86_64"
        ;;
    aarch64 | arm64)
        arch="aarch64"
        ;;
    *)
        echo "unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac
package="autoport-$platform-$arch"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

archive="$package.tar.gz"
url="https://github.com/$repo/releases/latest/download/$archive"

mkdir -p "$bindir"
curl -fsSL "$url" -o "$tmp/$archive"
tar -xzf "$tmp/$archive" -C "$tmp"
install -m 0755 "$tmp/$package/autoport" "$bindir/autoport"

echo "installed autoport -> $bindir/autoport"
case ":$PATH:" in
    *":$bindir:"*) ;;
    *) echo "note: $bindir is not on your PATH" ;;
esac
