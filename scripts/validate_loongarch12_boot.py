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
from dataclasses import dataclass


FORBIDDEN = (
    b"Panicked at",
    b"[kernel] panic:",
    b"[sched-stall]",
    b"lock-order inversion",
    b"#### SKIP",
    b"sh: can't open './",
)


@dataclass(frozen=True)
class GuestCommand:
    command: str
    marker: str = ""
    timeout: float = 180.0


def directed_commands(smp: int) -> tuple[GuestCommand, ...]:
    """Return the architecture-neutral SMP/kernel regression sequence."""
    mask = hex((1 << smp) - 1)
    return (
        GuestCommand("cpuinfotest", f"cpuinfotest passed: online mask {mask}"),
        GuestCommand("smpstress", f"smpstress passed: online={mask}, observed={mask}"),
        GuestCommand("affinitytest", f"affinitytest passed: online={mask}"),
        GuestCommand("clonesmptest", "clonesmptest passed:"),
        GuestCommand("futextest", "futextest passed!"),
        GuestCommand("mprotecttest", "mprotecttest passed!"),
        GuestCommand("shmtest", "shmtest passed!"),
        GuestCommand("run_pipe_test", "Iter 4 OK."),
        GuestCommand("sig_simple", "signal_simple: Done"),
        GuestCommand("sig_simple2", "signal_simple2: parent Done"),
    )


def official_commands() -> tuple[GuestCommand, ...]:
    """Return the unchanged 2025 preliminary BusyBox/lmbench/libc-test matrix."""
    commands: list[GuestCommand] = [
        GuestCommand(
            "/busybox sh -c '/busybox mount -t ext4 /dev/vda /mnt; "
            "printf \"PRE2025_MOUNT_RC=%s\\n\" \"$?\"'",
            "PRE2025_MOUNT_RC=0",
        ),
    ]
    for variant in ("musl", "glibc"):
        for group in ("busybox", "lmbench", "libctest"):
            # The official 2025 score matrix has libc-test only for musl;
            # glibc libc-test is therefore outside this matrix.
            if variant == "glibc" and group == "libctest":
                continue
            if group == "libctest":
                commands.extend(
                    (
                        GuestCommand(f"/busybox mkdir /mnt/{variant}/tmp"),
                        GuestCommand(
                            f"/busybox chroot /mnt/{variant} /busybox sh -c "
                            f"'exec ./busybox sh ./{group}_testcode.sh'",
                            f"#### OS COMP TEST GROUP END {group}-{variant} ####",
                            1800.0,
                        ),
                    )
                )
                continue
            setup: list[GuestCommand] = []
            shell_steps: list[str] = []
            if group == "lmbench":
                setup = [
                    GuestCommand("/busybox mkdir /code"),
                    GuestCommand("/busybox mkdir /code/lmbench_src"),
                    GuestCommand("/busybox mkdir /code/lmbench_src/bin"),
                    GuestCommand("/busybox mkdir /code/lmbench_src/bin/build"),
                    GuestCommand("/busybox mkdir /var"),
                    GuestCommand("/busybox mkdir /var/tmp"),
                    GuestCommand(
                        f"/busybox cp /mnt/{variant}/lmbench_all "
                        "/code/lmbench_src/bin/build/lmbench_all",
                    ),
                    GuestCommand("/busybox mkdir /tmp/bb"),
                    GuestCommand(
                        f"/busybox cp /mnt/{variant}/busybox /tmp/bb/cp"
                    ),
                ]
                shell_steps.extend(
                    [
                        "export PATH=/tmp/bb:$PATH",
                        "export ENOUGH=1000 TIMING_O=0.0 LOOP_O=0.0",
                    ]
                )
            commands.extend(setup)
            shell_steps.extend(
                [
                    f"cd /mnt/{variant}",
                    f"exec ./busybox sh ./{group}_testcode.sh",
                ]
            )
            commands.append(
                GuestCommand(
                    f"/busybox sh -c '{'; '.join(shell_steps)}'",
                    f"#### OS COMP TEST GROUP END {group}-{variant} ####",
                    1800.0 if group == "lmbench" else 600.0,
                )
            )
    return tuple(commands)


