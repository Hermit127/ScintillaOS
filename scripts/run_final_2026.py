#!/usr/bin/env python3
"""Run the official 2026 final workload with clean overlays and durable evidence."""

from __future__ import annotations

import argparse
import base64
import datetime as dt
import hashlib
import json
import os
import pathlib
import pty
import select
import shlex
import shutil
import subprocess
import sys
import tempfile
import time
import tty


CAGENT_CASES = (
    "factorial",
    "date",
    "network",
    "cpu",
    "kernel",
    "fs-create",
    "fs-readwrite",
    "fs-directory",
    "fs-search",
    "fs-usage",
)


def sha256_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while chunk := source.read(8 * 1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


def command_output(command: list[str], cwd: pathlib.Path | None = None) -> str:
    completed = subprocess.run(
        command,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    return completed.stdout.strip()


def image_format(path: pathlib.Path) -> str:
    completed = subprocess.run(
        ["qemu-img", "info", "--output=json", str(path)],
        text=True,
        stdout=subprocess.PIPE,
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


def verify_root_disk_arch(path: pathlib.Path, arch: str) -> None:
    """Use the embedded shell ELF header to catch cross-architecture disks."""
    if shutil.which("debugfs") is None or image_format(path) != "raw":
        return
    with tempfile.TemporaryDirectory(prefix="final2026-root-arch-") as temporary:
        extracted = pathlib.Path(temporary) / "user_shell"
        result = subprocess.run(
            ["debugfs", "-R", f"dump /bin/user_shell {extracted}", str(path)],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        if result.returncode != 0 or not extracted.is_file():
            return
        with extracted.open("rb") as source:
            header = source.read(20)
        if len(header) < 20 or header[:4] != b"\x7fELF":
            return
        machine = int.from_bytes(header[18:20], "little")
        expected = 243 if arch == "riscv64" else 258
        if machine != expected:
            raise ValueError(
                f"root image {path} contains e_machine={machine} /bin/user_shell; "
                f"{arch} requires e_machine={expected}"
            )


def read_until(
    master: int,
    transcript: bytearray,
    marker: bytes,
    timeout: float,
    start: int = 0,
) -> None:
    deadline = time.monotonic() + timeout
    while marker not in transcript[start:]:
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


def drain(master: int, transcript: bytearray, duration: float = 0.05) -> None:
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], min(0.01, deadline - time.monotonic()))
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


def send_slow(master: int, transcript: bytearray, data: bytes) -> None:
    for byte in data:
        os.write(master, bytes([byte]))
        drain(master, transcript, 0.001)


def send_line_wait_prompt(
    master: int,
    transcript: bytearray,
    line: bytes,
    prompt: bytes = b">> ",
    timeout: float = 20,
) -> None:
    start = len(transcript)
    send_slow(master, transcript, line.rstrip(b"\n") + b"\n")
    read_until(master, transcript, prompt, timeout, start=start)


def guest_driver(mode: str) -> bytes:
    if mode == "cagent":
        body = """
cd /glibc || exit 125
./cagent_testcode.sh
rc=$?
echo FINAL2026_CAGENT_RC=$rc
echo FINAL2026_DRIVER_DONE rc=$rc
exit $rc
"""
    elif mode == "q21":
        body = """
mount -t proc proc /proc 2>/dev/null || true
mount -t sysfs sysfs /sys 2>/dev/null || true
mount -t devtmpfs devtmpfs /dev 2>/dev/null || true
export PATH=/root/.cargo/bin:/usr/local/bin:/usr/bin:/bin:/sbin:/usr/sbin
export HOME=/root RUSTUP_HOME=/root/.rustup CARGO_HOME=/root/.cargo
export RUSTUP_TOOLCHAIN=nightly-2026-05-28 CARGO_NET_OFFLINE=true
toolchain_rc=1
if rustc --version && cargo --version; then
    echo BUILDSTORM_TOOLCHAIN ok
    toolchain_rc=0
else
    echo BUILDSTORM_TOOLCHAIN fail
fi
rm -rf /tmp/minibuild
minibuild_rc=1
if cargo new --vcs none /tmp/minibuild >/dev/null 2>&1 \
   && (cd /tmp/minibuild && cargo build >/dev/null 2>&1) \
   && test "$(/tmp/minibuild/target/debug/minibuild)" = "Hello, world!"; then
    echo BUILDSTORM_MINIBUILD ok
    minibuild_rc=0
else
    echo BUILDSTORM_MINIBUILD fail
fi
rc=$((toolchain_rc || minibuild_rc))
echo FINAL2026_Q21_RC=$rc
echo FINAL2026_DRIVER_DONE rc=$rc
exit $rc
"""
    elif mode == "buildstorm":
        body = """
cd /glibc || exit 125
./buildstorm_testcode.sh
rc=$?
echo FINAL2026_BUILDSTORM_RC=$rc
echo FINAL2026_DRIVER_DONE rc=$rc
exit $rc
"""
    elif mode == "all":
        body = """
cd /glibc || exit 125
./cagent_testcode.sh
cagent_rc=$?
echo FINAL2026_CAGENT_RC=$cagent_rc
./buildstorm_testcode.sh
buildstorm_rc=$?
echo FINAL2026_BUILDSTORM_RC=$buildstorm_rc
rc=$((cagent_rc || buildstorm_rc))
echo FINAL2026_DRIVER_DONE rc=$rc
exit $rc
"""
    else:
        raise ValueError(f"unsupported mode: {mode}")
    return ("#!/bin/bash\n" + body.lstrip()).encode()


def qemu_command(args: argparse.Namespace, test_overlay: pathlib.Path, root_overlay: pathlib.Path) -> list[str]:
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
    ]
    if args.arch == "riscv64":
        command[3:3] = ["-cpu", "max,v=true,vlen=128"]
        command.extend(
            [
                "-drive",
                f"file={test_overlay},if=none,format=qcow2,id=x0",
                "-device",
                "virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0",
                "-drive",
                f"file={root_overlay},if=none,format=qcow2,id=x1",
                "-device",
                "virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1",
                "-netdev",
                "user,id=net0",
                "-device",
                "virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2",
            ]
        )
    else:
        command.extend(
            [
                "-drive",
                f"file={test_overlay},if=none,format=qcow2,id=x0",
                "-device",
                "virtio-blk-pci,drive=x0",
                "-drive",
                f"file={root_overlay},if=none,format=qcow2,id=x1",
                "-device",
                "virtio-blk-pci,drive=x1",
                "-netdev",
                "user,id=net0",
                "-device",
                "virtio-net-pci,netdev=net0",
            ]
        )
    return command


def run_judges(args: argparse.Namespace, serial_log: pathlib.Path, run_dir: pathlib.Path) -> None:
    if args.testsuite is None or not args.testsuite.is_dir():
        return
    if args.mode in {"cagent", "all"}:
        judge = args.testsuite / "judge/judge_cagent-glibc.py"
        if judge.is_file():
            with serial_log.open("rb") as source, (run_dir / "cagent-judge.json").open("wb") as output:
                subprocess.run([sys.executable, str(judge)], stdin=source, stdout=output, check=False)
    if args.mode in {"q21", "buildstorm", "all"}:
        judge = args.testsuite / "judge/judge_buildstorm-glibc.py"
        if judge.is_file():
            with serial_log.open("rb") as source, (run_dir / "buildstorm-judge.json").open("wb") as output:
                subprocess.run([sys.executable, str(judge)], stdin=source, stdout=output, check=False)


def required_success_markers(mode: str) -> tuple[bytes, ...]:
    markers: list[bytes] = []
    if mode in {"cagent", "all"}:
        markers.extend(
            f"testcase cagent {name} pass ".encode() for name in CAGENT_CASES
        )
    if mode in {"q21", "buildstorm", "all"}:
        markers.extend((b"BUILDSTORM_TOOLCHAIN ok", b"BUILDSTORM_MINIBUILD ok"))
    if mode in {"buildstorm", "all"}:
        markers.append(b"BUILDSTORM_COMPILE mode=multi ok=true")
    return tuple(markers)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"), required=True)
    parser.add_argument("--mode", choices=("cagent", "q21", "buildstorm", "all"), default="all")
    parser.add_argument("--test-image", required=True, type=pathlib.Path)
    parser.add_argument("--root-image", type=pathlib.Path)
    parser.add_argument("--kernel", type=pathlib.Path)
    parser.add_argument("--qemu")
    parser.add_argument("--smp", type=int)
    parser.add_argument("--memory", default="8G")
    parser.add_argument("--timeout", type=float)
    parser.add_argument("--artifact-root", type=pathlib.Path, default=pathlib.Path("artifacts/final2026"))
    parser.add_argument("--testsuite", type=pathlib.Path, default=pathlib.Path("../testsuits-for-oskernel"))
    args = parser.parse_args()

    if args.smp is None:
        args.smp = 8 if args.arch == "riscv64" else 12
    platform_limit = 8 if args.arch == "riscv64" else 12
    if not 1 <= args.smp <= platform_limit:
        parser.error(f"--smp must be between 1 and {platform_limit} for {args.arch}")
    if args.kernel is None:
        args.kernel = pathlib.Path("kernel-rv" if args.arch == "riscv64" else "kernel-la")
    if args.root_image is None:
        args.root_image = pathlib.Path("disk.img" if args.arch == "riscv64" else "disk-la.img")
    if args.qemu is None:
        args.qemu = "qemu-system-riscv64" if args.arch == "riscv64" else "qemu-system-loongarch64"
    if args.timeout is None:
        args.timeout = 1800 if args.mode == "q21" else 21600 if args.mode in {"buildstorm", "all"} else 1200

    for required in (args.test_image, args.root_image, args.kernel):
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

    print(f"hashing immutable inputs for {run_dir}", flush=True)
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
        "kernel": str(args.kernel.resolve()),
        "kernel_sha256": sha256_file(args.kernel),
        "test_image": str(args.test_image.resolve()),
        "test_image_sha256": sha256_file(args.test_image),
        "root_image": str(args.root_image.resolve()),
        "root_image_sha256": sha256_file(args.root_image),
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
    try:
        read_until(master, transcript, b">> ", 90)
        send_line_wait_prompt(master, transcript, b"/busybox mount -t ext4 vda /mnt", timeout=60)
        driver = base64.b64encode(guest_driver(args.mode))
        send_line_wait_prompt(
            master,
            transcript,
            b"/busybox sh -c 'rm -f /mnt/tmp/final2026-run.b64 /mnt/tmp/final2026-run.sh'",
        )
        for offset in range(0, len(driver), 192):
            chunk = driver[offset : offset + 192]
            send_line_wait_prompt(
                master,
                transcript,
                b"/busybox sh -c 'printf %s "
                + chunk
                + b" >> /mnt/tmp/final2026-run.b64'",
            )
        send_line_wait_prompt(
            master,
            transcript,
            b"/busybox sh -c '/busybox base64 -d /mnt/tmp/final2026-run.b64 > /mnt/tmp/final2026-run.sh'",
        )
        send_line_wait_prompt(
            master,
            transcript,
            b"/busybox sh -c 'test -x /mnt/bin/bash -a -s /mnt/tmp/final2026-run.sh && echo FINAL2026_PREFLIGHT_OK'",
        )
        if b"FINAL2026_PREFLIGHT_OK" not in transcript:
            raise RuntimeError("official image preflight failed")
        start = len(transcript)
        send_slow(
            master,
            transcript,
            b"/busybox chroot /mnt /bin/bash /tmp/final2026-run.sh\n",
        )
        read_until(master, transcript, b"FINAL2026_DRIVER_DONE rc=", args.timeout, start=start)
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

    run_judges(args, serial_log, run_dir)
    if not completed or b"FINAL2026_DRIVER_DONE rc=0" not in transcript:
        print(f"final 2026 workload failed; evidence: {run_dir}", file=sys.stderr)
        return 1
    if not overlays_ok:
        print(f"one or more qcow2 overlays failed qemu-img check: {run_dir}", file=sys.stderr)
        return 1
    missing_markers = [
        marker for marker in required_success_markers(args.mode) if marker not in transcript
    ]
    if missing_markers:
        print(
            f"official success markers missing: {missing_markers!r}; evidence: {run_dir}",
            file=sys.stderr,
        )
        return 1
    expected_mask = f"[smp] online mask {hex((1 << args.smp) - 1)}".encode()
    if args.smp > 1 and expected_mask not in transcript:
        print(f"missing online CPU marker {expected_mask!r}; evidence: {run_dir}", file=sys.stderr)
        return 1
    print(f"final 2026 workload passed; evidence: {run_dir}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
