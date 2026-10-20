#!/usr/bin/env python3
"""Exercise a real SSH master and reconnect with long host and temp paths."""

import fcntl
import getpass
import os
from pathlib import Path
import pty
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import time


def run(*args):
    return subprocess.run(args, check=True, text=True, capture_output=True)


def main():
    binary = str(Path(sys.argv[1]).resolve())
    ssh = shutil.which("ssh")
    sshd = shutil.which("sshd") or "/usr/sbin/sshd"
    alias = "smoke-" + "a" * 40 + ".cde.example"
    with tempfile.TemporaryDirectory(prefix="ap-", dir="/tmp") as directory:
        root = Path(directory)
        runtime = root / ("runtime-" + "t" * 30)
        runtime.mkdir()
        for key in ("hostkey", "clientkey"):
            run("ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(root / key))
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            port = listener.getsockname()[1]
        host_key = (root / "hostkey.pub").read_text().split()
        (root / "known_hosts").write_text(
            f"[127.0.0.1]:{port} {host_key[0]} {host_key[1]}\n"
        )
        server_config = root / "sshd_config"
        server_config.write_text(f"""ListenAddress 127.0.0.1
Port {port}
HostKey {root}/hostkey
PidFile {root}/sshd.pid
AuthorizedKeysFile {root}/clientkey.pub
StrictModes no
UsePAM no
PasswordAuthentication no
KbdInteractiveAuthentication no
PubkeyAuthentication yes
AllowUsers {getpass.getuser()}
AllowTcpForwarding local
Compression yes
""")
        client_config = root / "ssh_config"
        client_config.write_text(f"""Host {alias}
    HostName 127.0.0.1
    Port {port}
    User {getpass.getuser()}
    IdentityFile {root}/clientkey
    IdentitiesOnly yes
    UserKnownHostsFile {root}/known_hosts
    StrictHostKeyChecking yes
    Compression no
""")
        log = root / "ssh.log"
        shim = root / "bin"
        shim.mkdir()
        (shim / "ssh").write_text(
            f'#!/bin/sh\nexec "{ssh}" -F "{client_config}" -vv -E "{log}" "$@"\n'
        )
        (shim / "ssh").chmod(0o755)
        with (root / "sshd.log").open("w") as server_log:
            daemon = subprocess.Popen(
                [sshd, "-D", "-e", "-f", str(server_config)], stderr=server_log
            )
            child = terminal = child_status = None
            try:
                deadline = time.monotonic() + 10
                while True:
                    if daemon.poll() is not None:
                        raise RuntimeError((root / "sshd.log").read_text())
                    try:
                        with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                            break
                    except OSError:
                        if time.monotonic() > deadline:
                            raise
                        time.sleep(0.05)
                env = dict(
                    os.environ,
                    PATH=f"{shim}:{os.environ['PATH']}",
                    TMPDIR=str(runtime),
                    TERM="xterm-256color",
                )
                env.pop("XDG_RUNTIME_DIR", None)
                child, terminal = pty.fork()
                if child == 0:
                    os.execve(binary, [binary, alias, "--compression", "--interval", "1"], env)
                os.set_blocking(terminal, False)
                fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 32, 140, 0, 0))
                output = bytearray()

                def child_exited():
                    nonlocal child_status
                    if child_status is None:
                        exited, status = os.waitpid(child, os.WNOHANG)
                        if exited:
                            child_status = status
                    return child_status is not None

                def wait_until(predicate):
                    deadline = time.monotonic() + 20
                    while time.monotonic() < deadline:
                        while True:
                            try:
                                data = os.read(terminal, 65536)
                                if not data:
                                    break
                                output.extend(data)
                            except (BlockingIOError, OSError):
                                break
                        result = predicate()
                        if result:
                            return result
                        if child_exited():
                            raise RuntimeError(output.decode(errors="replace") + log.read_text())
                        time.sleep(0.05)
                    raise TimeoutError(output.decode(errors="replace"))

                wait_until(lambda: b"\x1b[?1049h" in output)
                paths = list(runtime.iterdir())
                if len(paths) != 1:
                    raise RuntimeError(f"Expected one control socket, found {paths}")
                control_socket = paths[0]
                # macOS sun_path has 104 bytes including NUL; OpenSSH adds . + 16 bytes.
                assert len(os.fsencode(control_socket)) + 17 < 104
                check = run(ssh, "-F", str(client_config), "-S", str(control_socket), "-O", "check", alias)
                print(check.stderr.strip(), flush=True)
                run(ssh, "-F", str(client_config), "-S", str(control_socket), "-O", "exit", alias)

                def reconnected():
                    negotiations = re.findall(
                        r"kex: server->client cipher:.*compression: ([^\s]+)",
                        log.read_text(),
                    )
                    if len(negotiations) < 2 or negotiations[-1] != "zlib@openssh.com":
                        return False
                    return subprocess.run(
                        [ssh, "-F", str(client_config), "-S", str(control_socket), "-O", "check", alias],
                        capture_output=True,
                    ).returncode == 0

                wait_until(reconnected)
                check = run(ssh, "-F", str(client_config), "-S", str(control_socket), "-O", "check", alias)
                print(check.stderr.strip(), flush=True)
                os.write(terminal, b"q")
                wait_until(child_exited)
                status = child_status
                child = None
                assert os.waitstatus_to_exitcode(status) == 0
                print("Long-path SSH master and compressed reconnect passed.", flush=True)
            finally:
                if child is not None and not child_exited():
                    os.kill(child, signal.SIGTERM)
                    os.waitpid(child, 0)
                for control_socket in runtime.iterdir():
                    subprocess.run(
                        [ssh, "-F", str(client_config), "-S", str(control_socket), "-O", "exit", alias],
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                    )
                if terminal is not None:
                    os.close(terminal)
                daemon.terminate()
                daemon.wait(timeout=5)


if __name__ == "__main__":
    main()
