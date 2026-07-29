# SMP4 validation manifest

- Generated: 2026-07-26 12:12:18 +0800
- QEMU: `QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.1)`
- Topology: `-smp 4 -m 256M`
- Suite: `lmbench-musl`
- Result: `1/1 passed`

## Immutable input hashes

- kernel: `f2a591bef08e26b9357f5915c798ca9d387fe0aeb7922e9aa893cab251d29951` — `/home/lyz/OS/oskernel2026test/kernel-rv`
- root disk: `ae7dfd63cf838a0e19c24ac4ff9e8ee45c99020e2df7a6d07964f93457ce5b43` — `/home/lyz/OS/oskernel2026test/target/riscv64gc-unknown-none-elf/release/fs.img`
- test disk: `fe72922a5c0b49ee10665e1a7b8ab81445cb4d6bf4b7bd6de3067e128a9b05ad` — `/home/lyz/OS/testsuits-for-oskernel/lmbench-rv-official.img`

## Cold boot rounds

- round 1: PASS; log `round-01/serial.log`; detail `completed`; metrics `{"case_end": 0, "case_start": 0, "fail": 0, "group_end": 1, "pass": 0, "skip": 0}`
