# SMP4 validation manifest

- Generated: 2026-08-14 19:51:21 +0800
- QEMU: `QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.1)`
- Topology: `-smp 4 -m 256M`
- Suite: `directed`
- Result: `0/1 passed`

## Immutable input hashes

- kernel: `a2acac0e4fc865ea9a9cfd19a8bf9dd136cbe452e549fabc062444e99dccc1ef` — `/home/lyz/OS/oskernel2026test/kernel-rv`
- root disk: `8f9e9e36297b68be53759809c78c8a64f2ea980c918f722bd2e88635f7426396` — `/home/lyz/OS/oskernel2026test/fs-img.img`
- test disk: `e861218ff4d1a78f4e0413a01a9746e280a98f69488fcae14cf8c22d2a282266` — `/home/lyz/OS/oskernel2026test/test_disk.img`

## Cold boot rounds

- round 1: FAIL; log `round-01/serial.log`; detail `serial marker not seen: '>> '`; metrics `{"case_end": 0, "case_start": 0, "fail": 0, "group_end": 0, "pass": 0, "skip": 0}`
