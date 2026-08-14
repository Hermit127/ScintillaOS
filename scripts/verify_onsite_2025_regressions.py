#!/usr/bin/env python3
"""Compile and run timer, TTY, and Ext4 overwrite-rename regressions in Alpine."""

from __future__ import annotations

import argparse
import base64
import os
import pathlib
import pty
import select
import socket
import subprocess
import sys
import tempfile
import threading
import time
import tty


def read_until(master: int, transcript: bytearray, marker: bytes, timeout: float) -> None:
    deadline = time.monotonic() + timeout
    while marker not in transcript:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"timed out waiting for {marker!r}")
        ready, _, _ = select.select([master], [], [], min(remaining, 0.25))
        if not ready:
            continue
        chunk = os.read(master, 65536)
        if not chunk:
            raise RuntimeError("QEMU serial console closed unexpectedly")
        transcript.extend(chunk)
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()
        respond_to_terminal_queries(master, chunk)


def respond_to_terminal_queries(master: int, chunk: bytes) -> None:
    if b"\x1b[6n" in chunk:
        os.write(master, b"\x1b[24;1R")


def drain(master: int, transcript: bytearray, duration: float = 0.05) -> None:
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        remaining = deadline - time.monotonic()
        ready, _, _ = select.select([master], [], [], max(0.0, min(0.01, remaining)))
        if not ready:
            continue
        try:
            chunk = os.read(master, 65536)
        except OSError:
            return
        if not chunk:
            return
        transcript.extend(chunk)
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()
        respond_to_terminal_queries(master, chunk)


def send_slow(master: int, transcript: bytearray, data: bytes, delay: float = 0.001) -> None:
    for byte in data:
        os.write(master, bytes([byte]))
        drain(master, transcript, delay)


def encoded_install(source: pathlib.Path, destination: str) -> bytes:
    encoded = base64.b64encode(source.read_bytes())
    encoded_path = destination + ".b64"
    commands = [b"rm -f " + encoded_path.encode() + b" " + destination.encode() + b"\n"]
    # Keep every terminal line well below the common 4096-byte canonical-mode
    # limit.  This also makes the transfer reliable on slower UART models.
    for offset in range(0, len(encoded), 512):
        chunk = encoded[offset : offset + 512]
        commands.append(
            b"printf '%s' '" + chunk + b"' >> " + encoded_path.encode() + b"\n"
        )
    commands.append(
        b"base64 -d " + encoded_path.encode() + b" > " + destination.encode() + b"\n"
    )
    return b"".join(commands)


