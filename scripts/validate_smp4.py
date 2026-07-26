#!/usr/bin/env python3
"""Cold-boot four-hart ScintillaOS validation without changing guest tests.

The host owns all timeouts.  No timeout wrapper, CPU affinity, test argument,
or replacement script is injected into the guest.
"""

from __future__ import annotations

import argparse
import dataclasses
import hashlib
import json
import os
import selectors
import signal
import subprocess
import sys
import time
from pathlib import Path


FORBIDDEN = (
    "Panicked at",
    "[sched-stall]",
    "lock-order inversion",
    "same-class lock inversion",
    "recursive acquisition",
    "Segmentation fault",
    "#### SKIP",
    "sh: can't open './",
)


@dataclasses.dataclass(frozen=True)
class GuestCommand:
    command: str
    marker: str = ""
    prompt: str = ">> "
    timeout: float = 180.0


DIRECTED = (
    GuestCommand("cpuinfotest", "cpuinfotest passed: online mask 0xf"),
    GuestCommand("smpstress", "smpstress passed: online=0xf, observed=0xf"),
    GuestCommand("affinitytest", "affinitytest passed: online=0xf"),
    GuestCommand("clonesmptest", "clonesmptest passed:"),
    GuestCommand("futextest", "futextest passed!"),
    GuestCommand("mprotecttest", "mprotecttest passed!"),
    GuestCommand("shmtest", "shmtest passed!"),
    GuestCommand("run_pipe_test", "Iter 4 OK."),
    GuestCommand("sig_simple", "signal_simple: Done"),
    GuestCommand("sig_simple2", "signal_simple2: parent Done"),
)


