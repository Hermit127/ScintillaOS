# SMP4 validation manifest

- Generated: 2026-07-25 23:14:17 +0800
- QEMU: `QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.1)`
- Topology: `-smp 4 -m 256M`
- Suite: `lmbench-glibc`
- Result: `3/3 passed`

## Immutable input hashes

- kernel: `65d932015f85035edb905464387bba5eb55abd22bf21219ccce00e880a577c50` — `/home/lyz/OS/oskernel2026test/kernel-rv`
- root disk: `ae7dfd63cf838a0e19c24ac4ff9e8ee45c99020e2df7a6d07964f93457ce5b43` — `/home/lyz/OS/oskernel2026test/target/riscv64gc-unknown-none-elf/release/fs.img`
- test disk: `fe72922a5c0b49ee10665e1a7b8ab81445cb4d6bf4b7bd6de3067e128a9b05ad` — `/home/lyz/OS/testsuits-for-oskernel/lmbench-rv-official.img`

## Cold boot rounds

- round 1: PASS; log `round-01/serial.log`; detail `completed`
- round 2: PASS; log `round-02/serial.log`; detail `completed`
- round 3: PASS; log `round-03/serial.log`; detail `completed`
