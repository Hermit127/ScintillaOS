# SMP4 validation manifest

- Generated: 2026-07-25 22:59:48 +0800
- QEMU: `QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.1)`
- Topology: `-smp 4 -m 256M`
- Suite: `libctest-musl`
- Result: `3/3 passed`

## Immutable input hashes

- kernel: `65d932015f85035edb905464387bba5eb55abd22bf21219ccce00e880a577c50` — `/home/lyz/OS/oskernel2026test/kernel-rv`
- root disk: `ae7dfd63cf838a0e19c24ac4ff9e8ee45c99020e2df7a6d07964f93457ce5b43` — `/home/lyz/OS/oskernel2026test/target/riscv64gc-unknown-none-elf/release/fs.img`
- test disk: `1793d1345c61353fca60aa5d4cddd361ddc7d6de96a6466432156e190420940f` — `/home/lyz/OS/oskernel2026test/artifacts/test-images/libctest-rv-musl-official-20250624.img`

## Cold boot rounds

- round 1: PASS; log `round-01/serial.log`; detail `completed`
- round 2: PASS; log `round-02/serial.log`; detail `completed`
- round 3: PASS; log `round-03/serial.log`; detail `completed`
