#!/usr/bin/env python3
"""Drive a real interactive Vim session over the QEMU serial console.

The QEMU UART can drop bursts injected by non-interactive test runners.  This
driver sends editor keystrokes at human-like intervals and records every byte
printed by the guest.  It never creates or edits the guest test file itself.
"""

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
        try:
            chunk = os.read(master, 65536)
        except OSError as error:
            raise RuntimeError("QEMU serial console closed unexpectedly") from error
        if not chunk:
            raise RuntimeError("QEMU serial console closed unexpectedly")
        transcript.extend(chunk)
        sys.stdout.buffer.write(chunk)
        sys.stdout.buffer.flush()


def drain(master: int, transcript: bytearray, duration: float = 0.05) -> None:
    deadline = time.monotonic() + duration
    while time.monotonic() < deadline:
        ready, _, _ = select.select([master], [], [], 0.01)
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


def send_slow(master: int, transcript: bytearray, data: bytes, delay: float = 0.035) -> None:
    for byte in data:
        os.write(master, bytes([byte]))
        drain(master, transcript, delay)


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
    args = parser.parse_args()

    for required in (args.base_image, args.root_disk, args.kernel):
        if not required.is_file():
            parser.error(f"missing required file: {required}")

    transcript = bytearray()
    with tempfile.TemporaryDirectory(prefix="onsite25-vim-") as temp_dir:
        overlay = pathlib.Path(temp_dir) / "alpine.qcow2"
        subprocess.run(
            [
                "qemu-img",
                "create",
                "-f",
                "qcow2",
                "-F",
                "raw",
                "-b",
                str(args.base_image.resolve()),
                str(overlay),
            ],
            check=True,
        )

        master, slave = pty.openpty()
        command = [
            "qemu-system-riscv64",
            "-machine",
            "virt",
            "-cpu",
            "max",
            "-kernel",
            str(args.kernel),
            "-nographic",
            "-m",
            "1G",
            "-smp",
            "1",
            "-drive",
            f"file={overlay},if=none,format=qcow2,id=x0",
            "-device",
            "virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0",
            "-drive",
            f"file={args.root_disk},if=none,format=raw,id=x1,file.locking=off",
            "-device",
            "virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1",
        ]
        process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        try:
            read_until(master, transcript, b"Rust user shell", 30)
            read_until(master, transcript, b">> ", 5)
            setup = (
                b"/busybox mount -t ext4 vda /mnt\n"
                b"/busybox chroot /mnt /bin/sh\n"
                b"export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n"
                b"export TERM=vt100\n"
                b"stty rows 24 cols 80\n"
            )
            send_slow(master, transcript, setup, 0.012)
            preflight_start = len(transcript)
            preflight = (
                b"git -h >/tmp/git-help 2>&1; git_rc=$?; head -n 3 /tmp/git-help; "
                b"echo GIT_HELP_RC=$git_rc\n"
                b"rm -rf /tmp/onsite-git-local && mkdir /tmp/onsite-git-local\n"
                b"cd /tmp/onsite-git-local && git init\n"
                b"git config user.name 'OS Test'\n"
                b"git config user.email 'os@example.com'\n"
                b"printf '# OS onsite test\\n' > README.md\n"
                b"git add . && git commit -m 'add README.md'\n"
                b"if test -z \"$(git status --porcelain)\"; then git log --oneline -1; "
                b"echo GIT_LOCAL_OK; else echo GIT_LOCAL_FAILED; fi\n"
                b"cd /\n"
                b"vim -h >/tmp/vim-help 2>&1; vim_rc=$?; head -n 3 /tmp/vim-help; "
                b"echo VIM_HELP_RC=$vim_rc\n"
                b"gcc --help >/tmp/gcc-help 2>&1; gcc_rc=$?; head -n 3 /tmp/gcc-help; "
                b"echo GCC_HELP_RC=$gcc_rc\n"
                b"rustc -h >/tmp/rustc-help 2>&1; rustc_rc=$?; head -n 3 /tmp/rustc-help; "
                b"echo RUSTC_HELP_RC=$rustc_rc\n"
                b"printf 'THIS PREEXISTING FILE IS DELIBERATELY LONGER THAN THE VIM OUTPUT.\\n"
                b"SECOND OLD LINE THAT MUST BE TRUNCATED.\\n' > /tmp/hello-vim.c\n"
                b"echo OFFLINE_PREFLIGHT_DONE\n"
            )
            send_slow(master, transcript, preflight, 0.012)
            read_until(
                master,
                transcript,
                b"OFFLINE_PREFLIGHT_DONE",
                90,
                preflight_start,
            )

            vim_command = (
                b"vim -Nu NONE -N -T vt100 --cmd "
                b"'set t_RV= t_u7= t_RF= t_RB=' "
                b"/tmp/hello-vim.c\n"
            )
            send_slow(master, transcript, vim_command, 0.012)
            vim_start = len(transcript)
            send_slow(master, transcript, b"\x0c", 0.15)
            read_until(master, transcript, b'"/tmp/hello-vim.c"', 30, vim_start)
            full_redraw_start = len(transcript)
            send_slow(master, transcript, b"\x0c\x0c", 0.05)
            read_until(master, transcript, b"\x1b[1;1H", 10, full_redraw_start)
            drain(master, transcript, 3.0)
            send_slow(master, transcript, b"ggdG", 0.08)

            # These are genuine interactive Vim keystrokes.  A complete Ex
            # command is used only to enter Insert mode so the serial driver
            # does not leave a lone "i" in Vim's terminal-code collector.
            send_slow(master, transcript, b":startinsert\r", 0.08)
            drain(master, transcript, 3.0)
            source = (
                b"#include <stdio.h>\r"
                b"int main(void) {\r"
                b"    printf(\"Hello, Vim!\\n\");\r"
                b"    return 0;\r"
                b"}"
            )
            send_slow(master, transcript, source)
            drain(master, transcript, 3.0)
            send_slow(master, transcript, b"\x03", 3.0)
            quit_start = len(transcript)
            send_slow(master, transcript, b":wq\r", 0.08)
            read_until(master, transcript, b"/ # ", 30, quit_start)

            verify_start = len(transcript)
            verify = (
                b"printf 'VIM_FILE_BEGIN\\n'\n"
                b"cat /tmp/hello-vim.c\n"
                b"printf 'VIM_FILE_END\\n'\n"
                b"vim_stat_size=$(stat -c %s /tmp/hello-vim.c)\n"
                b"vim_read_size=$(cat /tmp/hello-vim.c | wc -c)\n"
                b"echo VIM_STAT_SIZE=$vim_stat_size VIM_READ_SIZE=$vim_read_size\n"
                b"if test \"$vim_stat_size\" = \"$vim_read_size\"; then "
                b"echo VIM_TRUNCATE_STAT_OK; else echo VIM_TRUNCATE_STAT_FAILED; fi\n"
                b"gcc /tmp/hello-vim.c -o /tmp/hello-vim\n"
                b"/tmp/hello-vim\n"
                b"echo VIM_RC=$?\n"
                b"printf '#include <stdio.h>\\nint main(void){printf(\"Hello, World!\\\\n\");return 0;}\\n' > /tmp/hello.c\n"
                b"gcc /tmp/hello.c -o /tmp/hello-c && /tmp/hello-c\n"
                b"echo GCC_HELLO_RC=$?\n"
                b"printf 'fn main() { println!(\"Hello, World!\"); }\\n' > /tmp/hello.rs\n"
                b"rustc /tmp/hello.rs -o /tmp/hello-rs && /tmp/hello-rs\n"
                b"echo RUST_HELLO_RC=$?\n"
            )
            send_slow(master, transcript, verify, 0.012)
            read_until(master, transcript, b"RUST_HELLO_RC=0", 120, verify_start)
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

    required_markers = (
        b"GIT_HELP_RC=0",
        b"GIT_LOCAL_OK",
        b"VIM_HELP_RC=0",
        b"GCC_HELP_RC=0",
        b"RUSTC_HELP_RC=0",
        b"VIM_FILE_BEGIN",
        b"#include <stdio.h>",
        b'printf("Hello, Vim!\\n");',
        b"VIM_FILE_END",
        b"VIM_TRUNCATE_STAT_OK",
        b"Hello, Vim!",
        b"VIM_RC=0",
        b"GCC_HELLO_RC=0",
        b"RUST_HELLO_RC=0",
    )
    missing = [marker for marker in required_markers if marker not in transcript]
    if missing:
        print(f"missing verification markers: {missing!r}", file=sys.stderr)
        return 1
    if b"shorter than expected" in transcript:
        print("GCC observed stale file size after Vim truncate", file=sys.stderr)
        return 1
    print("interactive Vim verification passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
