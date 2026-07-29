# SMP4 validation manifest

- Generated: 2026-07-26 12:01:20 +0800
- QEMU: `QEMU emulator version 10.2.1 (Debian 1:10.2.1+ds-1ubuntu3.1)`
- Topology: `-smp 4 -m 256M`
- Suite: `iperf-musl`
- Result: `1/1 passed`

## Immutable input hashes

- kernel: `5278f058fb10e4eac0f82a7d57eb97a190bdfad524212008ba08b7948c72861d` — `/home/lyz/OS/oskernel2026test/kernel-rv`
- root disk: `ae7dfd63cf838a0e19c24ac4ff9e8ee45c99020e2df7a6d07964f93457ce5b43` — `/home/lyz/OS/oskernel2026test/target/riscv64gc-unknown-none-elf/release/fs.img`
- test disk: `b57fba410d89ea6b651ceff33fee6f9c573b71cb9b40fa9244c0a3c68cc866a4` — `/home/lyz/OS/oskernel2026test/artifacts/test-images/iperf-official-riscv.img`

## Cold boot rounds

- round 1: PASS; log `round-01/serial.log`; detail `completed`; metrics `{"case_end": 0, "case_start": 0, "fail": 0, "group_end": 1, "pass": 0, "skip": 0}`
