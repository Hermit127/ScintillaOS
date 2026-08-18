#!/usr/bin/env python3

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
from datetime import datetime, timezone


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(4 * 1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def run(command: list[str], **kwargs) -> subprocess.CompletedProcess:
    return subprocess.run(command, check=True, text=True, **kwargs)


def debugfs(image: Path, command: str) -> None:
    result = run(
        ["debugfs", "-w", "-R", command, str(image)],
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    rejected = ("Filesystem not open", "Command not found", "File not found")
    if any(marker in result.stdout for marker in rejected):
        raise RuntimeError(f"debugfs rejected {command!r}:\n{result.stdout}")


def repair_ext4(image: Path) -> str:
    result = subprocess.run(
        ["e2fsck", "-fy", str(image)],
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    if result.returncode not in (0, 1):
        raise RuntimeError(f"e2fsck failed with status {result.returncode}:\n{result.stdout}")
    return "repaired" if result.returncode == 1 else "clean"


def main() -> int:
    repo = Path(__file__).resolve().parent.parent
    parser = argparse.ArgumentParser(description="Build the StarryOS onsite-2026 prediction Ext4 disk")
    parser.add_argument("--base-image", type=Path, default=repo.parent / "alpine-linux-riscv64-ext4fs.img")
    parser.add_argument("--output", type=Path, default=repo / "artifacts/onsite26-predict/starryos-onsite26-riscv64.img")
    args = parser.parse_args()

    base = args.base_image.resolve()
    output = args.output.resolve()
    payload = repo / "tests/onsite26_predict"
    required_tools = ("debugfs", "e2fsck")
    for tool in required_tools:
        if shutil.which(tool) is None:
            raise SystemExit(f"missing required host tool: {tool}")
    if not base.is_file():
        raise SystemExit(f"missing base image: {base}")
    if output == base:
        raise SystemExit("refusing to overwrite the base image")

    files = ["README.md", "run.sh", "kernel_probe.c", "rust_probe.rs"]
    for name in files:
        if not (payload / name).is_file():
            raise SystemExit(f"missing payload: {payload / name}")

    output.parent.mkdir(parents=True, exist_ok=True)
    temporary = Path(tempfile.mkstemp(prefix=f".{output.name}.", dir=output.parent)[1])
    try:
        shutil.copyfile(base, temporary)
        preinject_fsck = repair_ext4(temporary)
        debugfs(temporary, "mkdir /starry26")
        for name in files:
            debugfs(temporary, f"write {payload / name} /starry26/{name}")
        debugfs(temporary, "set_inode_field /starry26/run.sh mode 0100755")
        postinject_fsck = repair_ext4(temporary)
        os.replace(temporary, output)
    finally:
        if temporary.exists():
            temporary.unlink()

    readonly_check = run(["e2fsck", "-fn", str(output)], stdout=subprocess.PIPE, stderr=subprocess.STDOUT).stdout
    manifest = {
        "format": "starryos-onsite26-predict-v1",
        "generated_at": datetime.now(timezone.utc).isoformat(),
        "architecture": "riscv64",
        "filesystem": "ext4",
        "base_image": str(base),
        "base_sha256": sha256(base),
        "output_image": str(output),
        "output_size": output.stat().st_size,
        "output_sha256": sha256(output),
        "payload": {name: sha256(payload / name) for name in files},
        "preinject_fsck": preinject_fsck,
        "postinject_fsck": postinject_fsck,
        "e2fsck_readonly_ok": "FILE SYSTEM WAS MODIFIED" not in readonly_check and "UNEXPECTED INCONSISTENCY" not in readonly_check,
    }
    manifest_path = output.with_suffix(output.suffix + ".manifest.json")
    manifest_path.write_text(json.dumps(manifest, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    print(f"image: {output}")
    print(f"sha256: {manifest['output_sha256']}")
    print(f"manifest: {manifest_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
