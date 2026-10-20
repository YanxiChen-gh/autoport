#!/bin/sh
set -eu

repo="Havnevej/autoport"
bindir="${BINDIR:-$HOME/.local/bin}"

case "$(uname -m)" in
    x86_64 | amd64)
        package="autoport-linux-x86_64"
        ;;
    aarch64 | arm64)
        package="autoport-linux-aarch64"
        ;;
    *)
        echo "unsupported architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

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
