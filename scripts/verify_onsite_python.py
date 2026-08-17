#!/usr/bin/env python3
"""Run Python 3.13 onsite regressions with clean official-image overlays."""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import gzip
import http.server
import json
import os
import pathlib
import pty
import secrets
import shlex
import subprocess
import sys
import threading
import tty

from run_final_2026 import (
    command_output,
    create_overlay,
    drain,
    qemu_command,
    read_until,
    send_line_wait_prompt,
    send_slow,
    sha256_file,
    verify_root_disk_arch,
)


DEFAULT_HTTPS_URL = (
    "https://raw.githubusercontent.com/oscomp/xv6-riscv/"
    "f5dea58cc1057f2b076cdb90b446c2c21d91171e/README"
)
DEFAULT_HTTPS_SHA256 = "05342be05e75f9a75e70fc0bfabd223e7c755e373b6ac584b0918331ea5ae9a2"

MODE_MARKERS = {
    "core": (
        b"PYTHON_LOAD_OK",
        b"PYTHON_REPL_OK",
        b"PYTHON_STDLIB_OK",
        b"PYTHON_VENV_OK",
    ),
    "fs": (
        b"PYTHON_FS_ATOMIC_OK",
        b"PYTHON_SQLITE_OK",
        b"PYTHON_PROC_SYMLINK_OK",
    ),
    "concurrency": (
        b"PYTHON_SUBPROCESS_OK",
        b"PYTHON_THREAD_OK",
        b"PYTHON_MULTIPROCESS_OK",
        b"PYTHON_ASYNCIO_OK",
    ),
    "network": (
        b"PYTHON_DNS_OK",
        b"PYTHON_HTTPS_OK",
        b"PYTHON_REMOTE_SIDE_EFFECT_OK",
    ),
}


class ReceiptServer:
    def __init__(self, port: int, token: str) -> None:
        self.token = token
        self.received = threading.Event()
        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_POST(self) -> None:
                try:
                    length = int(self.headers.get("Content-Length", "0"))
                    payload = json.loads(self.rfile.read(length))
                    accepted = self.path == "/submit" and payload.get("token") == owner.token
                    if accepted:
                        owner.received.set()
                    body = json.dumps(
                        {"accepted": accepted, "token": payload.get("token")},
                        sort_keys=True,
                    ).encode()
                    self.send_response(200 if accepted else 400)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)
                except (OSError, ValueError, json.JSONDecodeError):
                    self.send_error(400)

            def log_message(self, format: str, *args: object) -> None:
                return

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def start(self) -> None:
        self.thread.start()

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def install_blob(
    master: int,
    transcript: bytearray,
    payload: bytes,
    guest_path: str,
) -> None:
    encoded = base64.b64encode(gzip.compress(payload, mtime=0))
    temporary = guest_path + ".gz.b64"
    compressed = guest_path + ".gz"
    send_line_wait_prompt(
        master,
        transcript,
        (
            f"/busybox sh -c '/busybox rm -f {shlex.quote(temporary)} "
            f"{shlex.quote(compressed)} {shlex.quote(guest_path)}'"
        ).encode(),
    )
    for offset in range(0, len(encoded), 192):
        chunk = encoded[offset : offset + 192]
        send_line_wait_prompt(
            master,
            transcript,
            b"/busybox sh -c 'printf %s " + chunk + b" >> " + temporary.encode() + b"'",
        )
    send_line_wait_prompt(
        master,
        transcript,
        (
            f"/busybox sh -c '/busybox base64 -d {shlex.quote(temporary)} > "
            f"{shlex.quote(compressed)} && /busybox gzip -dc {shlex.quote(compressed)} > "
            f"{shlex.quote(guest_path)} && /busybox rm -f {shlex.quote(temporary)} "
            f"{shlex.quote(compressed)}'"
        ).encode(),
    )


