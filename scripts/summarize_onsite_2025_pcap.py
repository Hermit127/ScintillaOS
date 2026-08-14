#!/usr/bin/env python3
"""Print a compact TCP tail from a classic Ethernet pcap without dependencies."""

from __future__ import annotations

import argparse
import collections
import ipaddress
import pathlib
import struct


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("pcap", type=pathlib.Path)
    parser.add_argument("--tail", type=int, default=30)
    args = parser.parse_args()
    data = args.pcap.read_bytes()
    if len(data) < 24:
        parser.error("truncated pcap")
    magic = data[:4]
    if magic in (b"\xd4\xc3\xb2\xa1", b"\x4d\x3c\xb2\xa1"):
        endian = "<"
    elif magic in (b"\xa1\xb2\xc3\xd4", b"\xa1\xb2\x3c\x4d"):
        endian = ">"
    else:
        parser.error("not a classic pcap")

    offset = 24
    packets = []
    flows: collections.Counter[tuple[str, int, str, int]] = collections.Counter()
    flow_times: dict[tuple[str, int, str, int], list[float]] = {}
    while offset + 16 <= len(data):
        seconds, micros, captured, _original = struct.unpack_from(endian + "IIII", data, offset)
        offset += 16
        frame = data[offset : offset + captured]
        offset += captured
        if len(frame) < 34 or frame[12:14] != b"\x08\x00":
            continue
        ip_start = 14
        ihl = (frame[ip_start] & 0x0F) * 4
        if ihl < 20 or len(frame) < ip_start + ihl + 20 or frame[ip_start + 9] != 6:
            continue
        tcp_start = ip_start + ihl
        src = str(ipaddress.IPv4Address(frame[ip_start + 12 : ip_start + 16]))
        dst = str(ipaddress.IPv4Address(frame[ip_start + 16 : ip_start + 20]))
        sport, dport, seq, ack = struct.unpack_from("!HHII", frame, tcp_start)
        header_len = (frame[tcp_start + 12] >> 4) * 4
        flags = frame[tcp_start + 13]
        window = struct.unpack_from("!H", frame, tcp_start + 14)[0]
        total_len = struct.unpack_from("!H", frame, ip_start + 2)[0]
        payload = max(0, total_len - ihl - header_len)
        flow = (src, sport, dst, dport)
        timestamp = seconds + micros / 1_000_000
        flows[flow] += payload
        if flow not in flow_times:
            flow_times[flow] = [timestamp, timestamp]
        else:
            flow_times[flow][1] = timestamp
        packets.append(
            (timestamp, flow, seq, ack, window, flags, payload)
        )

    print("TCP payload bytes by direction:")
    for flow, count in flows.most_common():
        first, last = flow_times[flow]
        print(
            f"  {flow[0]}:{flow[1]} -> {flow[2]}:{flow[3]} {count} "
            f"duration={last - first:.3f}s"
        )
    tail_packets = packets[-args.tail :] if args.tail else []
    print(f"last {len(tail_packets)} TCP packets:")
    for timestamp, flow, seq, ack, window, flags, payload in tail_packets:
        print(
            f"  {timestamp:.6f} {flow[0]}:{flow[1]} -> {flow[2]}:{flow[3]} "
            f"seq={seq} ack={ack} win={window} flags=0x{flags:02x} len={payload}"
        )


if __name__ == "__main__":
    main()