def start_tcp_eof_server() -> tuple[threading.Thread, list[BaseException]]:
    """Serve three deterministic connections through QEMU's 10.0.2.2 gateway."""
    ready = threading.Event()
    errors: list[BaseException] = []

    def serve() -> None:
        try:
            with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
                listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
                listener.bind(("127.0.0.1", 27899))
                listener.listen(3)
                listener.settimeout(120)
                ready.set()
                for _ in range(3):
                    connection, _ = listener.accept()
                    with connection:
                        try:
                            connection.sendall(b"ONSITE_TCP_EOF\n")
                        except (BrokenPipeError, ConnectionResetError):
                            # The getsockname probes intentionally close without reading.
                            pass
        except BaseException as error:  # surfaced in the main test thread below
            errors.append(error)
            ready.set()

    thread = threading.Thread(target=serve, name="onsite25-tcp-eof", daemon=True)
    thread.start()
    if not ready.wait(5):
        raise RuntimeError("host TCP EOF regression server did not start")
    if errors:
        raise RuntimeError("host TCP EOF regression server failed") from errors[0]
    return thread, errors


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-image", required=True, type=pathlib.Path)
    parser.add_argument("--root-disk", default="disk.img", type=pathlib.Path)
    parser.add_argument(
        "--kernel",
        default="target/riscv64gc-unknown-none-elf/release/os.bin",
        type=pathlib.Path,
    )
    parser.add_argument("--log", type=pathlib.Path)
    parser.add_argument(
        "--network-sockname",
        action="store_true",
        help="also run host-gateway getsockname and TCP EOF regressions",
    )
    args = parser.parse_args()

    repo = pathlib.Path(__file__).resolve().parent.parent
    sources = [
        repo / "tests/onsite2025/timer_regression.c",
        repo / "tests/onsite2025/tty_regression.c",
        repo / "tests/onsite2025/tty_icrnl_regression.c",
    ]
    if args.network_sockname:
        sources.extend(
            [
                repo / "tests/onsite2025/network_sockname_regression.c",
                repo / "tests/onsite2025/network_tcp_eof_regression.c",
            ]
        )
    for required in (args.base_image, args.root_disk, args.kernel, *sources):
        if not required.is_file():
            parser.error(f"missing required file: {required}")

    transcript = bytearray()
    eof_server = None
    eof_server_errors: list[BaseException] = []
    if args.network_sockname:
        eof_server, eof_server_errors = start_tcp_eof_server()
    with tempfile.TemporaryDirectory(prefix="onsite25-regression-") as temp_dir:
        overlay = pathlib.Path(temp_dir) / "alpine.qcow2"
        subprocess.run(
            [
                "qemu-img", "create", "-f", "qcow2", "-F", "raw", "-b",
                str(args.base_image.resolve()), str(overlay),
            ],
            check=True,
        )
        master, slave = pty.openpty()
        tty.setraw(slave)
        command = [
            "qemu-system-riscv64", "-machine", "virt", "-cpu", "max",
            "-kernel", str(args.kernel), "-nographic", "-m", "1G", "-smp", "1",
            "-drive", f"file={overlay},if=none,format=qcow2,id=x0",
            "-device", "virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0",
            "-drive", f"file={args.root_disk},if=none,format=raw,id=x1,file.locking=off",
            "-device", "virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1",
        ]
        if args.network_sockname:
            command.extend(
                [
                    "-netdev", "user,id=net0",
                    "-device", "virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2",
                ]
            )
        process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        try:
            read_until(master, transcript, b">> ", 35)
            setup = (
                b"/busybox mount -t ext4 vda /mnt\n"
                b"/busybox chroot /mnt /bin/sh\n"
                b"export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n"
            )
            send_slow(master, transcript, setup)
            read_until(master, transcript, b"/ # ", 10)
            send_slow(master, transcript, encoded_install(sources[0], "/tmp/timer_regression.c"))
            send_slow(master, transcript, encoded_install(sources[1], "/tmp/tty_regression.c"))
            send_slow(
                master,
                transcript,
                encoded_install(sources[2], "/tmp/tty_icrnl_regression.c"),
            )
            send_slow(
                master,
                transcript,
                b"gcc -O2 -Wall -Wextra /tmp/tty_icrnl_regression.c "
                b"-o /tmp/tty_icrnl_regression\n"
                b"/tmp/tty_icrnl_regression; echo TTY_ICRNL_RC=$?\n",
            )
            read_until(master, transcript, b"TTY_ICRNL_SEND_CR", 30)
            os.write(master, b"\r")
            read_until(master, transcript, b"TTY_RAW_SEND_CR", 10)
            os.write(master, b"\r")
            read_until(master, transcript, b"TTY_RAW_SEND_DEL", 10)
            os.write(master, b"\x7f")
            read_until(master, transcript, b"TTY_ERASE_SEND_DEL", 10)
            os.write(master, b"lyzhcX\x7fm\r")
            read_until(master, transcript, b"TTY_ERASE_SEND_BS", 10)
            os.write(master, b"abX\x08c\r")
            read_until(master, transcript, b"TTY_ICRNL_RC=0", 10)
            if args.network_sockname:
                send_slow(
                    master,
                    transcript,
                    encoded_install(sources[3], "/tmp/network_sockname_regression.c"),
                )
                send_slow(
                    master,
                    transcript,
                    encoded_install(sources[4], "/tmp/network_tcp_eof_regression.c"),
                )
            commands = (
                b"gcc -O2 -Wall -Wextra /tmp/timer_regression.c -o /tmp/timer_regression\n"
                b"/tmp/timer_regression; echo TIMER_TEST_RC=$?\n"
                b"gcc -O2 -Wall -Wextra /tmp/tty_regression.c -o /tmp/tty_regression\n"
            )
            if args.network_sockname:
                commands += (
                    b"gcc -O2 -Wall -Wextra /tmp/network_sockname_regression.c "
                    b"-o /tmp/network_sockname_regression\n"
                    b"/tmp/network_sockname_regression; echo NETWORK_SOCKNAME_RC=$?\n"
                    b"gcc -O2 -Wall -Wextra /tmp/network_tcp_eof_regression.c "
                    b"-o /tmp/network_tcp_eof_regression\n"
                    b"/tmp/network_tcp_eof_regression; echo NETWORK_TCP_EOF_RC=$?\n"
                )
            commands += (
                b"mkdir -p /tmp/ext4-regression/.git\n"
                b"printf 'old\\n' > /tmp/ext4-regression/.git/replace\n"
                b"printf 'new\\n' > /tmp/ext4-regression/.git/replace.lock\n"
                b"if mv -f /tmp/ext4-regression/.git/replace.lock /tmp/ext4-regression/.git/replace && "
                b"test \"$(cat /tmp/ext4-regression/.git/replace)\" = new && "
                b"test ! -e /tmp/ext4-regression/.git/replace.lock; then "
                b"echo EXT4_RENAME_OK; else echo EXT4_RENAME_FAILED; fi\n"
                b"/tmp/tty_regression; echo TTY_TEST_RC=$?; "
                b"printf 'REGRESSION_BATCH_%s\\n' DONE\n"
            )
            start = len(transcript)
            send_slow(master, transcript, commands)
            deadline = time.monotonic() + 120
            while b"REGRESSION_BATCH_DONE" not in transcript[start:]:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError("timed out waiting for regression batch")
                ready, _, _ = select.select([master], [], [], min(remaining, 0.25))
                if ready:
                    chunk = os.read(master, 65536)
                    if not chunk:
                        raise RuntimeError("QEMU serial console closed unexpectedly")
                    transcript.extend(chunk)
                    sys.stdout.buffer.write(chunk)
                    sys.stdout.buffer.flush()
        finally:
            try:
                os.write(master, b"\x01x")
            except OSError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=5)
            drain(master, transcript)
            os.close(master)
            if args.log:
                args.log.parent.mkdir(parents=True, exist_ok=True)
                args.log.write_bytes(transcript)

        subprocess.run(["qemu-img", "check", str(overlay)], check=True)

    if eof_server is not None:
        eof_server.join(timeout=5)
        if eof_server.is_alive():
            raise RuntimeError("host TCP EOF regression server did not finish")
        if eof_server_errors:
            raise RuntimeError("host TCP EOF regression server failed") from eof_server_errors[0]

    required = (
        b"TIMER_REGRESSION_OK",
        b"TIMER_TEST_RC=0",
        b"TTY_REGRESSION_OK",
        b"TTY_TEST_RC=0",
        b"TTY_ICRNL_OK",
        b"TTY_ICRNL_RC=0",
        b"TTY_ERASE_OK",
        b"EXT4_RENAME_OK",
    )
    if args.network_sockname:
        required += (
            b"NETWORK_SOCKNAME_OK",
            b"NETWORK_SOCKNAME_RC=0",
            b"NETWORK_TCP_EOF_OK",
            b"NETWORK_TCP_EOF_RC=0",
        )
    missing = [marker for marker in required if marker not in transcript]
    if missing:
        print(f"missing regression markers: {missing!r}", file=sys.stderr)
        return 1
    print("onsite 2025 timer/TTY/Ext4 regressions passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