def official_commands(suite: str) -> tuple[GuestCommand, ...]:
    variants = ("musl", "glibc") if suite == "matrix" else (suite.rsplit("-", 1)[-1],)
    groups = ("busybox", "lmbench", "libctest") if suite == "matrix" else (suite.split("-", 1)[0],)
    commands: list[GuestCommand] = [
        GuestCommand("/busybox mount -t ext4 /dev/vda /mnt"),
    ]
    for variant in variants:
        for group in groups:
            shell_steps: list[str] = []
            if group == "libctest":
                # The immutable libc-test ELFs use absolute PT_INTERP paths.
                # Each ABI's official disk directory is therefore the Linux
                # process root while its unchanged wrapper runs.  This keeps
                # `/lib/ld-*.so*` and the matching shared libraries isolated
                # from the kernel regression root disk.
                commands.append(
                    GuestCommand(
                        f"/busybox chroot /mnt/{variant} /busybox sh -c "
                        f"'exec ./busybox sh ./{group}_testcode.sh'",
                        f"#### OS COMP TEST GROUP END {group} ####",
                        ">> ",
                        1800.0,
                    )
                )
                continue
            if group == "lmbench":
                # Run setup utilities as direct children of the Rust shell.
                # The final BusyBox shell then performs builtins only before
                # exec'ing the unchanged official script.
                commands.extend(
                    (
                        GuestCommand("/busybox mkdir -p /code/lmbench_src/bin/build"),
                        GuestCommand(
                            f"/busybox cp /mnt/{variant}/lmbench_all "
                            "/code/lmbench_src/bin/build/lmbench_all"
                        ),
                        GuestCommand("/busybox mkdir -p /tmp/bb"),
                        GuestCommand(
                            f"/busybox cp /mnt/{variant}/busybox /tmp/bb/cp"
                        ),
                    )
                )
                shell_steps.extend(
                    [
                        "export PATH=/tmp/bb:$PATH",
                        "export ENOUGH=1000 TIMING_O=0.0 LOOP_O=0.0",
                    ]
                )
            shell_steps.extend(
                [
                    f"cd /mnt/{variant}",
                    f"exec ./busybox sh ./{group}_testcode.sh",
                ]
            )
            compound = "; ".join(shell_steps)
            commands.append(
                GuestCommand(
                    f"/busybox sh -c '{compound}'",
                    f"#### OS COMP TEST GROUP END {group} ####",
                    ">> ",
                    1800.0 if group in {"lmbench", "libctest"} else 600.0,
                )
            )
    return tuple(commands)


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(4 * 1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def qemu_version(qemu: str) -> str:
    result = subprocess.run(
        [qemu, "--version"], check=True, text=True, stdout=subprocess.PIPE
    )
    return result.stdout.splitlines()[0]


def serial_metrics(serial: str) -> dict[str, int]:
    return {
        "group_end": serial.count("#### OS COMP TEST GROUP END"),
        "case_start": serial.count("========== START "),
        "case_end": serial.count("========== END "),
        "pass": serial.count("Pass!"),
        "fail": sum(1 for line in serial.splitlines() if line.startswith("FAIL ")),
        "skip": serial.count("#### SKIP"),
    }


def create_overlay(base: Path, overlay: Path) -> None:
    subprocess.run(
        [
            "qemu-img",
            "create",
            "-q",
            "-f",
            "qcow2",
            "-F",
            "raw",
            "-b",
            str(base.resolve()),
            str(overlay),
        ],
        check=True,
    )


class QemuRun:
    def __init__(
        self,
        argv: list[str],
        log_path: Path,
        stall_timeout: float,
    ) -> None:
        self.argv = argv
        self.log_path = log_path
        self.stall_timeout = stall_timeout
        self.log = log_path.open("wb")
        self.proc = subprocess.Popen(
            argv,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            start_new_session=True,
            bufsize=0,
        )
        assert self.proc.stdout is not None
        self.selector = selectors.DefaultSelector()
        self.selector.register(self.proc.stdout, selectors.EVENT_READ)
        self.all_output = bytearray()
        self.scan_offset = 0
        self.last_output = time.monotonic()

    def close(self) -> None:
        if self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGTERM)
            try:
                self.proc.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(self.proc.pid, signal.SIGKILL)
                self.proc.wait(timeout=3)
        self.selector.close()
        self.log.close()

    def monitor_dump(self, destination: Path) -> None:
        start = len(self.all_output)
        try:
            assert self.proc.stdin is not None
            # -nographic provides a stdio multiplexer.  Ctrl-A c switches
            # from the guest serial port to the host QEMU monitor.
            self.proc.stdin.write(b"\x01c")
            self.proc.stdin.flush()
            time.sleep(0.1)
            commands = [b"info cpus\n"]
            for cpu in range(4):
                commands.extend(
                    (
                        f"cpu {cpu}\n".encode(),
                        b"info registers\n",
                    )
                )
            commands.append(b"info mtree\n")
            self.proc.stdin.write(b"".join(commands))
            self.proc.stdin.flush()
            deadline = time.monotonic() + 2.0
            while time.monotonic() < deadline and self.proc.poll() is None:
                self._read_once(deadline)
            self.proc.stdin.write(b"\x01c")
            self.proc.stdin.flush()
            destination.write_bytes(bytes(self.all_output[start:]))
        except Exception as error:
            destination.write_text(f"monitor unavailable: {error}\n", encoding="utf-8")

    def drain_after_failure(self, seconds: float = 0.75) -> None:
        """Archive the rest of a panic line without applying marker checks."""
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline and self.proc.poll() is None:
            events = self.selector.select(0.05)
            if not events:
                continue
            data = os.read(self.proc.stdout.fileno(), 65536)
            if not data:
                break
            self.all_output.extend(data)
            self.log.write(data)
            self.log.flush()

    def _read_once(self, deadline: float) -> None:
        remaining = max(0.0, min(0.5, deadline - time.monotonic()))
        events = self.selector.select(remaining)
        if not events:
            if self.proc.poll() is not None:
                raise RuntimeError(f"QEMU exited with status {self.proc.returncode}")
            if time.monotonic() - self.last_output >= self.stall_timeout:
                raise TimeoutError(
                    f"no serial output for {self.stall_timeout:.0f} seconds"
                )
            return
        data = os.read(self.proc.stdout.fileno(), 65536)
        if not data:
            raise RuntimeError(f"QEMU closed serial output ({self.proc.poll()})")
        self.last_output = time.monotonic()
        self.all_output.extend(data)
        self.log.write(data)
        self.log.flush()
        text = bytes(self.all_output[self.scan_offset :]).decode("utf-8", "replace")
        for forbidden in FORBIDDEN:
            if forbidden in text:
                raise RuntimeError(f"forbidden serial marker: {forbidden}")
        self.scan_offset = max(0, len(self.all_output) - 256)

    def wait_for(self, marker: str, timeout: float, start: int | None = None) -> None:
        needle = marker.encode()
        if start is None:
            start = len(self.all_output)
        deadline = time.monotonic() + timeout
        while needle not in self.all_output[start:]:
            if time.monotonic() >= deadline:
                raise TimeoutError(f"serial marker not seen: {marker!r}")
            self._read_once(deadline)

    def command(self, spec: GuestCommand) -> None:
        assert self.proc.stdin is not None
        start = len(self.all_output)
        self.proc.stdin.write(spec.command.encode() + b"\n")
        self.proc.stdin.flush()
        # The Rust shell prints the prompt before reading the command, so the
        # serial transcript contains ``>> command``.  Searching for ``>> ``
        # from `start` would mistake that same prompt for the *next* prompt and
        # enqueue commands while the previous child is still running.
        self.wait_for(spec.command, spec.timeout, start)
        echo_at = self.all_output.find(spec.command.encode(), start)
        assert echo_at >= 0
        completion_start = echo_at + len(spec.command)
        if spec.marker:
            self.wait_for(spec.marker, spec.timeout, completion_start)
            marker_at = self.all_output.find(spec.marker.encode(), completion_start)
            assert marker_at >= 0
            completion_start = marker_at + len(spec.marker)
        self.wait_for(spec.prompt, spec.timeout, completion_start)


