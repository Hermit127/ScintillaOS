#!/usr/bin/env python3
"""Expose a host loopback HTTP CONNECT proxy to QEMU SLIRP, without logging data."""

from __future__ import annotations

import argparse
import asyncio


CHUNK_SIZE = 65536


async def copy_stream(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    buffer_bytes: int,
) -> None:
    # QEMU's guest TCP consumer can be much slower than the host proxy.  If
    # drain() backpressures the upstream read directly, a multi-megabyte Git
    # response remains open for minutes and some desktop proxies truncate its
    # tail.  A bounded queue lets the relay receive the complete upstream
    # response promptly while preserving backpressure for larger transfers.
    queue: asyncio.Queue[bytes | None] = asyncio.Queue(
        maxsize=max(1, buffer_bytes // CHUNK_SIZE)
    )

    async def produce() -> None:
        try:
            while data := await reader.read(CHUNK_SIZE):
                await queue.put(data)
        finally:
            await queue.put(None)

    async def consume() -> None:
        while (data := await queue.get()) is not None:
            writer.write(data)
            await writer.drain()
        if writer.can_write_eof():
            try:
                writer.write_eof()
                await writer.drain()
            except (ConnectionError, OSError):
                pass

    producer = asyncio.create_task(produce())
    consumer = asyncio.create_task(consume())
    try:
        await asyncio.gather(producer, consumer)
    except (ConnectionError, OSError):
        producer.cancel()
        consumer.cancel()
        await asyncio.gather(producer, consumer, return_exceptions=True)


async def relay_connection(
    guest_reader: asyncio.StreamReader,
    guest_writer: asyncio.StreamWriter,
    upstream_port: int,
    buffer_bytes: int,
) -> None:
    try:
        proxy_reader, proxy_writer = await asyncio.open_connection(
            "127.0.0.1", upstream_port
        )
    except (ConnectionError, OSError):
        guest_writer.close()
        await guest_writer.wait_closed()
        return

    try:
        await asyncio.gather(
            copy_stream(guest_reader, proxy_writer, buffer_bytes),
            copy_stream(proxy_reader, guest_writer, buffer_bytes),
        )
    except (ConnectionError, OSError):
        pass
    finally:
        proxy_writer.close()
        guest_writer.close()
        await asyncio.gather(
            proxy_writer.wait_closed(), guest_writer.wait_closed(), return_exceptions=True
        )


async def run(listen_port: int, upstream_port: int, buffer_bytes: int) -> None:
    server = await asyncio.start_server(
        lambda reader, writer: relay_connection(
            reader, writer, upstream_port, buffer_bytes
        ),
        "127.0.0.1",
        listen_port,
    )
    print(
        f"proxy relay ready: 127.0.0.1:{listen_port} -> "
        f"127.0.0.1:{upstream_port}",
        flush=True,
    )
    async with server:
        await server.serve_forever()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--listen-port", type=int, default=27898)
    parser.add_argument("--upstream-port", type=int, default=7897)
    parser.add_argument(
        "--buffer-mib",
        type=int,
        default=32,
        help="bounded buffer per relay direction (default: 32 MiB)",
    )
    args = parser.parse_args()
    for value in (args.listen_port, args.upstream_port):
        if not 1 <= value <= 65535:
            parser.error("ports must be between 1 and 65535")
    if not 1 <= args.buffer_mib <= 256:
        parser.error("--buffer-mib must be between 1 and 256")
    try:
        asyncio.run(
            run(args.listen_port, args.upstream_port, args.buffer_mib * 1024 * 1024)
        )
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    main()