def guest_driver(args: argparse.Namespace, token: str) -> bytes:
    network = args.mode in {"network", "all"}
    interactive = args.mode in {"core", "all"}
    environment = [
        "export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "export HOME=/root TMPDIR=/tmp TERM=xterm-256color PYTHONUNBUFFERED=1",
        "export PYTHONDONTWRITEBYTECODE=0",
    ]
    if network:
        environment.extend(
            [
                f"export https_proxy={shlex.quote(args.proxy)} HTTPS_PROXY={shlex.quote(args.proxy)}",
                "export no_proxy=10.0.2.2,127.0.0.1 NO_PROXY=10.0.2.2,127.0.0.1",
                f"export PYTHON_TEST_HTTPS_URL={shlex.quote(args.https_url)}",
                f"export PYTHON_TEST_HTTPS_SHA256={shlex.quote(args.https_sha256)}",
                f"export PYTHON_TEST_POST_URL={shlex.quote(args.post_url)}",
                f"export PYTHON_TEST_POST_TOKEN={shlex.quote(token)}",
            ]
        )
    lines = [
        "#!/bin/bash",
        "set +e",
        "mount -t proc proc /proc 2>/dev/null || true",
        "mount -t sysfs sysfs /sys 2>/dev/null || true",
        "mount -t devtmpfs devtmpfs /dev 2>/dev/null || true",
        "mkdir -p /dev/shm",
        "mount -t tmpfs tmpfs /dev/shm 2>/dev/null || true",
        *environment,
    ]
    if network:
        lines.extend(
            [
                "printf 'nameserver 10.0.2.3\\noptions timeout:2 attempts:3\\n' > /etc/resolv.conf",
                "echo PYTHON_NETWORK_DIAG_BEGIN",
                "cat /etc/resolv.conf",
                "getent ahostsv4 github.com 2>&1",
                "echo PYTHON_GETENT_RC=$?",
                "echo PYTHON_NETWORK_DIAG_END",
            ]
        )
    if interactive:
        lines.extend(
            [
                "python3 -h >/tmp/python-help.txt 2>&1",
                "load_rc=$?",
                "if test $load_rc -eq 0 -a -s /tmp/python-help.txt; then echo PYTHON_LOAD_OK; else echo PYTHON_LOAD_FAILED; fi",
                "echo PYTHON_REPL_BEGIN",
                "python3",
                "repl_rc=$?",
                "echo PYTHON_REPL_RC=$repl_rc",
                "import_rc=0",
                "for module in asyncio compileall gzip hashlib json multiprocessing pathlib py_compile select shutil socket sqlite3 subprocess threading urllib.request venv; do python3 -c \"import $module\"; item_rc=$?; if test $item_rc -ne 0; then echo PYTHON_IMPORT_FAILED=$module; import_rc=$item_rc; break; fi; echo PYTHON_IMPORT_OK=$module; done",
            ]
        )
    else:
        lines.extend(["load_rc=0", "repl_rc=0", "import_rc=0"])
    lines.extend(
        [
            f"python3 /tmp/python_regression.py --mode {shlex.quote(args.mode)}",
            "test_rc=$?",
            "rc=$((load_rc || repl_rc || import_rc || test_rc))",
            "echo PYTHON_TEST_RC=$test_rc",
            "echo PYTHON_DRIVER_DONE rc=$rc",
            "exit $rc",
        ]
    )
    return ("\n".join(lines) + "\n").encode()


