# SMP4 final validation manifest

- Date: 2026-07-26
- Source branch: `smp-riscv4`
- Topology: QEMU RISC-V `virt`, `-cpu max,v=true,vlen=128`, `-m 256M`,
  `-smp 4`, two VirtIO block devices
- Final ordinary release kernel SHA-256 after the `main` iperf adaptation:
  `f2a591bef08e26b9357f5915c798ca9d387fe0aeb7922e9aa893cab251d29951`
- Root image SHA-256:
  `ae7dfd63cf838a0e19c24ac4ff9e8ee45c99020e2df7a6d07964f93457ce5b43`

## Immutable test inputs

| Image | SHA-256 |
|---|---|
| BusyBox/lmbench official combined disk | `fe72922a5c0b49ee10665e1a7b8ab81445cb4d6bf4b7bd6de3067e128a9b05ad` |
| musl libc-test official 20250624 disk | `1793d1345c61353fca60aa5d4cddd361ddc7d6de96a6466432156e190420940f` |
| glibc libc-test official 20250624 disk | `090d40b3058994b96b654daf80432af5527f9a8373f171c47a112a4fa8758316` |
| Official musl/glibc iperf disk | `b57fba410d89ea6b651ceff33fee6f9c573b71cb9b40fa9244c0a3c68cc866a4` |

Every validation round used separate root/test qcow2 overlays. The repository
keeps each retained suite's `MANIFEST.md`, input/post-run hashes and
`results.json`. Per-round QEMU commands, complete serial logs, monitor dumps and
writable overlays were moved out of Git after validation.

## Raw evidence archive

- Host path: `/home/lyz/OS/oskernel2026test-validation-raw-20260726/`
- Contents: 112 retained cold-boot rounds, 488 files, 311,418,095 bytes
- Aggregate SHA-256 of the sorted per-file SHA-256 listing:
  `c7fce4a7126a1ca2e584b82d2463949db5ba696f1628f8826a4a0a705f41451d`

The archive mirrors the retained evidence directory names. Copy a suite's
`round-*` directories back beside its repository summary only when raw-log
inspection is required.

## Final evidence

| Suite | Result | Evidence directory |
|---|---:|---|
| Four-hart directed | 20/20 | `../smp4-validation-final-directed-20260725-r1/` |
| musl BusyBox | 3/3 END | `../smp4-validation-final-busybox-musl-20260725-r2/` |
| glibc BusyBox | 3/3 END | `../smp4-validation-final-busybox-glibc-20260725-r2/` |
| musl lmbench | 3/3 END | `../smp4-validation-final-lmbench-musl-20260725-r2/` |
| glibc lmbench | 3/3 END | `../smp4-validation-final-lmbench-glibc-20260725-r2/` |
| musl libc-test | 3/3; every round 217/217 Pass | `../smp4-validation-final-libctest-musl-20260725-r2/` |
| glibc libc-test | 3/3 complete 217-case runs | `../smp4-validation-final-libctest-glibc-20260725-r2/` |
| diagnostics musl libc-test | 1/1; 217/217 Pass | `../smp4-validation-official-libctest-musl-diagnostics-20260725-r2/` |
| musl pthread_cond static+dynamic | 20/20 cold boots | `../smp4-validation-libctest-musl-pthread-cond-both-20260725-r2/` |
| Four-hart directed after iperf adaptation | 20/20 | `directed-after-iperf-r2/` |
| musl iperf official six-case suite | 3/3 END; 18/18 cases success | `iperf-musl-final-r2/` |
| glibc iperf official six-case suite | 3/3 END; 18/18 cases success | `iperf-glibc-final-r2/` |
| diagnostics musl iperf | 1/1 END; 6/6 cases success | `iperf-musl-diagnostics-r2/` |
| BusyBox after iperf adaptation | musl 1/1, glibc 1/1 END | `busybox-*-after-iperf-r2/` |
| lmbench after iperf adaptation | musl 1/1, glibc 1/1 END | `lmbench-*-after-iperf-r2/` |
| libc-test after iperf adaptation | musl 1/1, glibc 1/1 END | `libctest-*-after-iperf-r2/` |

No final BusyBox/lmbench/musl-libc/iperf log contains an unexpected `SKIP`,
panic, segmentation fault, scheduler-stall, or lockdep marker. Every iperf
round contains exactly the official BASIC/PARALLEL/REVERSE TCP and UDP six-case
success set followed by the natural group `END`. The glibc runs of the
musl-oriented libc-test complete naturally but retain stable cross-libc
semantic failures; the exact 178 Pass/39 FAIL breakdown is recorded in
`../../documents/四核SMP适配与死锁修复全记录.md`.
