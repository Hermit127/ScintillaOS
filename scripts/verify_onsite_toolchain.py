#!/usr/bin/env python3
"""Run the six-tool onsite workload with clean official-image overlays."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import pathlib
import pty
import secrets
import shlex
import subprocess
import sys
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
from verify_onsite_python import ReceiptServer, install_blob


DEFAULT_HTTPS_URL = (
    "https://raw.githubusercontent.com/oscomp/xv6-riscv/"
    "f5dea58cc1057f2b076cdb90b446c2c21d91171e/README"
)

MODE_MARKERS = {
    "versions": (
        b"MAKE_LOAD_OK", b"CMAKE_LOAD_OK", b"NINJA_LOAD_OK",
        b"CARGO_LOAD_OK", b"PYTHON_LOAD_OK", b"CURL_LOAD_OK",
    ),
    "build": (
        b"MAKE_BUILD_OK", b"CMAKE_MAKE_BUILD_OK",
        b"NINJA_BUILD_OK", b"CMAKE_NINJA_BUILD_OK",
    ),
    "cargo": (b"CARGO_BUILD_OK", b"CARGO_TEST_OK"),
    "python": (b"PYTHON_TOOLCHAIN_OK",),
    "curl": (b"CURL_HTTPS_OK", b"CURL_REMOTE_SIDE_EFFECT_OK"),
}


def required_markers(mode: str) -> tuple[bytes, ...]:
    if mode == "all":
        return tuple(
            marker
            for name in ("versions", "build", "cargo", "python", "curl")
            for marker in MODE_MARKERS[name]
        ) + (b"TOOLCHAIN_COMBINED_OK", b"TOOLCHAIN_ONSITE_ALL_OK")
    return MODE_MARKERS[mode]


def guest_driver(args: argparse.Namespace, token: str) -> bytes:
    network = args.mode in {"curl", "all"}
    lines = [
        "#!/bin/bash",
        "set +e",
        "mount -t proc proc /proc 2>/dev/null || true",
        "mount -t sysfs sysfs /sys 2>/dev/null || true",
        "mount -t devtmpfs devtmpfs /dev 2>/dev/null || true",
        "mkdir -p /dev/shm",
        "mount -t tmpfs tmpfs /dev/shm 2>/dev/null || true",
        "export PATH=/root/.cargo/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        "export HOME=/root TMPDIR=/tmp TERM=xterm-256color PYTHONUNBUFFERED=1",
        "export CARGO_HOME=/root/.cargo RUSTUP_HOME=/root/.rustup CARGO_NET_OFFLINE=true",
    ]
    if network:
        lines.extend(
            [
                "printf 'nameserver 10.0.2.3\\noptions timeout:2 attempts:3\\n' > /etc/resolv.conf",
                f"export http_proxy={shlex.quote(args.proxy)} https_proxy={shlex.quote(args.proxy)}",
                f"export HTTP_PROXY={shlex.quote(args.proxy)} HTTPS_PROXY={shlex.quote(args.proxy)}",
                "export no_proxy=10.0.2.2,127.0.0.1 NO_PROXY=10.0.2.2,127.0.0.1",
                f"export TOOLCHAIN_HTTPS_URL={shlex.quote(args.https_url)}",
                f"export TOOLCHAIN_POST_URL={shlex.quote(args.post_url)}",
                f"export TOOLCHAIN_POST_TOKEN={shlex.quote(token)}",
                "echo TOOLCHAIN_NETWORK_DIAG_BEGIN",
                "cat /etc/resolv.conf",
                "getent ahostsv4 github.com 2>&1",
                "echo TOOLCHAIN_GETENT_RC=$?",
                "echo TOOLCHAIN_NETWORK_DIAG_END",
            ]
        )
    lines.extend(
        [
            f"python3 /tmp/toolchain_regression.py --mode {shlex.quote(args.mode)}",
            "test_rc=$?",
            "echo TOOLCHAIN_TEST_RC=$test_rc",
            "echo TOOLCHAIN_DRIVER_DONE rc=$test_rc",
            "exit $test_rc",
        ]
    )
    return ("\n".join(lines) + "\n").encode()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"), required=True)
    parser.add_argument(
        "--mode", choices=("versions", "build", "cargo", "python", "curl", "all"), default="all"
    )
    parser.add_argument("--test-image", required=True, type=pathlib.Path)
    parser.add_argument("--root-image", type=pathlib.Path)
    parser.add_argument("--kernel", type=pathlib.Path)
    parser.add_argument(
        "--test-script", type=pathlib.Path,
        default=pathlib.Path("tests/onsite_toolchain/toolchain_regression.py"),
    )
    parser.add_argument("--qemu")
    parser.add_argument("--smp", type=int)
    parser.add_argument("--memory", default="2G")
    parser.add_argument("--timeout", type=float, default=1800)
    parser.add_argument(
        "--artifact-root", type=pathlib.Path,
        default=pathlib.Path("artifacts/onsite-toolchain"),
    )
    parser.add_argument("--proxy", default="http://10.0.2.2:27898")
    parser.add_argument("--https-url", default=DEFAULT_HTTPS_URL)
    parser.add_argument("--post-port", type=int, default=27900)
    parser.add_argument("--post-url", default="http://10.0.2.2:27900/submit")
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

    network = args.mode in {"curl", "all"}
    token = "TOOLCHAIN_POST_" + secrets.token_hex(12)
    receipt = ReceiptServer(args.post_port, token) if network else None
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
        "https_url": args.https_url if network else None,
    }
    (run_dir / "metadata.json").write_text(
        json.dumps(metadata, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    command = qemu_command(args, test_overlay, root_overlay)
    (run_dir / "qemu-command.txt").write_text(shlex.join(command) + "\n", encoding="utf-8")

    transcript = bytearray()
    completed = False
    overlays_ok = False
    master = slave = -1
    process: subprocess.Popen[bytes] | None = None
    try:
        master, slave = pty.openpty()
        tty.setraw(slave)
        process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        slave = -1
        read_until(master, transcript, b">> ", 120)
        send_line_wait_prompt(master, transcript, b"/busybox mount -t ext4 vda /mnt", timeout=60)
        install_blob(master, transcript, args.test_script.read_bytes(), "/mnt/tmp/toolchain_regression.py")
        install_blob(master, transcript, guest_driver(args, token), "/mnt/tmp/onsite-toolchain-run.sh")
        preflight_start = len(transcript)
        send_slow(
            master,
            transcript,
            b"/busybox sh -c 'test -s /mnt/tmp/toolchain_regression.py -a -s /mnt/tmp/onsite-toolchain-run.sh && echo TOOLCHAIN_PREFLIGHT_OK'\n",
        )
        marker = b"\nTOOLCHAIN_PREFLIGHT_OK"
        read_until(master, transcript, marker, 30, start=preflight_start)
        prompt_start = transcript.find(marker, preflight_start)
        read_until(master, transcript, b">> ", 30, start=prompt_start)

        start = len(transcript)
        send_slow(master, transcript, b"/busybox chroot /mnt /bin/bash /tmp/onsite-toolchain-run.sh\n")
        read_until(master, transcript, b"TOOLCHAIN_DRIVER_DONE rc=", args.timeout, start=start)
        completed = True
        send_slow(master, transcript, b"/shutdown\n")
        try:
            process.wait(timeout=60)
        except subprocess.TimeoutExpired:
            process.terminate()
            process.wait(timeout=10)
    finally:
        if process is not None and process.poll() is None:
            try:
                os.write(master, b"\x01x")
            except OSError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.terminate()
                process.wait(timeout=10)
        if master >= 0:
            drain(master, transcript)
            os.close(master)
        if slave >= 0:
            os.close(slave)
        serial_log.write_bytes(transcript)
        overlays_ok = True
        for overlay in (test_overlay, root_overlay):
            with (run_dir / f"{overlay.stem}-check.txt").open("wb") as output:
                checked = subprocess.run(
                    ["qemu-img", "check", str(overlay)], stdout=output,
                    stderr=subprocess.STDOUT, check=False,
                )
                overlays_ok &= checked.returncode == 0
        if receipt is not None:
            receipt.close()

    failures: list[str] = []
    if not completed or b"TOOLCHAIN_DRIVER_DONE rc=0" not in transcript:
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
        print(f"Toolchain onsite regression failed: {'; '.join(failures)}", file=sys.stderr)
        print(f"evidence: {run_dir}", file=sys.stderr)
        return 1
    print(f"Toolchain onsite regression passed: {args.arch} {args.mode}")
    print(f"evidence: {run_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
