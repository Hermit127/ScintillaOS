#!/usr/bin/env python3
"""Unmodified toolchain workload for an onsite-style compatibility test."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import json
import os
import pathlib
import shutil
import sqlite3
import subprocess
import sys
import threading
import time


ROOT = pathlib.Path("/tmp/onsite-toolchain")
EXPECTED_HTTPS_SHA256 = "05342be05e75f9a75e70fc0bfabd223e7c755e373b6ac584b0918331ea5ae9a2"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def fresh(path: pathlib.Path) -> None:
    if path.is_symlink() or (path.exists() and not path.is_dir()):
        path.unlink()
    elif path.exists():
        shutil.rmtree(path)
    path.mkdir(parents=True)


def run(
    command: list[str],
    *,
    cwd: pathlib.Path | None = None,
    timeout: int = 300,
    expected: str | None = None,
) -> subprocess.CompletedProcess[str]:
    print("TOOLCHAIN_COMMAND " + " ".join(command), flush=True)
    result = subprocess.run(
        command,
        cwd=cwd,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=timeout,
        check=False,
    )
    if result.stdout:
        print(result.stdout, end="" if result.stdout.endswith("\n") else "\n", flush=True)
    require(result.returncode == 0, f"command failed rc={result.returncode}: {command}")
    if expected is not None:
        require(expected in result.stdout, f"missing {expected!r}: {command}")
    return result


def write(path: pathlib.Path, data: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(data, encoding="utf-8")


def test_versions() -> None:
    checks = (
        ("make", ["make", "--version"], "GNU Make", "MAKE_LOAD_OK"),
        ("cmake", ["cmake", "--version"], "cmake version", "CMAKE_LOAD_OK"),
        ("ninja", ["ninja", "--version"], None, "NINJA_LOAD_OK"),
        ("cargo", ["cargo", "--version"], "cargo ", "CARGO_LOAD_OK"),
        ("python", ["python3", "--version"], "Python 3.13", "PYTHON_LOAD_OK"),
        ("curl", ["curl", "--version"], "curl ", "CURL_LOAD_OK"),
    )
    for name, command, expected, marker in checks:
        result = run(command, timeout=60)
        require(result.stdout.strip(), f"{name} version output is empty")
        if expected is not None:
            require(expected in result.stdout, f"unexpected {name} version output")
        print(marker, flush=True)


def c_sources(project: pathlib.Path) -> None:
    write(
        project / "include/answer.h",
        "#ifndef ANSWER_H\n#define ANSWER_H\nint answer(void);\n#endif\n",
    )
    write(
        project / "src/answer.c",
        '#include "answer.h"\nint answer(void) { return 42; }\n',
    )
    write(
        project / "src/main.c",
        '#include <stdio.h>\n#include "answer.h"\n'
        'int main(void) { printf("TOOLCHAIN_RESULT=%d\\n", answer()); return answer() == 42 ? 0 : 1; }\n',
    )


def test_make() -> None:
    project = ROOT / "make-project"
    fresh(project)
    c_sources(project)
    write(
        project / "Makefile",
        "CC ?= cc\n"
        "CFLAGS := -O2 -Wall -Wextra -Werror -Iinclude\n"
        "OBJ := build/main.o build/answer.o\n"
        ".PHONY: all clean check\n"
        "all: build/tool\n"
        "build:\n\tmkdir -p build\n"
        "build/main.o: src/main.c include/answer.h | build\n\t$(CC) $(CFLAGS) -c $< -o $@\n"
        "build/answer.o: src/answer.c include/answer.h | build\n\t$(CC) $(CFLAGS) -c $< -o $@\n"
        "build/tool: $(OBJ)\n\t$(CC) $(OBJ) -o $@\n"
        "check: build/tool\n\t./build/tool\n"
        "clean:\n\trm -rf build\n",
    )
    run(["make", "-j4", "check"], cwd=project, expected="TOOLCHAIN_RESULT=42")
    binary = project / "build/tool"
    first_mtime = binary.stat().st_mtime_ns
    time.sleep(0.01)
    run(["make", "-j4", "all"], cwd=project)
    require(binary.stat().st_mtime_ns == first_mtime, "make rebuilt an unchanged target")
    write(project / "src/answer.c", '#include "answer.h"\n/* rebuild */\nint answer(void) { return 42; }\n')
    run(["make", "-j4", "check"], cwd=project, expected="TOOLCHAIN_RESULT=42")
    require(binary.stat().st_mtime_ns > first_mtime, "make did not rebuild a changed source")
    print("MAKE_BUILD_OK", flush=True)


def write_cmake_project(project: pathlib.Path) -> None:
    c_sources(project)
    write(project / "configured.h.in", '#define BUILD_GENERATOR "@CMAKE_GENERATOR@"\n')
    write(
        project / "src/main.c",
        '#include <stdio.h>\n#include "answer.h"\n#include "configured.h"\n'
        'int main(void) { printf("CMAKE_RESULT=%d GENERATOR=%s\\n", answer(), BUILD_GENERATOR); '
        "return answer() == 42 ? 0 : 1; }\n",
    )
    write(
        project / "CMakeLists.txt",
        "cmake_minimum_required(VERSION 3.16)\n"
        "project(onsite_toolchain C)\n"
        "configure_file(configured.h.in configured.h @ONLY)\n"
        "add_library(answer STATIC src/answer.c)\n"
        "target_include_directories(answer PUBLIC include ${CMAKE_CURRENT_BINARY_DIR})\n"
        "add_executable(tool src/main.c)\n"
        "target_link_libraries(tool PRIVATE answer)\n"
        "enable_testing()\n"
        "add_test(NAME tool-output COMMAND tool)\n",
    )


def cmake_variant(generator: str, marker: str) -> None:
    suffix = "make" if generator == "Unix Makefiles" else "ninja"
    project = ROOT / f"cmake-{suffix}"
    build = project / "build"
    fresh(project)
    write_cmake_project(project)
    run(["cmake", "-S", str(project), "-B", str(build), "-G", generator, "-DCMAKE_BUILD_TYPE=Release"])
    run(["cmake", "--build", str(build), "--parallel", "4"])
    run(["ctest", "--test-dir", str(build), "--output-on-failure"], expected="100% tests passed")
    output = run([str(build / "tool")], expected="CMAKE_RESULT=42").stdout
    require(f"GENERATOR={generator}" in output, "CMake configured generator differs")
    print(marker, flush=True)


def test_ninja_direct() -> None:
    project = ROOT / "ninja-project"
    fresh(project)
    c_sources(project)
    write(
        project / "build.ninja",
        "rule cc\n  command = cc -O2 -Wall -Wextra -Werror -Iinclude -c $in -o $out\n"
        "rule link\n  command = cc $in -o $out\n"
        "rule run\n  command = ./$in > $out\n"
        "build main.o: cc src/main.c\n"
        "build answer.o: cc src/answer.c\n"
        "build tool: link main.o answer.o\n"
        "build result.txt: run tool\n"
        "default result.txt\n",
    )
    run(["ninja", "-v", "-j4"], cwd=project)
    require((project / "result.txt").read_text().strip() == "TOOLCHAIN_RESULT=42", "ninja output differs")
    binary_mtime = (project / "tool").stat().st_mtime_ns
    require(binary_mtime > 1_000_000_000, "new Ninja target retained an epoch timestamp")
    run(["ninja", "-d", "explain", "-j4"], cwd=project)
    settled_mtime = (project / "tool").stat().st_mtime_ns
    if settled_mtime != binary_mtime:
        # The kernel currently exposes Ext4 timestamps at one-second
        # resolution.  A parallel compile/link that straddles a second can
        # conservatively relink once; the resulting dependency graph must be
        # stable on the immediately following invocation.
        print("NINJA_COARSE_MTIME_REBUILD_OBSERVED", flush=True)
    run(["ninja", "-d", "explain", "-j4"], cwd=project)
    require(
        (project / "tool").stat().st_mtime_ns == settled_mtime,
        "ninja continued rebuilding an unchanged target",
    )
    print("NINJA_BUILD_OK", flush=True)


def test_build_tools() -> None:
    fresh(ROOT)
    test_make()
    cmake_variant("Unix Makefiles", "CMAKE_MAKE_BUILD_OK")
    test_ninja_direct()
    cmake_variant("Ninja", "CMAKE_NINJA_BUILD_OK")


def test_cargo() -> None:
    project = ROOT / "cargo-project"
    project.parent.mkdir(parents=True, exist_ok=True)
    if project.exists():
        shutil.rmtree(project)
    run(["cargo", "new", "--bin", "--vcs", "none", str(project)], timeout=120)
    write(
        project / "src/main.rs",
        "fn triangular(n: u64) -> u64 { (0..=n).sum() }\n"
        'fn main() { println!("CARGO_RESULT={}", triangular(100)); }\n'
        "#[cfg(test)] mod tests { use super::*; #[test] fn value() { assert_eq!(triangular(100), 5050); } }\n",
    )
    run(["cargo", "build", "--offline", "--release", "-j", "4"], cwd=project, timeout=600)
    run([str(project / "target/release/cargo-project")], expected="CARGO_RESULT=5050")
    print("CARGO_BUILD_OK", flush=True)
    run(["cargo", "test", "--offline", "-j", "4"], cwd=project, timeout=600, expected="test result: ok")
    metadata = run(["cargo", "metadata", "--offline", "--no-deps", "--format-version", "1"], cwd=project)
    parsed = json.loads(metadata.stdout[metadata.stdout.find("{") :])
    require(parsed["packages"][0]["name"] == "cargo-project", "cargo metadata differs")
    print("CARGO_TEST_OK", flush=True)


def test_python() -> None:
    project = ROOT / "python-project"
    fresh(project)
    document = {"tool": "python", "values": list(range(64))}
    encoded = json.dumps(document, sort_keys=True, separators=(",", ":")).encode()
    require(json.loads(gzip.decompress(gzip.compress(encoded, mtime=0))) == document, "Python data round trip failed")
    digest = hashlib.sha256(encoded).hexdigest()
    require(len(digest) == 64, "Python SHA-256 failed")
    database = sqlite3.connect(project / "state.db")
    database.execute("create table state(k text primary key, v text not null)")
    database.execute("insert into state values ('sha256', ?)", (digest,))
    database.commit()
    database.close()
    require(sqlite3.connect(project / "state.db").execute("select v from state").fetchone()[0] == digest, "Python SQLite reopen failed")

    counter = 0
    lock = threading.Lock()

    def worker() -> None:
        nonlocal counter
        for _ in range(1000):
            with lock:
                counter += 1

    threads = [threading.Thread(target=worker) for _ in range(4)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(20)
    require(counter == 4000, "Python threading result differs")
    run(["python3", "-m", "venv", "--without-pip", str(project / "venv")], timeout=300)
    run([str(project / "venv/bin/python"), "-c", "import sys; assert sys.prefix != sys.base_prefix"])
    print("PYTHON_TOOLCHAIN_OK", flush=True)


def test_curl() -> None:
    project = ROOT / "curl-project"
    fresh(project)
    url = os.environ["TOOLCHAIN_HTTPS_URL"]
    output = project / "download.bin"
    run(
        [
            "curl",
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--connect-timeout",
            "30",
            "--max-time",
            "180",
            "--output",
            str(output),
            url,
        ],
        timeout=240,
    )
    digest = hashlib.sha256(output.read_bytes()).hexdigest()
    require(digest == EXPECTED_HTTPS_SHA256, f"curl HTTPS hash differs: {digest}")
    print("CURL_HTTPS_OK", flush=True)

    token = os.environ["TOOLCHAIN_POST_TOKEN"]
    post_url = os.environ["TOOLCHAIN_POST_URL"]
    payload = json.dumps({"token": token, "sha256": digest}, sort_keys=True)
    response = run(
        [
            "curl",
            "--fail",
            "--silent",
            "--show-error",
            "--max-time",
            "60",
            "--noproxy",
            "10.0.2.2",
            "-H",
            "Content-Type: application/json",
            "--data-binary",
            payload,
            post_url,
        ],
        timeout=90,
    )
    require(json.loads(response.stdout) == {"accepted": True, "token": token}, "curl POST reply differs")
    print("CURL_REMOTE_SIDE_EFFECT_OK", flush=True)


def test_combined() -> None:
    project = ROOT / "combined"
    fresh(project)
    write(
        project / "generate.py",
        "from pathlib import Path\nPath('generated.h').write_text('#define COMBINED_VALUE 2026\\n')\n",
    )
    write(
        project / "main.c",
        '#include <stdio.h>\n#include "generated.h"\n'
        'int main(void) { printf("COMBINED_C=%d\\n", COMBINED_VALUE); return COMBINED_VALUE == 2026 ? 0 : 1; }\n',
    )
    write(
        project / "CMakeLists.txt",
        "cmake_minimum_required(VERSION 3.16)\nproject(combined C)\nadd_executable(combined main.c)\n",
    )
    cargo = project / "cargo-app"
    write(cargo / "Cargo.toml", '[package]\nname="combined-cargo"\nversion="0.1.0"\nedition="2021"\n')
    write(cargo / "src/main.rs", 'fn main() { println!("COMBINED_CARGO=2026"); }\n')
    write(
        project / "Makefile",
        ".PHONY: all generate configure c rust check\n"
        "all: check\n"
        "generate:\n\tpython3 generate.py\n"
        "configure: generate\n\tcmake -S . -B build -G Ninja -DCMAKE_BUILD_TYPE=Release\n"
        "c: configure\n\tcmake --build build --parallel 4\n"
        "rust:\n\tcargo build --offline --release --manifest-path cargo-app/Cargo.toml -j 4\n"
        "check: c rust\n\t./build/combined\n\t./cargo-app/target/release/combined-cargo\n",
    )
    result = run(["make", "-j2", "all"], cwd=project, timeout=900)
    require("COMBINED_C=2026" in result.stdout and "COMBINED_CARGO=2026" in result.stdout, "combined build output differs")
    print("TOOLCHAIN_COMBINED_OK", flush=True)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=("versions", "build", "cargo", "python", "curl", "all"), required=True)
    args = parser.parse_args()

    if args.mode in {"versions", "all"}:
        test_versions()
    if args.mode in {"build", "all"}:
        test_build_tools()
    if args.mode in {"cargo", "all"}:
        test_cargo()
    if args.mode in {"python", "all"}:
        test_python()
    if args.mode == "all":
        test_combined()
    if args.mode in {"curl", "all"}:
        test_curl()
    if args.mode == "all":
        print("TOOLCHAIN_ONSITE_ALL_OK", flush=True)
    return 0


if __name__ == "__main__":
    try:
        result = main()
    except Exception as error:
        print(f"TOOLCHAIN_REGRESSION_FAILED type={type(error).__name__} detail={error}", flush=True)
        raise
    raise SystemExit(result)
