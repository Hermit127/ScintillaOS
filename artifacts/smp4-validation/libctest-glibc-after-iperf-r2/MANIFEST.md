# SMP4 validation manifest

- Generated: 2026-07-26 12:16:04 +0800
- QEMU: `QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.1)`
- Topology: `-smp 4 -m 256M`
- Suite: `libctest-glibc`
- Result: `1/1 passed`

## Immutable input hashes

- kernel: `f2a591bef08e26b9357f5915c798ca9d387fe0aeb7922e9aa893cab251d29951` — `/home/lyz/OS/oskernel2026test/kernel-rv`
- root disk: `ae7dfd63cf838a0e19c24ac4ff9e8ee45c99020e2df7a6d07964f93457ce5b43` — `/home/lyz/OS/oskernel2026test/target/riscv64gc-unknown-none-elf/release/fs.img`
- test disk: `090d40b3058994b96b654daf80432af5527f9a8373f171c47a112a4fa8758316` — `/home/lyz/OS/oskernel2026test/artifacts/test-images/libctest-rv-glibc-official-20250624.img`

## Cold boot rounds

- round 1: PASS; log `round-01/serial.log`; detail `completed`; metrics `{"case_end": 217, "case_start": 217, "fail": 40, "group_end": 1, "pass": 177, "skip": 0}`
