#!/usr/bin/env python3
"""Validate a cold LoongArch multi-hart boot and preserve reproducible evidence."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import pathlib
import select
import shlex
import signal
import subprocess
import sys
import time


FORBIDDEN = (
    b"Panicked at",
    b"[kernel] panic:",
    b"[sched-stall]",
    b"lock-order inversion",
    b"Segmentation fault",
)


def sha256_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def image_format(path: pathlib.Path) -> str:
    completed = subprocess.run(
        ["qemu-img", "info", "--output=json", str(path)],
        stdout=subprocess.PIPE,
        text=True,
        check=True,
    )
    return str(json.loads(completed.stdout)["format"])


def create_overlay(base: pathlib.Path, overlay: pathlib.Path) -> None:
    subprocess.run(
        [
            "qemu-img",
            "create",
            "-q",
            "-f",
            "qcow2",
            "-F",
            image_format(base),
            "-b",
            str(base.resolve()),
            str(overlay),
        ],
        check=True,
    )


def qemu_version(qemu: str) -> str:
    completed = subprocess.run(
        [qemu, "--version"],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        text=True,
        check=True,
    )
    return completed.stdout.splitlines()[0]


def read_until(
    process: subprocess.Popen[bytes],
    transcript: bytearray,
    marker: bytes,
    timeout: float,
    start: int = 0,
) -> None:
    assert process.stdout is not None
    deadline = time.monotonic() + timeout
    while marker not in transcript[start:]:
        if any(forbidden in transcript for forbidden in FORBIDDEN):
            # Preserve the complete panic/assertion line before terminating
            # QEMU; the first marker often arrives one UART fragment earlier
            # than the source location and failed expression.
            drain(process, transcript)
            raise RuntimeError("forbidden kernel marker observed")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError(f"timed out waiting for {marker!r}")
        ready, _, _ = select.select([process.stdout], [], [], min(remaining, 0.25))
        if not ready:
            if process.poll() is not None:
                raise RuntimeError(f"QEMU exited with status {process.returncode}")
            continue
        chunk = os.read(process.stdout.fileno(), 65536)
        if not chunk:
            raise RuntimeError(f"QEMU closed serial output ({process.poll()})")
        transcript.extend(chunk)
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()


def drain(process: subprocess.Popen[bytes], transcript: bytearray) -> None:
    assert process.stdout is not None
    deadline = time.monotonic() + 0.5
    while time.monotonic() < deadline:
        ready, _, _ = select.select([process.stdout], [], [], 0.05)
        if not ready:
            continue
        chunk = os.read(process.stdout.fileno(), 65536)
        if not chunk:
            return
        transcript.extend(chunk)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--kernel", type=pathlib.Path, default=pathlib.Path("kernel-la"))
    parser.add_argument(
        "--root-image", type=pathlib.Path, default=pathlib.Path("disk-la.img")
    )
    parser.add_argument("--qemu", default="qemu-system-loongarch64")
    parser.add_argument("--smp", type=int, default=12)
    parser.add_argument("--memory", default="2G")
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument(
        "--artifact-root",
        type=pathlib.Path,
        default=pathlib.Path("artifacts/final2026/loongarch-boot"),
    )
    args = parser.parse_args()

    if not 1 <= args.smp <= 12:
        parser.error("--smp must be between 1 and 12 for LoongArch")
    for path in (args.kernel, args.root_image):
        if not path.is_file():
            parser.error(f"missing required file: {path}")

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_dir = args.artifact_root / stamp
    run_dir.mkdir(parents=True, exist_ok=False)
    overlay = run_dir / "root-overlay.qcow2"
    serial_log = run_dir / "serial.log"
    create_overlay(args.root_image, overlay)

    command = [
        args.qemu,
        "-machine",
        "virt",
        "-kernel",
        str(args.kernel.resolve()),
        "-nographic",
        "-monitor",
        "none",
        "-m",
        args.memory,
        "-smp",
        str(args.smp),
        "-drive",
        f"file={overlay},if=none,format=qcow2,id=x0",
        "-device",
        "virtio-blk-pci,drive=x0",
        "-netdev",
        "user,id=net0",
        "-device",
        "virtio-net-pci,netdev=net0",
    ]
    (run_dir / "qemu-command.txt").write_text(
        shlex.join(command) + "\n", encoding="utf-8"
    )
    metadata = {
        "timestamp_utc": stamp,
        "smp": args.smp,
        "memory": args.memory,
        "qemu_version": qemu_version(args.qemu),
        "kernel": str(args.kernel.resolve()),
        "kernel_sha256": sha256_file(args.kernel),
        "root_image": str(args.root_image.resolve()),
        "root_image_sha256": sha256_file(args.root_image),
    }
    (run_dir / "metadata.json").write_text(
        json.dumps(metadata, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )

    transcript = bytearray()
    process = subprocess.Popen(
        command,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        start_new_session=True,
        bufsize=0,
    )
    passed = False
    detail = "completed"
    expected_mask = hex((1 << args.smp) - 1)
    try:
        read_until(process, transcript, f"[smp] online mask {expected_mask}".encode(), args.timeout)
        read_until(process, transcript, b"Rust user shell", args.timeout)
        read_until(process, transcript, b">> ", args.timeout)
        assert process.stdin is not None
        command_start = len(transcript)
        process.stdin.write(b"cpuinfotest\n")
        process.stdin.flush()
        success_marker = f"cpuinfotest passed: online mask {expected_mask}".encode()
        read_until(
            process,
            transcript,
            success_marker,
            args.timeout,
            command_start,
        )
        marker_at = transcript.find(success_marker, command_start)
        read_until(
            process,
            transcript,
            b">> ",
            args.timeout,
            marker_at + len(success_marker),
        )
        passed = True
        process.stdin.write(b"shutdown\n")
        process.stdin.flush()
        try:
            process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            pass
    except Exception as error:
        detail = str(error)
    finally:
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)
        drain(process, transcript)
        serial_log.write_bytes(transcript)

    checked = subprocess.run(
        ["qemu-img", "check", str(overlay)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    (run_dir / "root-overlay-check.txt").write_bytes(checked.stdout)
    if checked.returncode != 0:
        passed = False
        detail = f"{detail}; qemu-img check failed"
    result = {"passed": passed, "detail": detail, "online_mask": expected_mask}
    (run_dir / "result.json").write_text(
        json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    if not passed:
        print(f"LoongArch boot validation failed: {detail}; evidence: {run_dir}", file=sys.stderr)
        return 1
    print(f"LoongArch {args.smp}-hart boot validation passed; evidence: {run_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
