# SMP4 validation manifest

- Generated: 2026-08-14 20:42:52 +0800
- QEMU: `QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.1)`
- Topology: `-smp 4 -m 256M`
- Suite: `directed`
- Result: `0/1 passed`

## Immutable input hashes

- kernel: `6c8b933dec8068dd3df0977f61d4a645c7ffdbd74ae9e9cd7368ed14c3d02dc1` — `/home/lyz/OS/oskernel2026test/kernel-rv`
- root disk: `d548a6dbdcf61959575089d6e61cc66816f6f045f5ee438b411c150fa5c47d74` — `/home/lyz/OS/oskernel2026test/disk.img`
- test disk: `e861218ff4d1a78f4e0413a01a9746e280a98f69488fcae14cf8c22d2a282266` — `/home/lyz/OS/oskernel2026test/test_disk.img`

## Cold boot rounds

- round 1: FAIL; log `round-01/serial.log`; detail `no serial output for 120 seconds`; metrics `{"case_end": 0, "case_start": 0, "fail": 0, "group_end": 0, "pass": 0, "skip": 0}`