def required_markers(mode: str) -> tuple[bytes, ...]:
    if mode == "all":
        return tuple(marker for name in ("core", "fs", "concurrency", "network") for marker in MODE_MARKERS[name]) + (
            b"PYTHON_ONSITE_ALL_OK",
        )
    return MODE_MARKERS[mode]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"), required=True)
    parser.add_argument("--mode", choices=("core", "fs", "concurrency", "network", "all"), default="all")
    parser.add_argument("--test-image", required=True, type=pathlib.Path)
    parser.add_argument("--root-image", type=pathlib.Path)
    parser.add_argument("--kernel", type=pathlib.Path)
    parser.add_argument("--test-script", type=pathlib.Path, default=pathlib.Path("tests/onsite_python/python_regression.py"))
    parser.add_argument("--qemu")
    parser.add_argument("--smp", type=int)
    parser.add_argument("--memory", default="2G")
    parser.add_argument("--timeout", type=float, default=900)
    parser.add_argument("--artifact-root", type=pathlib.Path, default=pathlib.Path("artifacts/onsite-python"))
    parser.add_argument("--proxy", default="http://10.0.2.2:27898")
    parser.add_argument("--https-url", default=DEFAULT_HTTPS_URL)
    parser.add_argument("--https-sha256", default=DEFAULT_HTTPS_SHA256)
    parser.add_argument("--post-port", type=int, default=27899)
    parser.add_argument("--post-url", default="http://10.0.2.2:27899/submit")
    args = parser.parse_args()

    if args.smp is None:
        args.smp = 4 if args.arch == "riscv64" else 12
    platform_limit = 8 if args.arch == "riscv64" else 12
    if not 1 <= args.smp <= platform_limit:
        parser.error(f"--smp must be between 1 and {platform_limit} for {args.arch}")
    if args.kernel is None:
        args.kernel = pathlib.Path("kernel-rv" if args.arch == "riscv64" else "kernel-la")
    if args.root_image is None:
        args.root_image = pathlib.Path("disk.img" if args.arch == "riscv64" else "disk-la.img")
    if args.qemu is None:
        args.qemu = "qemu-system-riscv64" if args.arch == "riscv64" else "qemu-system-loongarch64"
    for required in (args.test_image, args.root_image, args.kernel, args.test_script):
        if not required.is_file():
            parser.error(f"missing required file: {required}")
    try:
        verify_root_disk_arch(args.root_image, args.arch)
    except ValueError as error:
        parser.error(str(error))
    if not command_output(["sh", "-c", f"command -v {shlex.quote(args.qemu)}"]):
        parser.error(f"QEMU executable not found: {args.qemu}")

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_dir = args.artifact_root / f"{stamp}-{args.arch}-{args.mode}"
    run_dir.mkdir(parents=True, exist_ok=False)
    test_overlay = run_dir / "test-overlay.qcow2"
    root_overlay = run_dir / "root-overlay.qcow2"
    serial_log = run_dir / "serial.log"
    create_overlay(args.test_image, test_overlay)
    create_overlay(args.root_image, root_overlay)

    token = "PYTHON_POST_" + secrets.token_hex(12)
    receipt = ReceiptServer(args.post_port, token) if args.mode in {"network", "all"} else None
    if receipt is not None:
        receipt.start()

    repo = pathlib.Path(__file__).resolve().parent.parent
    metadata = {
        "timestamp_utc": stamp,
        "arch": args.arch,
        "mode": args.mode,
        "smp": args.smp,
        "memory": args.memory,
        "git_head": command_output(["git", "rev-parse", "HEAD"], cwd=repo),
        "git_status": command_output(["git", "status", "--short"], cwd=repo).splitlines(),
        "qemu_version": command_output([args.qemu, "--version"]).splitlines()[:1],
        "kernel_sha256": sha256_file(args.kernel),
        "test_image_sha256": sha256_file(args.test_image),
        "root_image_sha256": sha256_file(args.root_image),
        "test_script_sha256": sha256_file(args.test_script),
        "https_url": args.https_url if args.mode in {"network", "all"} else None,
        "https_sha256": args.https_sha256 if args.mode in {"network", "all"} else None,
    }
    (run_dir / "metadata.json").write_text(
        json.dumps(metadata, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    command = qemu_command(args, test_overlay, root_overlay)
    (run_dir / "qemu-command.txt").write_text(shlex.join(command) + "\n", encoding="utf-8")

    transcript = bytearray()
    master, slave = pty.openpty()
    tty.setraw(slave)
    process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave)
    os.close(slave)
    completed = False
    overlays_ok = False
    try:
        read_until(master, transcript, b">> ", 120)
        send_line_wait_prompt(master, transcript, b"/busybox mount -t ext4 vda /mnt", timeout=60)
        install_blob(master, transcript, args.test_script.read_bytes(), "/mnt/tmp/python_regression.py")
        install_blob(master, transcript, guest_driver(args, token), "/mnt/tmp/onsite-python-run.sh")
        preflight_start = len(transcript)
        send_slow(
            master,
            transcript,
            b"/busybox sh -c 'test -x /mnt/usr/bin/python3 -a -s /mnt/tmp/python_regression.py -a -s /mnt/tmp/onsite-python-run.sh && echo PYTHON_PREFLIGHT_OK'\n",
        )
        marker = b"\nPYTHON_PREFLIGHT_OK"
        read_until(master, transcript, marker, 30, start=preflight_start)
        prompt_start = transcript.find(marker, preflight_start)
        read_until(master, transcript, b">> ", 30, start=prompt_start)

        start = len(transcript)
        send_slow(master, transcript, b"/busybox chroot /mnt /bin/bash /tmp/onsite-python-run.sh\n")
        if args.mode in {"core", "all"}:
            read_until(master, transcript, b"PYTHON_REPL_BEGIN", 60, start=start)
            read_until(master, transcript, b">>> ", 60, start=start)
            repl_start = len(transcript)
            send_slow(master, transcript, b'print("PYTHON_REPL_OK")\n')
            read_until(master, transcript, b"PYTHON_REPL_OK", 30, start=repl_start)
            os.write(master, b"\x04")
        read_until(master, transcript, b"PYTHON_DRIVER_DONE rc=", args.timeout, start=start)
        completed = True
        send_slow(master, transcript, b"/shutdown\n")
        try:
            process.wait(timeout=60)
        except subprocess.TimeoutExpired:
            process.terminate()
            process.wait(timeout=10)
    finally:
        if process.poll() is None:
            try:
                os.write(master, b"\x01x")
            except OSError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=10)
        drain(master, transcript)
        os.close(master)
        serial_log.write_bytes(transcript)
        overlays_ok = True
        for overlay in (test_overlay, root_overlay):
            with (run_dir / f"{overlay.stem}-check.txt").open("wb") as output:
                checked = subprocess.run(
                    ["qemu-img", "check", str(overlay)],
                    stdout=output,
                    stderr=subprocess.STDOUT,
                    check=False,
                )
                overlays_ok &= checked.returncode == 0
        if receipt is not None:
            receipt.close()

    failures: list[str] = []
    if not completed or b"PYTHON_DRIVER_DONE rc=0" not in transcript:
        failures.append("guest driver did not finish with rc=0")
    missing = [marker for marker in required_markers(args.mode) if marker not in transcript]
    if missing:
        failures.append(f"missing markers: {missing!r}")
    if not overlays_ok:
        failures.append("one or more qcow2 overlays failed qemu-img check")
    if args.smp > 1:
        expected_mask = f"[smp] online mask {hex((1 << args.smp) - 1)}".encode()
        if expected_mask not in transcript:
            failures.append(f"missing online CPU marker: {expected_mask!r}")
    if receipt is not None and not receipt.received.is_set():
        failures.append("host did not observe the POST side effect")
    if failures:
        print(f"Python onsite regression failed: {'; '.join(failures)}", file=sys.stderr)
        print(f"evidence: {run_dir}", file=sys.stderr)
        return 1
    print(f"Python onsite regression passed: {args.arch} {args.mode}")
    print(f"evidence: {run_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