def signal_commands() -> tuple[GuestCommand, ...]:
    """Return focused libc regressions for signal delivery and TLS teardown."""
    return (
        GuestCommand(
            "/busybox sh -c '/busybox mount -t ext4 /dev/vda /mnt; "
            "printf \"PRE2025_MOUNT_RC=%s\\n\" \"$?\"'",
            "PRE2025_MOUNT_RC=0",
        ),
        GuestCommand("/busybox mkdir /mnt/musl/tmp"),
        GuestCommand(
            "/busybox chroot /mnt/musl /busybox sh -c "
            "'exec ./runtest.exe -w entry-static.exe pthread_cancel_sem_wait'",
            "========== END entry-static.exe pthread_cancel_sem_wait ==========",
        ),
        GuestCommand(
            "/busybox chroot /mnt/musl /busybox sh -c "
            "'exec ./runtest.exe -w entry-dynamic.exe pthread_cancel_points'",
            "========== END entry-dynamic.exe pthread_cancel_points ==========",
        ),
        GuestCommand(
            "/busybox chroot /mnt/musl /busybox sh -c "
            "'exec ./runtest.exe -w entry-dynamic.exe tls_get_new_dtv'",
            "========== END entry-dynamic.exe tls_get_new_dtv ==========",
        ),
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


def validate_scored_matrix(transcript: bytes) -> None:
    """Validate the result-bearing rows of the official 2025 score matrix."""
    serial = transcript.decode("utf-8", "replace")
    if "waitpid: Operation not permitted" in serial:
        raise RuntimeError("official matrix observed libc waitpid EPERM")
    expected_groups = 5
    group_ends = serial.count("#### OS COMP TEST GROUP END")
    if group_ends != expected_groups:
        raise RuntimeError(
            f"official matrix completed {group_ends}/{expected_groups} scored groups"
        )
    start_marker = "#### OS COMP TEST GROUP START libctest-musl ####"
    end_marker = "#### OS COMP TEST GROUP END libctest-musl ####"
    start = serial.find(start_marker)
    end = serial.find(end_marker, start + len(start_marker)) if start >= 0 else -1
    if start < 0 or end < 0:
        raise RuntimeError("official musl libc-test result section is missing")
    libc_section = serial[start:end]
    failed = [
        line.rstrip("\r")
        for line in libc_section.splitlines()
        if line.rstrip("\r").startswith("FAIL ")
    ]
    if failed:
        raise RuntimeError(f"official musl libc-test reported {len(failed)} FAIL rows")
    passed = sum(line.rstrip("\r") == "Pass!" for line in libc_section.splitlines())
    if passed != 217:
        raise RuntimeError(f"official musl libc-test reported {passed}/217 Pass rows")


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
    parser.add_argument(
        "--test-image",
        type=pathlib.Path,
        help="2025 preliminary Ext4 test image; required by --suite matrix",
    )
    parser.add_argument("--qemu", default="qemu-system-loongarch64")
    parser.add_argument("--smp", type=int, default=12)
    parser.add_argument("--memory", default="2G")
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument(
        "--suite",
        choices=("boot", "directed", "signal", "matrix"),
        default="boot",
        help="boot checks one SMP probe; directed runs local regressions; signal runs focused libc regressions; matrix runs the 2025 preliminary matrix",
    )
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
    if args.suite in {"signal", "matrix"} and args.test_image is None:
        parser.error(f"--suite {args.suite} requires --test-image")
    if args.test_image is not None and not args.test_image.is_file():
        parser.error(f"missing required file: {args.test_image}")

    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    run_dir = args.artifact_root / stamp
    run_dir.mkdir(parents=True, exist_ok=False)
    root_overlay = run_dir / "root-overlay.qcow2"
    test_overlay = run_dir / "test-overlay.qcow2" if args.test_image else None
    serial_log = run_dir / "serial.log"
    create_overlay(args.root_image, root_overlay)
    if test_overlay is not None:
        assert args.test_image is not None
        create_overlay(args.test_image, test_overlay)

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
    if test_overlay is not None:
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
            ]
        )
    else:
        command.extend(
            [
                "-drive",
                f"file={root_overlay},if=none,format=qcow2,id=x0",
                "-device",
                "virtio-blk-pci,drive=x0",
            ]
        )
    command.extend([
        "-netdev",
        "user,id=net0",
        "-device",
        "virtio-net-pci,netdev=net0",
    ])
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
    if args.test_image is not None:
        metadata["test_image"] = str(args.test_image.resolve())
        metadata["test_image_sha256"] = sha256_file(args.test_image)
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
    completed_commands: list[str] = []
    try:
        read_until(process, transcript, f"[smp] online mask {expected_mask}".encode(), args.timeout)
        read_until(process, transcript, b"Rust user shell", args.timeout)
        read_until(process, transcript, b">> ", args.timeout)
        assert process.stdin is not None
        commands = (
            directed_commands(args.smp)
            if args.suite == "directed"
            else signal_commands()
            if args.suite == "signal"
            else official_commands()
            if args.suite == "matrix"
            else directed_commands(args.smp)[:1]
        )
        for guest_command in commands:
            command_start = len(transcript)
            process.stdin.write((guest_command.command + "\n").encode())
            process.stdin.flush()
            if guest_command.marker:
                success_marker = guest_command.marker.encode()
                read_until(
                    process,
                    transcript,
                    success_marker,
                    guest_command.timeout,
                    command_start,
                )
                prompt_start = (
                    transcript.find(success_marker, command_start) + len(success_marker)
                )
            else:
                prompt_start = command_start
            read_until(
                process,
                transcript,
                b">> ",
                guest_command.timeout,
                prompt_start,
            )
            completed_commands.append(guest_command.command)
        if args.suite == "matrix":
            validate_scored_matrix(bytes(transcript))
        if args.suite == "signal":
            serial = transcript.decode("utf-8", "replace")
            failed = [line for line in serial.splitlines() if line.startswith("FAIL ")]
            if failed or sum(line == "Pass!" for line in serial.splitlines()) != 3:
                raise RuntimeError(
                    f"focused signal/TLS regressions reported {len(failed)} FAIL rows"
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

    overlays = [("root", root_overlay)]
    if test_overlay is not None:
        overlays.append(("test", test_overlay))
    for name, overlay in overlays:
        checked = subprocess.run(
            ["qemu-img", "check", str(overlay)],
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            check=False,
        )
        (run_dir / f"{name}-overlay-check.txt").write_bytes(checked.stdout)
        if checked.returncode != 0:
            passed = False
            detail = f"{detail}; {name} qemu-img check failed"
    result = {
        "passed": passed,
        "detail": detail,
        "suite": args.suite,
        "online_mask": expected_mask,
        "completed_commands": completed_commands,
    }
    (run_dir / "result.json").write_text(
        json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    if not passed:
        print(f"LoongArch boot validation failed: {detail}; evidence: {run_dir}", file=sys.stderr)
        return 1
    print(
        f"LoongArch {args.smp}-hart {args.suite} validation passed; "
        f"evidence: {run_dir}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
