# Four-hart RISC-V lmbench validation

- Kernel source base: `3bd02c7021b31a57fb9ce23c5af46ff4360738f1`
- QEMU topology: `virt`, 256 MiB, `-smp 4`, two VirtIO block devices
- Kernel image SHA-256: `47ad1d782b13996bc846c56f16dc1fd4f23b2c68da9d93e3401b009747b2a58a`
- Root image SHA-256: `352fd8195ff9e04d40ea4b25bbf0646402a16010bed234f7fcee0d4468ced933`
- Official test image SHA-256: `d997975db5c4d8b007faacf6f1fc55139473e7a30333ca926965bbf9b0321681`
- musl script SHA-256: `71c05d4d48431a3ef3efa41cd6874e77d383f9c74a11e6f64042a22ff3a6b753`
- glibc script SHA-256: `71c05d4d48431a3ef3efa41cd6874e77d383f9c74a11e6f64042a22ff3a6b753`
- musl `lmbench_all` SHA-256: `3a49367cf24e9e243d8b0b558c263b379a7539457e33a9e053cab09006890a26`
- glibc `lmbench_all` SHA-256: `d86c7ec376eb6c5f1065f81dddee04883a16435b8c16b800c514db6c65e5af54`

The official scripts were not edited, skipped, or supplied altered command
arguments. The `/code/lmbench_src/bin/build` directory and a plain `cp`
BusyBox applet were provided as runtime layout required by the official
wrappers. `ENOUGH=1000`, `TIMING_O=0.0`, and `LOOP_O=0.0` reduce calibration
duration only; script commands and their `-P`/`-N` arguments remain unchanged.

## Consecutive cold-boot results

1. `glibc-cold1.log`: natural lmbench `END`; harts 0, 1, 2, and 3 dispatched tasks.
2. `musl-cold2.log`: natural lmbench `END`; harts 0, 1, 2, and 3 dispatched tasks.
3. `glibc-cold2.log`: natural lmbench `END`; harts 0, 1, 2, and 3 dispatched tasks.

No log contains `Panicked`, `Operation not permitted`, `not found`, or a
lmbench `SKIP` marker.

## Directed regression results

- `lat_sig prot`: 20 consecutive passes.
- `lat_pipe -P 1 -N 1`: 20 consecutive passes.
- `lat_proc fork -N 1`: 20 consecutive passes.
- `lat_proc exec -N 1`: 20 consecutive passes.
- `lat_proc shell -N 1`: 30 consecutive passes.
- `lat_fs /var/tmp`: 20 consecutive passes after the scheduler/TaskLock retry fix.
- Original `lat_ctx -P 1 -s 32 2 4 8 16 24 32 64 96`: complete through 96 workers.
- `cpuinfotest`: `online mask 0xf`.
- `smpstress`: `online=0xf, observed=0xf`.
- `clonesmptest`: parent CPU0, child CPU2, remote TLB replacement passed.
- `affinitytest`: all four CPUs tested.
- `run_pipe_test`: five large-pipe iterations passed.

## Reproduction command

```sh
make build ARCH=riscv64
make run-dual-prebuilt \
  ARCH=riscv64 QEMU_SMP=4 QEMU_NET_HOSTFWD= \
  TEST_DISK=/path/to/lmbench-rv-official.img \
  ROOT_DISK=/path/to/fresh-root.img
```

Inside the guest, mount `/dev/vda` at `/mnt`, copy the selected libc directory
to `/code/lmbench_src/bin/build`, provide `cp` on `PATH`, `cd` to `/mnt/musl`
or `/mnt/glibc`, and execute the unchanged `lmbench_testcode.sh`.
