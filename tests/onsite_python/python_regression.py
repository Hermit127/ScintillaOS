#!/usr/bin/env python3
"""Python 3.13 onsite regression workload.

The tests exercise the unmodified interpreter and standard library from the
official final image.  Every mode validates results and durable side effects;
the printed markers are summaries, not substitutes for the assertions.
"""

from __future__ import annotations

import argparse
import asyncio
import compileall
import gzip
import hashlib
import json
import multiprocessing
import os
import pathlib
import py_compile
import select
import shutil
import socket
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.request
import venv


ROOT = pathlib.Path("/tmp/python-onsite")
FS_ROOT = pathlib.Path("/tmp/python-onsite-fs")
EXPECTED_JSON_SHA256 = "765c4e50b607c43fa83e1c4e0bb8281f3ece077c798c8b5cd3fee1ed28d5ef06"


def require(condition: bool, message: str) -> None:
    if not condition:
        raise AssertionError(message)


def fresh_directory(path: pathlib.Path) -> None:
    if path.exists() or path.is_symlink():
        shutil.rmtree(path)
    path.mkdir(parents=True)


def process_worker(connection, begin: int, end: int) -> None:
    connection.send((os.getpid(), sum(range(begin, end))))
    connection.close()


def test_core() -> None:
    print("PYTHON_CORE_STAGE begin", flush=True)
    require(sys.version_info[:2] == (3, 13), f"unexpected Python version: {sys.version}")
    fresh_directory(ROOT)

    document = {
        "message": "ScintillaOS Python onsite",
        "nested": {"enabled": True, "value": None},
        "numbers": list(range(16)),
    }
    encoded = json.dumps(document, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
    require(hashlib.sha256(encoded).hexdigest() == EXPECTED_JSON_SHA256, "JSON/hash mismatch")
    require(json.loads(encoded) == document, "JSON round trip failed")
    compressed = gzip.compress(encoded, mtime=0)
    require(gzip.decompress(compressed) == encoded, "gzip round trip failed")
    print("PYTHON_CORE_STAGE data", flush=True)
    print("PYTHON_CORE_STAGE random-begin", flush=True)
    require(len(os.urandom(32)) == 32, "getrandom/urandom failed")
    print("PYTHON_CORE_STAGE random-end", flush=True)
    started = time.monotonic()
    print("PYTHON_CORE_STAGE sleep-begin", flush=True)
    time.sleep(0.02)
    print("PYTHON_CORE_STAGE sleep-end", flush=True)
    require(time.monotonic() > started, "monotonic clock did not advance")
    print("PYTHON_CORE_STAGE clock-random", flush=True)

    module = ROOT / "onsite_module.py"
    module.write_text("VALUE = 2026\n", encoding="utf-8")
    require(os.listxattr(module) == [], "new file unexpectedly has xattrs")
    require(compileall.compile_file(module, quiet=1, force=True), "compileall failed")
    cached = pathlib.Path(py_compile.compile(str(module), doraise=True))
    require(cached.is_file() and cached.stat().st_size > 0, "pyc was not created")
    print("PYTHON_CORE_STAGE bytecode", flush=True)
    imported = subprocess.run(
        [sys.executable, "-c", "import onsite_module; print(onsite_module.VALUE)"],
        cwd=ROOT,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    require(imported.returncode == 0 and imported.stdout.strip() == "2026", "pyc/import failed")
    print("PYTHON_STDLIB_OK", flush=True)

    environment = ROOT / "venv"
    print("PYTHON_CORE_STAGE venv-begin", flush=True)
    venv.EnvBuilder(with_pip=False, clear=True, symlinks=True).create(environment)
    venv_python = environment / "bin/python"
    require(venv_python.exists(), "venv interpreter missing")
    checked = subprocess.run(
        [str(venv_python), "-c", "import sys; print(sys.prefix != sys.base_prefix)"],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    require(checked.returncode == 0 and checked.stdout.strip() == "True", "venv execution failed")
    print("PYTHON_VENV_OK", flush=True)


def test_fs_verify(database: pathlib.Path) -> None:
    connection = sqlite3.connect(database)
    row = connection.execute("select count(*), sum(id) from records").fetchone()
    rolled_back = connection.execute("select count(*) from records where value='ROLLBACK'").fetchone()[0]
    connection.close()
    require(row == (128, 8128), f"reopened SQLite contents differ: {row}")
    require(rolled_back == 0, "rolled-back SQLite row became visible")
    result = json.loads((database.parent / "result.json").read_text(encoding="utf-8"))
    require(result == {"bytes": 12345, "rows": 128, "sum": 8128}, "filesystem manifest differs")
    print("PYTHON_FS_REOPEN_OK", flush=True)


def test_fs() -> None:
    fresh_directory(FS_ROOT)
    source = FS_ROOT / "publish.lock"
    destination = FS_ROOT / "publish"
    payload = bytes(range(256)) * 1024
    with source.open("wb") as output:
        output.write(payload)
        output.flush()
        os.fsync(output.fileno())
    destination.write_bytes(b"old\n")
    os.replace(source, destination)
    require(not source.exists(), "replace source still exists")
    require(destination.read_bytes() == payload, "replace destination differs")

    hardlink = FS_ROOT / "publish.hard"
    symlink = FS_ROOT / "publish.sym"
    os.link(destination, hardlink)
    os.symlink(destination.name, symlink)
    require(os.stat(destination).st_ino == os.stat(hardlink).st_ino, "hard link inode differs")
    require(os.readlink(symlink) == destination.name, "symlink target differs")
    os.truncate(destination, 12345)
    require(destination.stat().st_size == 12345, "truncate stat length differs")
    require(len(destination.read_bytes()) == 12345, "truncate read length differs")
    require("publish" in {entry.name for entry in FS_ROOT.iterdir()}, "directory entry missing")
    executable = os.readlink("/proc/self/exe")
    require("python3.13" in executable, f"unexpected /proc/self/exe: {executable}")
    print("PYTHON_FS_ATOMIC_OK", flush=True)

    database = FS_ROOT / "onsite.db"
    first = sqlite3.connect(database, timeout=10)
    journal = first.execute("pragma journal_mode=wal").fetchone()[0].lower()
    require(journal == "wal", f"SQLite WAL unavailable: {journal}")
    first.execute("create table records(id integer primary key, value text not null)")
    first.executemany(
        "insert into records(id, value) values (?, ?)",
        ((index, hashlib.sha256(f"row-{index}".encode()).hexdigest()) for index in range(128)),
    )
    first.commit()
    second = sqlite3.connect(database, timeout=10)
    require(second.execute("select count(*) from records").fetchone()[0] == 128, "second connection cannot see commit")
    first.execute("insert into records(id, value) values (999, 'ROLLBACK')")
    first.rollback()
    require(second.execute("select count(*) from records").fetchone()[0] == 128, "rollback changed row count")
    second.close()
    first.execute("pragma wal_checkpoint(full)").fetchone()
    first.close()
    (FS_ROOT / "result.json").write_text(
        json.dumps({"bytes": destination.stat().st_size, "rows": 128, "sum": 8128}, sort_keys=True),
        encoding="utf-8",
    )
    os.sync()
    verified = subprocess.run(
        [sys.executable, __file__, "--mode", "fs-verify", "--database", str(database)],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    require(verified.returncode == 0 and "PYTHON_FS_REOPEN_OK" in verified.stdout, verified.stdout)
    print("PYTHON_SQLITE_OK", flush=True)
    print("PYTHON_PROC_SYMLINK_OK", flush=True)


async def asyncio_workload() -> None:
    reader, writer = socket.socketpair()
    reader.setblocking(False)
    writer.setblocking(False)
    loop = asyncio.get_running_loop()
    payload = b"asyncio-epoll-payload" * 128
    await loop.sock_sendall(writer, payload)
    received = bytearray()
    while len(received) < len(payload):
        received.extend(await loop.sock_recv(reader, len(payload) - len(received)))
    reader.close()
    writer.close()
    require(bytes(received) == payload, "asyncio socketpair payload differs")
    values = await asyncio.gather(*(asyncio.sleep(0.001, result=index) for index in range(64)))
    require(sum(values) == 2016, "asyncio timer tasks differ")


def test_concurrency() -> None:
    child = subprocess.run(
        [sys.executable, "-c", "import os,sys; data=sys.stdin.buffer.read(); print(os.getpid(), data.hex())"],
        input=b"subprocess-pipe",
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        check=False,
    )
    require(child.returncode == 0 and child.stdout.rstrip().endswith(b"73756270726f636573732d70697065"), "subprocess pipe failed")
    print("PYTHON_SUBPROCESS_OK", flush=True)

    counter = 0
    lock = threading.Lock()
    condition = threading.Condition()
    ready = 0

    def thread_worker() -> None:
        nonlocal counter, ready
        with condition:
            ready += 1
            condition.notify_all()
        for _ in range(4000):
            with lock:
                counter += 1

    threads = [threading.Thread(target=thread_worker) for _ in range(8)]
    for thread in threads:
        thread.start()
    with condition:
        require(condition.wait_for(lambda: ready == len(threads), timeout=10), "thread condition timed out")
    for thread in threads:
        thread.join(timeout=20)
        require(not thread.is_alive(), "thread did not exit")
    require(counter == 32000, f"thread counter differs: {counter}")
    print("PYTHON_THREAD_OK", flush=True)

    methods = [method for method in ("fork", "spawn") if method in multiprocessing.get_all_start_methods()]
    require(methods, "no multiprocessing start method available")
    expected = sum(range(1000, 9000))
    for method in methods:
        context = multiprocessing.get_context(method)
        parent, child_connection = context.Pipe(duplex=False)
        process = context.Process(target=process_worker, args=(child_connection, 1000, 9000))
        process.start()
        child_connection.close()
        require(parent.poll(30), f"multiprocessing {method} result timed out")
        child_pid, value = parent.recv()
        parent.close()
        process.join(30)
        require(process.exitcode == 0 and child_pid > 0 and value == expected, f"multiprocessing {method} failed")
    print("PYTHON_MULTIPROCESS_OK", flush=True)

    read_fd, write_fd = os.pipe2(os.O_NONBLOCK | os.O_CLOEXEC)
    epoll = select.epoll()
    epoll.register(read_fd, select.EPOLLIN)
    os.write(write_fd, b"e")
    events = epoll.poll(5)
    require(events and events[0][0] == read_fd and events[0][1] & select.EPOLLIN, "epoll did not report pipe")
    epoll.close()
    os.close(read_fd)
    os.close(write_fd)
    asyncio.run(asyncio_workload())
    print("PYTHON_ASYNCIO_OK", flush=True)


def test_network() -> None:
    host = os.environ.get("PYTHON_TEST_DNS_HOST", "github.com")
    addresses = socket.getaddrinfo(host, 443, socket.AF_INET, socket.SOCK_STREAM)
    require(addresses and all(item[0] == socket.AF_INET for item in addresses), "DNS returned no IPv4 address")
    print("PYTHON_DNS_OK", flush=True)

    url = os.environ["PYTHON_TEST_HTTPS_URL"]
    expected_sha256 = os.environ["PYTHON_TEST_HTTPS_SHA256"]
    request = urllib.request.Request(url, headers={"User-Agent": "ScintillaOS-Python-Onsite/1"})
    with urllib.request.urlopen(request, timeout=120) as response:
        require(response.status == 200, f"HTTPS status is {response.status}")
        body = response.read()
    digest = hashlib.sha256(body).hexdigest()
    require(digest == expected_sha256, f"HTTPS payload hash differs: {digest}")
    print("PYTHON_HTTPS_OK", flush=True)

    token = os.environ["PYTHON_TEST_POST_TOKEN"]
    post_url = os.environ["PYTHON_TEST_POST_URL"]
    post_body = json.dumps({"token": token, "sha256": digest}, sort_keys=True).encode()
    post = urllib.request.Request(
        post_url,
        data=post_body,
        headers={"Content-Type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(post, timeout=30) as response:
        reply = json.loads(response.read())
    require(reply == {"accepted": True, "token": token}, f"POST reply differs: {reply}")
    print("PYTHON_REMOTE_SIDE_EFFECT_OK", flush=True)


def main() -> int:
    print("PYTHON_MODULE_IMPORTS_OK", flush=True)
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--mode",
        choices=("core", "fs", "fs-verify", "concurrency", "network", "all"),
        required=True,
    )
    parser.add_argument("--database", type=pathlib.Path)
    args = parser.parse_args()

    if args.mode == "fs-verify":
        require(args.database is not None, "--database is required for fs-verify")
        test_fs_verify(args.database)
        return 0
    if args.mode in {"core", "all"}:
        test_core()
    if args.mode in {"fs", "all"}:
        test_fs()
    if args.mode in {"concurrency", "all"}:
        test_concurrency()
    if args.mode in {"network", "all"}:
        test_network()
    if args.mode == "all":
        print("PYTHON_ONSITE_ALL_OK", flush=True)
    return 0


if __name__ == "__main__":
    try:
        result = main()
    except Exception as error:
        print(f"PYTHON_REGRESSION_FAILED type={type(error).__name__} detail={error}", flush=True)
        raise
    raise SystemExit(result)