def parse_command(value: str) -> GuestCommand:
    fields = value.split(":::", 2)
    fields += [""] * (3 - len(fields))
    return GuestCommand(fields[0], fields[1], fields[2] or ">> ")


def write_manifest(
    path: Path,
    args: argparse.Namespace,
    inputs: dict[str, dict[str, str]],
    results: list[dict[str, object]],
    qemu: str,
) -> None:
    passed = sum(bool(result["passed"]) for result in results)
    lines = [
        "# SMP4 validation manifest",
        "",
        f"- Generated: {time.strftime('%Y-%m-%d %H:%M:%S %z')}",
        f"- QEMU: `{qemu}`",
        f"- Topology: `-smp 4 -m 256M`",
        f"- Suite: `{args.suite}`",
        f"- Result: `{passed}/{len(results)} passed`",
        "",
        "## Immutable input hashes",
        "",
    ]
    for name, item in inputs.items():
        lines.append(f"- {name}: `{item['sha256']}` — `{item['path']}`")
    lines.extend(["", "## Cold boot rounds", ""])
    for result in results:
        metrics = result.get("metrics", {})
        metric_text = (
            f"; metrics `{json.dumps(metrics, sort_keys=True)}`" if metrics else ""
        )
        lines.append(
            f"- round {result['round']}: {'PASS' if result['passed'] else 'FAIL'}; "
            f"log `{result['log']}`; detail `{result['detail']}`{metric_text}"
        )
    path.write_text("\n".join(lines) + "\n", encoding="utf-8")


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--kernel", type=Path, default=Path("kernel-rv"))
    parser.add_argument(
        "--root-disk",
        type=Path,
        default=Path("target/riscv64gc-unknown-none-elf/release/fs.img"),
    )
    parser.add_argument("--test-disk", type=Path, required=True)
    parser.add_argument(
        "--suite",
        choices=(
            "directed",
            "busybox-musl",
            "busybox-glibc",
            "lmbench-musl",
            "lmbench-glibc",
            "libctest-musl",
            "libctest-glibc",
            "matrix",
            "custom",
        ),
        default="directed",
    )
    parser.add_argument(
        "--command",
        action="append",
        default=[],
        help="custom command as COMMAND[:::REQUIRED_MARKER[:::PROMPT]]",
    )
    parser.add_argument("--rounds", type=int, default=3)
    parser.add_argument("--stall-timeout", type=float, default=120.0)
    parser.add_argument(
        "--output-dir", type=Path, default=Path("artifacts/smp4-validation")
    )
    parser.add_argument("--qemu", default="qemu-system-riscv64")
    parser.add_argument(
        "--cpu",
        default="max,v=true,vlen=128",
        help="QEMU RISC-V CPU; official glibc ELFs require V 1.0/Zvl128b",
    )
    parser.add_argument("--build", action="store_true")
    parser.add_argument("--diagnostics", action="store_true")
    args = parser.parse_args()

    for path in (args.kernel, args.root_disk, args.test_disk):
        if not path.is_file():
            parser.error(f"missing input: {path}")
    if args.rounds < 1:
        parser.error("--rounds must be positive")
    if args.suite == "custom" and not args.command:
        parser.error("--suite custom requires --command")

    if args.build:
        subprocess.run(
            [
                "make",
                "kernel-rv",
                "ARCH=riscv64",
                f"SMP_DIAGNOSTICS={1 if args.diagnostics else 0}",
            ],
            check=True,
        )

    commands = (
        tuple(parse_command(value) for value in args.command)
        if args.command
        else DIRECTED
        if args.suite == "directed"
        else official_commands(args.suite)
    )
    args.output_dir.mkdir(parents=True, exist_ok=True)
    inputs = {
        "kernel": {"path": str(args.kernel.resolve()), "sha256": sha256(args.kernel)},
        "root disk": {
            "path": str(args.root_disk.resolve()),
            "sha256": sha256(args.root_disk),
        },
        "test disk": {
            "path": str(args.test_disk.resolve()),
            "sha256": sha256(args.test_disk),
        },
    }
    (args.output_dir / "inputs.json").write_text(
        json.dumps(inputs, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )

    qemu = qemu_version(args.qemu)
    results: list[dict[str, object]] = []
    for round_number in range(1, args.rounds + 1):
        round_dir = args.output_dir / f"round-{round_number:02d}"
        if round_dir.exists():
            parser.error(
                f"refusing to overwrite prior evidence: {round_dir}; "
                "choose a new --output-dir"
            )
        round_dir.mkdir()
        test_overlay = round_dir / "test.qcow2"
        root_overlay = round_dir / "root.qcow2"
        create_overlay(args.test_disk, test_overlay)
        create_overlay(args.root_disk, root_overlay)
        log = round_dir / "serial.log"
        argv = [
            args.qemu,
            "-machine",
            "virt",
            "-cpu",
            args.cpu,
            "-kernel",
            str(args.kernel.resolve()),
            "-nographic",
            "-m",
            "256M",
            "-smp",
            "4",
            "-drive",
            f"file={test_overlay},if=none,format=qcow2,id=x0",
            "-device",
            "virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0",
            "-drive",
            f"file={root_overlay},if=none,format=qcow2,id=x1",
            "-device",
            "virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1",
        ]
        (round_dir / "qemu-command.txt").write_text(
            " ".join(argv) + "\n", encoding="utf-8"
        )
        run = QemuRun(argv, log, args.stall_timeout)
        passed = False
        detail = "completed"
        try:
            run.wait_for("[smp] online mask 0xf", 90, 0)
            run.wait_for(">> ", 90, 0)
            for command in commands:
                run.command(command)
            serial = bytes(run.all_output).decode("utf-8", "replace")
            if args.suite == "directed" and "observed=0xf" not in serial:
                raise RuntimeError("four-hart user execution evidence missing")
            if args.suite == "libctest-musl" and any(
                line.startswith("FAIL ") for line in serial.splitlines()
            ):
                raise RuntimeError("official musl libc-test reported FAIL")
            passed = True
        except Exception as error:  # preserve all evidence before continuing
            detail = str(error)
            run.drain_after_failure()
            run.monitor_dump(round_dir / "monitor-dump.txt")
        finally:
            run.close()
        results.append(
            {
                "round": round_number,
                "passed": passed,
                "detail": detail,
                "log": str(log.relative_to(args.output_dir)),
                "metrics": serial_metrics(
                    bytes(run.all_output).decode("utf-8", "replace")
                ),
            }
        )
        if not passed:
            break

    post_hashes = {
        "root disk": sha256(args.root_disk),
        "test disk": sha256(args.test_disk),
    }
    (args.output_dir / "post-run-hashes.json").write_text(
        json.dumps(post_hashes, indent=2) + "\n", encoding="utf-8"
    )
    for name in ("root disk", "test disk"):
        if post_hashes[name] != inputs[name]["sha256"]:
            results.append(
                {
                    "round": "hash",
                    "passed": False,
                    "detail": f"{name} changed despite qcow2 overlays",
                    "log": "-",
                }
            )
    (args.output_dir / "results.json").write_text(
        json.dumps(results, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )
    write_manifest(args.output_dir / "MANIFEST.md", args, inputs, results, qemu)
    return 0 if len(results) == args.rounds and all(r["passed"] for r in results) else 1


if __name__ == "__main__":
    sys.exit(main())
