#!/usr/bin/env python3
"""Clone the official xv6 repository through VirtIO networking in a fresh overlay."""

from __future__ import annotations

import argparse
import os
import pathlib
import pty
import select
import subprocess
import sys
import tempfile
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


def drain(master: int, transcript: bytearray, duration: float) -> None:
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


def send_slow(master: int, transcript: bytearray, data: bytes) -> None:
    for byte in data:
        os.write(master, bytes([byte]))
        drain(master, transcript, 0.002)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--base-image", required=True, type=pathlib.Path)
    parser.add_argument("--root-disk", default="disk.img", type=pathlib.Path)
    parser.add_argument(
        "--kernel",
        default="target/riscv64gc-unknown-none-elf/release/os.bin",
        type=pathlib.Path,
    )
    parser.add_argument("--proxy", default="http://10.0.2.2:27898")
    parser.add_argument("--log", type=pathlib.Path)
    parser.add_argument("--pcap", type=pathlib.Path)
    parser.add_argument("--timeout", type=float, default=600.0)
    args = parser.parse_args()
    for required in (args.base_image, args.root_disk, args.kernel):
        if not required.is_file():
            parser.error(f"missing required file: {required}")

    transcript = bytearray()
    with tempfile.TemporaryDirectory(prefix="onsite25-git-clone-") as temp_dir:
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
            "-netdev", "user,id=net0",
            "-device", "virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2",
        ]
        if args.pcap:
            args.pcap.parent.mkdir(parents=True, exist_ok=True)
            command.extend(
                [
                    "-object",
                    f"filter-dump,id=onsite25dump,netdev=net0,file={args.pcap.resolve()}",
                ]
            )
        process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        try:
            read_until(master, transcript, b">> ", 35)
            proxy = args.proxy.encode("ascii")
            commands = (
                b"/busybox mount -t ext4 vda /mnt\n"
                b"/busybox chroot /mnt /bin/sh\n"
                b"export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n"
                b"printf 'nameserver 10.0.2.3\\n' > /etc/resolv.conf\n"
                b"export http_proxy=" + proxy + b"\n"
                b"export https_proxy=$http_proxy\n"
                b"export GIT_PAGER=cat\n"
                b"rm -rf /tmp/xv6-riscv\n"
                b"git clone https://github.com/oscomp/xv6-riscv.git /tmp/xv6-riscv\n"
                b"clone_rc=$?\n"
                b"echo GIT_CLONE_RC=$clone_rc\n"
                b"if test $clone_rc -eq 0 && test -f /tmp/xv6-riscv/Makefile && "
                b"test -z \"$(git -C /tmp/xv6-riscv status --porcelain)\"; then "
                b"git -C /tmp/xv6-riscv rev-parse HEAD; echo GIT_CLONE_TREE_OK; "
                b"else echo GIT_CLONE_TREE_FAILED; fi\n"
                b"echo GIT_CLONE_BATCH_DONE\n"
            )
            send_slow(master, transcript, commands)
            read_until(master, transcript, b"GIT_CLONE_BATCH_DONE", args.timeout)
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
            drain(master, transcript, 0.05)
            os.close(master)
            if args.log:
                args.log.parent.mkdir(parents=True, exist_ok=True)
                args.log.write_bytes(transcript)
        subprocess.run(["qemu-img", "check", str(overlay)], check=True)

    required = (b"GIT_CLONE_RC=0", b"GIT_CLONE_TREE_OK")
    missing = [marker for marker in required if marker not in transcript]
    if missing:
        print(f"missing Git clone markers: {missing!r}", file=sys.stderr)
        return 1
    print("onsite 2025 full Git clone passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
