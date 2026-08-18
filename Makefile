# Building
export LOG ?= error
ARCH ?= riscv64
COMMA := ,
# Keep the baseline runnable.  Higher core counts are selected explicitly by
# the SMP run targets or with QEMU_SMP=<n>.
QEMU_SMP ?= 1
QEMU_CPU ?= max
SMP_DIAGNOSTICS ?= 0
OS_FEATURES_EXTRA ?=

ifeq ($(SMP_DIAGNOSTICS),1)
  OS_FEATURES_LIST := smp-diagnostics $(OS_FEATURES_EXTRA)
else
  OS_FEATURES_LIST := $(OS_FEATURES_EXTRA)
endif
ifneq ($(strip $(OS_FEATURES_LIST)),)
  OS_FEATURES := --features "$(strip $(OS_FEATURES_LIST))"
endif

ifeq ($(ARCH), x86_64)
  TARGET := x86_64-unknown-none
  QEMU_EXEC += qemu-system-x86_64 \
				-machine q35 \
				-kernel $(KERNEL_ELF) \
				-cpu IvyBridge-v2
  BUS := pci
  VIRTIO_BLK_BUS := virtio-mmio-bus.0
  VIRTIO_BLK_BUS2 := virtio-mmio-bus.1
  VIRTIO_BLK_DEVICE := virtio-blk-device
  QEMU_MEM := 256M
else ifeq ($(ARCH), riscv64)
  TARGET := riscv64gc-unknown-none-elf
  QEMU_EXEC += qemu-system-$(ARCH) \
				-machine virt \
				-cpu $(QEMU_CPU) \
				-kernel $(KERNEL_BIN)
  VIRTIO_BLK_BUS := virtio-mmio-bus.0
  VIRTIO_BLK_BUS2 := virtio-mmio-bus.1
  VIRTIO_BLK_DEVICE := virtio-blk-device
  QEMU_MEM := 256M
else ifeq ($(ARCH), aarch64)
  TARGET := aarch64-unknown-none-softfloat
  QEMU_EXEC += qemu-system-$(ARCH) \
				-cpu cortex-a72 \
				-machine virt \
				-kernel $(KERNEL_BIN)
  VIRTIO_BLK_BUS := virtio-mmio-bus.0
  VIRTIO_BLK_BUS2 := virtio-mmio-bus.1
  VIRTIO_BLK_DEVICE := virtio-blk-device
  QEMU_MEM := 256M
else ifeq ($(ARCH), loongarch64)
  TARGET := loongarch64-unknown-none
  QEMU_EXEC += qemu-system-$(ARCH) -kernel $(KERNEL_ELF)
  VIRTIO_BLK_BUS := pcie.0
  VIRTIO_BLK_BUS2 := pcie.0
  VIRTIO_BLK_DEVICE := virtio-blk-pci
  QEMU_MEM := 2G
else
  $(error "ARCH" must be one of "x86_64", "riscv64", "aarch64" or "loongarch64")
endif

MODE := release
KERNEL_ELF := target/$(TARGET)/$(MODE)/os
KERNEL_BIN := $(KERNEL_ELF).bin
DISASM_TMP := target/$(TARGET)/$(MODE)/asm
FS_IMG := target/$(TARGET)/$(MODE)/fs.img
ROOTFS_DIR := target/$(TARGET)/$(MODE)/rootfs
ROOTFS_BIN_DIR := $(ROOTFS_DIR)/bin
ROOTFS_MNT_DIR := $(ROOTFS_DIR)/mnt
OSCOMP_DIR ?= basic/user
OSCOMP_BIN_DIR := $(OSCOMP_DIR)/build/$(ARCH)
APPS := user/src/bin/*

# BOARD
BOARD := qemu
SBI ?= rustsbi
BOOTLOADER := ../bootloader/$(SBI)-$(BOARD).bin

# Building mode argument
ifeq ($(MODE), release)
	MODE_ARG := --release
endif

# KERNEL ENTRY
KERNEL_ENTRY_PA := 0x80200000

# Binutils
RUST_SYSROOT := $(shell rustc --print sysroot)
LLVM_TOOLS_DIR := $(RUST_SYSROOT)/lib/rustlib/$(shell rustc -vV | sed -n 's/^host: //p')/bin
OBJDUMP ?= $(if $(shell command -v rust-objdump 2>/dev/null),rust-objdump,$(LLVM_TOOLS_DIR)/llvm-objdump) --arch-name=riscv64
OBJCOPY ?= $(if $(shell command -v rust-objcopy 2>/dev/null),rust-objcopy,$(LLVM_TOOLS_DIR)/llvm-objcopy) --binary-architecture=riscv64

# Disassembly
DISASM ?= -x

# Run usertests or usershell
TEST ?=

# LTP 集成
LTP_DIR_DEFAULT := testsuits-for-oskernel/sdcard/riscv/musl
LTP_BIN_DIR ?= $(LTP_DIR_DEFAULT)
LTP_MODE ?= subset

# ==================== Contest Submission Targets ====================

prepare-vendor:
	@find vendor -name cargo-checksum.json -exec sh -c \
		'cp "$$1" "$$(dirname "$$1")/.cargo-checksum.json"' _ {} \;

.NOTPARALLEL: all
all: kernel-rv disk.img kernel-la disk-la.img
	@echo "===== Build complete ====="
	@ls -lh kernel-rv kernel-la disk.img disk-la.img 2>/dev/null || true

kernel-rv:
	@echo "===== Building kernel-rv (riscv64) ====="
	@$(MAKE) kernel ARCH=riscv64
	@$(OBJCOPY) target/riscv64gc-unknown-none-elf/release/os --strip-all -O binary target/riscv64gc-unknown-none-elf/release/os.bin
	@cp target/riscv64gc-unknown-none-elf/release/os kernel-rv
	@echo "kernel-rv ready (ELF)"

kernel-la:
	@echo "===== Building kernel-la (loongarch64) ====="
	@$(MAKE) kernel ARCH=loongarch64
	@cp target/loongarch64-unknown-none/release/os kernel-la
	@echo "kernel-la ready"

kernel-rv-final-diag:
	@$(MAKE) kernel-rv ARCH=riscv64 \
		OS_FEATURES_EXTRA="lockdep lock-stats scheduler-stall-diagnostics buildstorm-mm-diagnostics unknown-syscall-diagnostics"
	@cp kernel-rv kernel-rv-final-diag
	@echo "kernel-rv-final-diag ready (ELF)"

disk.img: fs-img
	@if [ -f $(FS_IMG) ]; then \
		cp $(FS_IMG) disk.img; \
		echo "disk.img ready ($(FS_IMG))"; \
	fi

disk-rv.img:
	@echo "===== Building disk-rv.img (riscv64) ====="
	@$(MAKE) fs-img ARCH=riscv64
	@cp target/riscv64gc-unknown-none-elf/release/fs.img disk-rv.img
	@echo "disk-rv.img ready"

disk-la.img:
	@echo "===== Building disk-la.img (loongarch64) ====="
	@$(MAKE) fs-img ARCH=loongarch64
	@cp target/loongarch64-unknown-none/release/fs.img disk-la.img
	@echo "disk-la.img ready"

# ==================== Local Development Targets ====================

build: env fs-img $(KERNEL_BIN) 

env:
	(rustup target list | grep "$(TARGET) (installed)") || rustup target add $(TARGET)
	(command -v rust-objcopy >/dev/null 2>&1 && command -v rust-objdump >/dev/null 2>&1) || \
		(test -x "$(LLVM_TOOLS_DIR)/llvm-objcopy" && test -x "$(LLVM_TOOLS_DIR)/llvm-objdump") || \
		(rustup component add llvm-tools-preview)
	rustup component add rust-src
	rustup component add llvm-tools-preview

$(KERNEL_BIN): kernel
	@$(OBJCOPY) $(KERNEL_ELF) --strip-all -O binary $@

fs-img: prepare-vendor $(APPS)
	@mkdir -p .cargo 2>/dev/null || true
	@cp -f cargo_config/config.toml .cargo/config.toml 2>/dev/null || true
	@$(MAKE) -C user build TARGET=$(TARGET) TEST=$(TEST)
	@if [ -d "$(OSCOMP_DIR)" ]; then \
		if [ ! -d "$(OSCOMP_BIN_DIR)" ] || [ -z "$$(ls -A $(OSCOMP_BIN_DIR) 2>/dev/null)" ]; then \
			$(MAKE) -C "$(OSCOMP_DIR)" ARCH=$(ARCH) clean binary; \
		else \
			echo "[fs-img] using pre-built OSCOMP binaries: $(OSCOMP_BIN_DIR)"; \
		fi; \
	fi
	@rm -f $(FS_IMG)
	@rm -rf $(ROOTFS_DIR)
	@mkdir -p $(ROOTFS_BIN_DIR) $(ROOTFS_MNT_DIR) $(ROOTFS_DIR)/tmp
	@mkdir -p $(ROOTFS_BIN_DIR)/mnt
ifdef LM_ONLY
	@touch $(ROOTFS_DIR)/lmbench_only
endif
ifdef LM_GLIBC_ONLY
	@touch $(ROOTFS_DIR)/lmbench_glibc_only
endif
ifdef IOZONE_ONLY
	@touch $(ROOTFS_DIR)/iozone_only
endif
ifdef IOZONE_GLIBC_ONLY
	@touch $(ROOTFS_DIR)/iozone_glibc_only
endif
ifdef LUA_ONLY
	@touch $(ROOTFS_DIR)/lua_only
endif
	@printf "Hi, this is a text file.\nsyscalls testing success!\n" > $(ROOTFS_DIR)/text.txt
	@for src in user/src/bin/*.rs; do \
		app=$$(basename $$src .rs); \
		cp target/$(TARGET)/release/$$app $(ROOTFS_BIN_DIR)/; \
	done
	@if [ -d "$(OSCOMP_BIN_DIR)" ]; then \
		for app in "$(OSCOMP_BIN_DIR)"/*; do \
			if [ -f "$$app" ] && [ ! -d "$$app" ]; then \
				cp "$$app" "$(ROOTFS_BIN_DIR)/"; \
			fi; \
		done; \
		if [ -f "$(OSCOMP_BIN_DIR)/text.txt" ]; then \
			cp "$(OSCOMP_BIN_DIR)/text.txt" "$(ROOTFS_DIR)/text.txt"; \
		fi; \
		mkdir -p "$(ROOTFS_MNT_DIR)"; \
	fi
	@if [ "$(ARCH)" = "riscv64" ]; then \
		cp -f tmp/busybox-musl $(ROOTFS_BIN_DIR)/busybox 2>/dev/null || true; \
		cp -f tmp/busybox-musl $(ROOTFS_DIR)/busybox 2>/dev/null || true; \
		cp -f tmp/busybox-glibc-riscv64 $(ROOTFS_BIN_DIR)/busybox-glibc 2>/dev/null || true; \
		cp -f tmp/busybox-glibc-riscv64 $(ROOTFS_DIR)/busybox-glibc 2>/dev/null || true; \
		cat testsuits-for-oskernel/scripts/busybox/busybox_cmd.txt > $(ROOTFS_DIR)/busybox_cmd.txt 2>/dev/null || cat tmp/busybox_cmd.txt > $(ROOTFS_DIR)/busybox_cmd.txt 2>/dev/null || true; \
		sed 's|\./busybox|./busybox-glibc|g' $(ROOTFS_DIR)/busybox_cmd.txt > $(ROOTFS_DIR)/busybox_glibc_cmd.txt 2>/dev/null || true; \
		printf '#!/bin/sh\n./busybox-glibc --install -s /bin 2>/dev/null\necho "#### OS COMP TEST GROUP START busybox-glibc ####"\nwhile IFS= read -r line; do case "$$line" in ""|\\#*) continue ;; esac; eval "./busybox-glibc $$line"; RTN=$$?; if [ $$RTN -ne 0 ] && [ "$$line" != "false" ]; then echo "testcase busybox $$line fail"; else echo "testcase busybox $$line success"; fi; done < ./busybox_glibc_cmd.txt\necho "#### OS COMP TEST GROUP END busybox-glibc ####"\n' > $(ROOTFS_DIR)/busybox_glibc_testcode.sh; \
		ln -sf busybox $(ROOTFS_BIN_DIR)/ls 2>/dev/null || true; \
		printf '#!/bin/sh\n./busybox --install -s /bin 2>/dev/null\necho "#### OS COMP TEST GROUP START busybox-musl ####"\nwhile IFS= read -r line; do case "$$line" in ""|\\#*) continue ;; esac; eval "./busybox $$line"; RTN=$$?; if [ $$RTN -ne 0 ] && [ "$$line" != "false" ]; then echo "testcase busybox $$line fail"; else echo "testcase busybox $$line success"; fi; done < ./busybox_cmd.txt\necho "#### OS COMP TEST GROUP END busybox-musl ####"\n' > $(ROOTFS_DIR)/busybox_testcode.sh; \
		cp -f tmp/netperf-musl-riscv64 $(ROOTFS_DIR)/netperf 2>/dev/null || true; \
		cp -f tmp/netserver-musl-riscv64 $(ROOTFS_DIR)/netserver 2>/dev/null || true; \
		mkdir -p $(ROOTFS_DIR)/lib 2>/dev/null || true; \
		cp -f tmp/ld-musl-riscv64.so.1 $(ROOTFS_DIR)/lib/ld-musl-riscv64.so.1 2>/dev/null || true; \
		ln -sf ld-musl-riscv64.so.1 $(ROOTFS_DIR)/lib/libc.so 2>/dev/null || true; \
	elif [ "$(ARCH)" = "loongarch64" ]; then \
		cp -f tmp/busybox-musl-loongarch64 $(ROOTFS_BIN_DIR)/busybox 2>/dev/null || true; \
		cp -f tmp/busybox-musl-loongarch64 $(ROOTFS_DIR)/busybox 2>/dev/null || true; \
		cp -f tmp/busybox-glibc-loongarch64 $(ROOTFS_BIN_DIR)/busybox-glibc 2>/dev/null || true; \
		cp -f tmp/busybox-glibc-loongarch64 $(ROOTFS_DIR)/busybox-glibc 2>/dev/null || true; \
		cat testsuits-for-oskernel/scripts/busybox/busybox_cmd.txt > $(ROOTFS_DIR)/busybox_cmd.txt 2>/dev/null || cat tmp/busybox_cmd.txt > $(ROOTFS_DIR)/busybox_cmd.txt 2>/dev/null || true; \
		sed 's|\./busybox|./busybox-glibc|g' $(ROOTFS_DIR)/busybox_cmd.txt > $(ROOTFS_DIR)/busybox_glibc_cmd.txt 2>/dev/null || true; \
		printf '#!/bin/sh\n./busybox-glibc --install -s /bin 2>/dev/null\necho "#### OS COMP TEST GROUP START busybox-glibc ####"\nwhile IFS= read -r line; do case "$$line" in ""|\\#*) continue ;; esac; eval "./busybox-glibc $$line"; RTN=$$?; if [ $$RTN -ne 0 ] && [ "$$line" != "false" ]; then echo "testcase busybox $$line fail"; else echo "testcase busybox $$line success"; fi; done < ./busybox_glibc_cmd.txt\necho "#### OS COMP TEST GROUP END busybox-glibc ####"\n' > $(ROOTFS_DIR)/busybox_glibc_testcode.sh; \
		ln -sf busybox $(ROOTFS_BIN_DIR)/ls 2>/dev/null || true; \
		printf '#!/bin/sh\n./busybox --install -s /bin 2>/dev/null\necho "#### OS COMP TEST GROUP START busybox-musl ####"\nwhile IFS= read -r line; do case "$$line" in ""|\\#*) continue ;; esac; eval "./busybox $$line"; RTN=$$?; if [ $$RTN -ne 0 ] && [ "$$line" != "false" ]; then echo "testcase busybox $$line fail"; else echo "testcase busybox $$line success"; fi; done < ./busybox_cmd.txt\necho "#### OS COMP TEST GROUP END busybox-musl ####"\n' > $(ROOTFS_DIR)/busybox_testcode.sh; \
	fi
	@mkdir -p $(ROOTFS_DIR)/etc $(ROOTFS_DIR)/sbin 2>/dev/null || true
	@if [ -f "$(ROOTFS_DIR)/lmbench_testcode.sh" ]; then \
		{ \
			printf '%s\n' '#!/bin/sh'; \
			printf '%s\n' 'cd /'; \
			printf '%s\n' 'export ENOUGH=50000'; \
			printf '%s\n' 'export TIMING_O=0.0'; \
			printf '%s\n' 'export LOOP_O=0.0'; \
			printf '%s\n' ''; \
			printf '%s\n' 'echo "#### OS COMP TEST GROUP START lmbench-musl ####"'; \
			printf '%s\n' ''; \
			printf '%s\n' 'echo latency measurements'; \
			printf '%s\n' '/lmbench_all lat_syscall -P 1 null'; \
			printf '%s\n' '/lmbench_all lat_syscall -P 1 read'; \
			printf '%s\n' '/lmbench_all lat_syscall -P 1 write'; \
			printf '%s\n' '/busybox mkdir -p /var/tmp'; \
			printf '%s\n' '/busybox touch /var/tmp/lmbench'; \
			printf '%s\n' '/lmbench_all lat_syscall -P 1 stat /var/tmp/lmbench'; \
			printf '%s\n' '/lmbench_all lat_syscall -P 1 fstat /var/tmp/lmbench'; \
			printf '%s\n' '/lmbench_all lat_syscall -P 1 open /var/tmp/lmbench'; \
			printf '%s\n' '/lmbench_all lat_select -n 100 -P 1 file'; \
			printf '%s\n' '/lmbench_all lat_sig -P 1 install'; \
			printf '%s\n' '/lmbench_all lat_sig -P 1 catch'; \
			printf '%s\n' '/lmbench_all lat_sig -P 1 prot /lat_sig'; \
			printf '%s\n' '/lmbench_all lat_pipe -P 1'; \
			printf '%s\n' '/lmbench_all lat_proc -P 1 fork'; \
			printf '%s\n' '/lmbench_all lat_proc -P 1 exec'; \
			printf '%s\n' '/busybox cp /hello /tmp/hello'; \
			printf '%s\n' '/lmbench_all lat_proc -P 1 shell'; \
			printf '%s\n' '/lmbench_all lmdd label="File /var/tmp/XXX write bandwidth:" of=/var/tmp/XXX move=1m fsync=1 print=3'; \
			printf '%s\n' '/lmbench_all lat_pagefault -P 1 /var/tmp/XXX'; \
			printf '%s\n' '/lmbench_all lat_mmap -P 1 512k /var/tmp/XXX'; \
			printf '%s\n' 'echo file system latency'; \
			printf '%s\n' '/lmbench_all lat_fs /var/tmp'; \
			printf '%s\n' 'echo Bandwidth measurements'; \
			printf '%s\n' '/lmbench_all bw_pipe -P 1'; \
			printf '%s\n' '/lmbench_all bw_file_rd -P 1 512k io_only /var/tmp/XXX'; \
			printf '%s\n' '/lmbench_all bw_file_rd -P 1 512k open2close /var/tmp/XXX'; \
			printf '%s\n' '/lmbench_all bw_mmap_rd -P 1 512k mmap_only /var/tmp/XXX'; \
			printf '%s\n' '/lmbench_all bw_mmap_rd -P 1 512k open2close /var/tmp/XXX'; \
			printf '%s\n' 'echo context switch overhead'; \
			printf '%s\n' '/lmbench_all lat_ctx -P 1 -s 32 2 4 8 16 24 32 64 96'; \
			printf '%s\n' ''; \
			printf '%s\n' '/busybox sync'; \
			printf '%s\n' 'echo "#### OS COMP TEST GROUP END lmbench-musl ####"'; \
		} > "$(ROOTFS_DIR)/lmbench_testcode.sh"; \
		cp "$(ROOTFS_DIR)/lmbench_testcode.sh" "$(ROOTFS_DIR)/lmbench_musl_testcode.sh"; \
	fi
	@if [ -f "$(ROOTFS_DIR)/lmbench_all-glibc" ]; then \
		sed \
			-e 's|#### OS COMP TEST GROUP START lmbench-musl ####|#### OS COMP TEST GROUP START lmbench-glibc ####|' \
			-e 's|#### OS COMP TEST GROUP END lmbench-musl ####|#### OS COMP TEST GROUP END lmbench-glibc ####|' \
			-e 's|/busybox cp /hello /tmp/hello|/busybox cp /hello-glibc /tmp/hello|' \
			-e 's|/busybox|/busybox-glibc|g' \
			-e 's|/lmbench_all|/lmbench_all-glibc|g' \
			-e 's|/lat_sig|/lat_sig-glibc|g' \
			"$(ROOTFS_DIR)/lmbench_testcode.sh" > "$(ROOTFS_DIR)/lmbench_glibc_testcode.sh"; \
	fi
	@if [ -f "$(ROOTFS_BIN_DIR)/iozone" ]; then \
		{ \
			printf '%s\n' '#!/bin/sh'; \
			printf '%s\n' 'echo "#### OS COMP TEST GROUP START iozone-musl ####"'; \
			printf '%s\n' 'echo iozone throughput write/read measurements'; \
			printf '%s\n' '/bin/iozone -i 0 -i 1 -r 1k -s 1m 2>&1; echo "iozone exit: $$?"'; \
			printf '%s\n' 'echo iozone throughput random-read measurements'; \
			printf '%s\n' '/bin/iozone -i 0 -i 2 -r 1k -s 1m 2>&1'; \
			printf '%s\n' 'echo iozone throughput read-backwards measurements'; \
			printf '%s\n' '/bin/iozone -i 0 -i 3 -r 1k -s 1m 2>&1'; \
			printf '%s\n' 'echo iozone throughput stride-read measurements'; \
			printf '%s\n' '/bin/iozone -i 0 -i 5 -r 1k -s 1m 2>&1'; \
			printf '%s\n' 'echo iozone throughput fwrite/fread measurements'; \
			printf '%s\n' '/bin/iozone -i 6 -i 7 -r 1k -s 1m 2>&1'; \
			printf '%s\n' 'echo iozone throughput pwrite/pread measurements'; \
			printf '%s\n' '/bin/iozone -i 9 -i 10 -r 1k -s 1m 2>&1'; \
			printf '%s\n' 'echo iozone throughtput pwritev/preadv measurements'; \
			printf '%s\n' '/bin/iozone -i 11 -i 12 -r 1k -s 1m 2>&1'; \
			printf '%s\n' 'echo "#### OS COMP TEST GROUP END iozone-musl ####"'; \
		} > "$(ROOTFS_DIR)/iozone_testcode.sh"; \
		sed \
			-e 's|iozone-musl|iozone-glibc|g' \
			-e 's|/bin/iozone |/bin/iozone-glibc |g' \
			"$(ROOTFS_DIR)/iozone_testcode.sh" > "$(ROOTFS_DIR)/iozone_glibc_testcode.sh"; \
	fi
	@chmod +x $(ROOTFS_DIR)/hello $(ROOTFS_DIR)/lat_sig $(ROOTFS_DIR)/hello-glibc $(ROOTFS_DIR)/lat_sig-glibc $(ROOTFS_DIR)/lmbench_testcode.sh $(ROOTFS_DIR)/lmbench_glibc_testcode.sh $(ROOTFS_DIR)/iozone_testcode.sh $(ROOTFS_DIR)/iozone_glibc_testcode.sh 2>/dev/null || true
	@cp -f tmp/debug_test.sh $(ROOTFS_DIR)/debug_test.sh 2>/dev/null || true
	@cp -f tmp/run_all.sh $(ROOTFS_DIR)/run_all.sh 2>/dev/null || true
	@cp -f tmp/contest.sh $(ROOTFS_DIR)/contest.sh 2>/dev/null || true
	@cp -f tmp/basic_testcode.sh $(ROOTFS_DIR)/basic_testcode.sh 2>/dev/null || true
	@cp -f tmp/init.sh $(ROOTFS_DIR)/etc/init.sh 2>/dev/null || true
	@cp -f tmp/netperf_testcode.sh $(ROOTFS_DIR)/netperf_testcode.sh 2>/dev/null || \
		cp -f testsuits-for-oskernel/scripts/netperf/netperf_testcode.sh $(ROOTFS_DIR)/netperf_testcode.sh 2>/dev/null || true
	@cp -f target/$(TARGET)/release/contest_runner $(ROOTFS_DIR)/contest_runner 2>/dev/null || true
	@echo "===== LTP integration (mode: $(LTP_MODE)) ====="
ifeq ($(LTP_MODE), full)
	@touch $(ROOTFS_DIR)/ltp_full
endif
ifeq ($(LTP_MODE), subset)
	@if [ -f tmp/ltp_testcode_subset.sh ]; then \
		cp -f tmp/ltp_testcode_subset.sh $(ROOTFS_DIR)/ltp_testcode.sh; \
	else \
		echo "  LTP subset script not found, skipped"; \
	fi
else
	@if [ -f tmp/ltp_testcode.sh ]; then \
		cp -f tmp/ltp_testcode.sh $(ROOTFS_DIR)/ltp_testcode.sh; \
	else \
		echo "  LTP script not found, skipped"; \
	fi
endif

	@mkdir -p $(ROOTFS_DIR)/ltp/testcases/bin
	@if [ -d "$(LTP_BIN_DIR)/ltp/testcases/bin" ]; then \
		for f in $(LTP_BIN_DIR)/ltp/testcases/bin/*; do \
			if [ -f "$$f" ] && [ -x "$$f" ]; then \
				cp "$$f" $(ROOTFS_DIR)/ltp/testcases/bin/; \
			fi; \
		done; \
		echo "  LTP binaries copied: $$(ls $(ROOTFS_DIR)/ltp/testcases/bin/ | wc -l) files"; \
	else \
		echo "  LTP directory not found: $(LTP_BIN_DIR)/ltp/testcases/bin (build LTP first in Docker)"; \
	fi
	@echo "  runtime-dispatch initproc enabled for preliminary/final test disks"
	@dd if=/dev/zero of=$(FS_IMG) bs=1M count=512
	@mke2fs -t ext4 -b 4096 -F -O ^metadata_csum,^64bit,^orphan_file -d $(ROOTFS_DIR) $(FS_IMG)
	@e2fsck -fy $(FS_IMG) 2>/dev/null || true
	@if [ "$(ARCH)" = "x86_64" ]; then \
		cp $(FS_IMG) fs-img.img; \
	fi

$(APPS):

kernel: prepare-vendor
	@echo Platform: $(BOARD)
	@mkdir -p .cargo 2>/dev/null || true
	@cp -f cargo_config/config.toml .cargo/config.toml 2>/dev/null || true
	@$(MAKE) -C user build TARGET=$(TARGET)
	@cargo build --release --target $(TARGET) $(OS_FEATURES)

clean:
	@cargo clean

disasm: kernel
	@$(OBJDUMP) $(DISASM) $(KERNEL_ELF) | less

disasm-vim: kernel
	@$(OBJDUMP) $(DISASM) $(KERNEL_ELF) > $(DISASM_TMP)
	@vim $(DISASM_TMP)
	@rm $(DISASM_TMP)

run: run-inner

run-smp4:
	@$(MAKE) run-inner ARCH=riscv64 QEMU_SMP=4 SMP_DIAGNOSTICS=$(SMP_DIAGNOSTICS)

run-smp8:
	@$(MAKE) run-inner ARCH=riscv64 QEMU_SMP=8 SMP_DIAGNOSTICS=$(SMP_DIAGNOSTICS)

run-dual-smp4:
	@$(MAKE) run-dual ARCH=riscv64 QEMU_SMP=4 SMP_DIAGNOSTICS=$(SMP_DIAGNOSTICS) TEST_DISK="$(TEST_DISK)" ROOT_DISK="$(ROOT_DISK)"

run-dual-smp4-prebuilt:
	@$(MAKE) run-dual-prebuilt ARCH=riscv64 QEMU_SMP=4 SMP_DIAGNOSTICS=$(SMP_DIAGNOSTICS) TEST_DISK="$(TEST_DISK)" ROOT_DISK="$(ROOT_DISK)"

run-la-smp2:
	@$(MAKE) run-inner ARCH=loongarch64 QEMU_SMP=2 SMP_DIAGNOSTICS=$(SMP_DIAGNOSTICS)

run-la-smp8:
	@$(MAKE) run-inner ARCH=loongarch64 QEMU_SMP=8 SMP_DIAGNOSTICS=$(SMP_DIAGNOSTICS)

run-la-dual-smp2:
	@$(MAKE) run-dual ARCH=loongarch64 QEMU_SMP=2 SMP_DIAGNOSTICS=$(SMP_DIAGNOSTICS) TEST_DISK="$(TEST_DISK)" ROOT_DISK=disk-la.img

run-la-dual-smp2-prebuilt:
	@$(MAKE) run-dual-prebuilt ARCH=loongarch64 QEMU_SMP=2 SMP_DIAGNOSTICS=$(SMP_DIAGNOSTICS) TEST_DISK="$(TEST_DISK)" ROOT_DISK=disk-la.img

QEMU_EXEC += -nographic \
				-m $(QEMU_MEM) \
				-smp $(QEMU_SMP)

# Uncomment for QEMU debug log:
# 				-D qemu.log -d in_asm,int,pcall,cpu_reset,guest_errors

# Single-disk mode (same disk on both bus 0 and bus 1, for local dev)
# ROOT_BLOCK_DEVICE uses bus 1, mount test & contest use bus 0
QEMU_ARGS_SINGLE := -drive file=$(FS_IMG),if=none,format=raw,id=x0,file.locking=off \
			-device $(VIRTIO_BLK_DEVICE),drive=x0,bus=$(VIRTIO_BLK_BUS) \
			-drive file=$(FS_IMG),if=none,format=raw,id=x1,file.locking=off \
			-device $(VIRTIO_BLK_DEVICE),drive=x1,bus=$(VIRTIO_BLK_BUS2)

# External contest test disk path (user can override).  This name is never
# produced by the build, so make all cannot overwrite a downloaded test image.
TEST_DISK ?= testcases.img
ROOT_DISK ?= $(if $(filter loongarch64,$(ARCH)),disk-la.img,disk.img)
ON_SITE_2025_DISK ?= ../alpine-linux-riscv64-ext4fs.img
ON_SITE_2025_ARTIFACT_DIR ?= artifacts/onsite2025
FINAL_2026_RV_IMAGE ?= testsuits-for-oskernel-final/image/sdcard-rv-pub.img
FINAL_2026_LA_IMAGE ?= testsuits-for-oskernel-final/image/sdcard-la-pub.img
FINAL_2026_MODE ?= all
FINAL_2026_MEMORY ?= 8G
FINAL_2026_ARTIFACT_DIR ?= artifacts/final2026
PYTHON_ONSITE_MODE ?= all
PYTHON_ONSITE_SMP ?= 4
PYTHON_ONSITE_MEMORY ?= 2G
PYTHON_ONSITE_ARTIFACT_DIR ?= artifacts/onsite-python
PYTHON_ONSITE_PROXY ?= http://10.0.2.2:27898
TOOLCHAIN_ONSITE_MODE ?= all
TOOLCHAIN_ONSITE_SMP ?= 4
TOOLCHAIN_ONSITE_MEMORY ?= 2G
TOOLCHAIN_ONSITE_ARTIFACT_DIR ?= artifacts/onsite-toolchain
TOOLCHAIN_ONSITE_PROXY ?= http://10.0.2.2:27898

# Dual-disk mode (test disk bus 0, arch-matched root disk bus 1)
QEMU_ARGS_DUAL := -drive file=$(TEST_DISK),if=none,format=raw,id=x0 \
			-device $(VIRTIO_BLK_DEVICE),drive=x0$(if $(filter loongarch64,$(ARCH)),,$(COMMA)bus=$(VIRTIO_BLK_BUS)) \
			-drive file=$(ROOT_DISK),if=none,format=raw,id=x1 \
			-device $(VIRTIO_BLK_DEVICE),drive=x1$(if $(filter loongarch64,$(ARCH)),,$(COMMA)bus=$(VIRTIO_BLK_BUS2))

# virtio-net network configuration (SLIRP user-mode)
QEMU_NET_HOSTFWD ?= hostfwd=tcp::5555-:5001$(COMMA)hostfwd=tcp::5556-:5201
QEMU_NETDEV_USER := user$(COMMA)id=net0$(if $(QEMU_NET_HOSTFWD),$(COMMA)$(QEMU_NET_HOSTFWD))
ifeq ($(ARCH), riscv64)
  QEMU_NET_ARGS := -netdev $(QEMU_NETDEV_USER) \
			-device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2
else ifeq ($(ARCH), aarch64)
  QEMU_NET_ARGS := -netdev $(QEMU_NETDEV_USER) \
			-device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2
else ifeq ($(ARCH), loongarch64)
  QEMU_NET_ARGS := -netdev $(QEMU_NETDEV_USER) \
			-device virtio-net-pci,netdev=net0
else
  QEMU_NET_ARGS :=
endif

run-inner: build
	$(QEMU_EXEC) $(QEMU_ARGS_SINGLE) $(QEMU_NET_ARGS)

run-dual: build disk.img
	@echo "Running with dual-disk: $(TEST_DISK) + $(ROOT_DISK)"
	$(QEMU_EXEC) $(QEMU_ARGS_DUAL) $(QEMU_NET_ARGS)

run-dual-prebuilt:
	@test -f $(if $(filter loongarch64,$(ARCH)),$(KERNEL_ELF),$(KERNEL_BIN)) || { \
		echo "missing kernel image: $(if $(filter loongarch64,$(ARCH)),$(KERNEL_ELF),$(KERNEL_BIN))"; \
		echo "run make build ARCH=$(ARCH) first"; \
		exit 1; \
	}
	@test -f $(ROOT_DISK) || { echo "missing root disk: $(ROOT_DISK)"; exit 1; }
	@test -f $(TEST_DISK) || { echo "missing test disk: $(TEST_DISK)"; exit 1; }
	@echo "Running with prebuilt dual-disk: $(TEST_DISK) + $(ROOT_DISK)"
	$(QEMU_EXEC) $(QEMU_ARGS_DUAL) $(QEMU_NET_ARGS)

run-onsite-2025-rv: kernel-rv
	@ON_SITE_2025_DISK="$(ON_SITE_2025_DISK)" \
		ON_SITE_2025_ROOT_DISK="$(ROOT_DISK)" \
		ON_SITE_2025_ARTIFACT_DIR="$(ON_SITE_2025_ARTIFACT_DIR)" \
		ON_SITE_2025_SMP="$(QEMU_SMP)" \
		./scripts/run_onsite_2025_rv.sh

onsite-2025-proxy-relay:
	@python3 scripts/onsite_2025_proxy_relay.py

verify-onsite-2025-vim: kernel-rv
	@python3 scripts/verify_onsite_2025_vim.py \
		--base-image "$(ON_SITE_2025_DISK)" --root-disk "$(ROOT_DISK)" \
		--smp "$(QEMU_SMP)"

verify-onsite-2025-regressions: kernel-rv
	@python3 scripts/verify_onsite_2025_regressions.py \
		--base-image "$(ON_SITE_2025_DISK)" --root-disk "$(ROOT_DISK)" \
		--smp "$(QEMU_SMP)"

verify-onsite-2025-network: kernel-rv
	@python3 scripts/verify_onsite_2025_regressions.py --network-sockname \
		--base-image "$(ON_SITE_2025_DISK)" --root-disk "$(ROOT_DISK)" \
		--smp "$(QEMU_SMP)"

verify-onsite-2026-abi: kernel-rv
	@python3 scripts/verify_onsite_2025_regressions.py --abi-2026 \
		--base-image "$(ON_SITE_2025_DISK)" --root-disk "$(ROOT_DISK)" \
		--smp "$(QEMU_SMP)"

verify-onsite-python-rv: kernel-rv disk.img
	@python3 scripts/verify_onsite_python.py --arch riscv64 \
		--mode "$(PYTHON_ONSITE_MODE)" --test-image "$(FINAL_2026_RV_IMAGE)" \
		--root-image disk.img --kernel kernel-rv --smp "$(PYTHON_ONSITE_SMP)" \
		--memory "$(PYTHON_ONSITE_MEMORY)" --proxy "$(PYTHON_ONSITE_PROXY)" \
		--artifact-root "$(PYTHON_ONSITE_ARTIFACT_DIR)"

verify-onsite-python-core: kernel-rv disk.img
	@$(MAKE) verify-onsite-python-rv PYTHON_ONSITE_MODE=core

verify-onsite-python-fs: kernel-rv disk.img
	@$(MAKE) verify-onsite-python-rv PYTHON_ONSITE_MODE=fs

verify-onsite-python-concurrency: kernel-rv disk.img
	@$(MAKE) verify-onsite-python-rv PYTHON_ONSITE_MODE=concurrency

verify-onsite-python-network: kernel-rv disk.img
	@$(MAKE) verify-onsite-python-rv PYTHON_ONSITE_MODE=network

verify-onsite-python-all: kernel-rv disk.img
	@$(MAKE) verify-onsite-python-rv PYTHON_ONSITE_MODE=all

verify-onsite-python-la-docker:
	@test -f kernel-la || { echo "missing kernel-la; build it in the official Docker environment first"; exit 1; }
	@test -f disk-la.img || { echo "missing disk-la.img; build it in the official Docker environment first"; exit 1; }
	@sh scripts/verify_onsite_python_docker.sh --arch loongarch64 \
		--mode "$(PYTHON_ONSITE_MODE)" --test-image "$(FINAL_2026_LA_IMAGE)" \
		--root-image disk-la.img --kernel kernel-la --smp 12 \
		--memory "$(PYTHON_ONSITE_MEMORY)" --proxy "$(PYTHON_ONSITE_PROXY)" \
		--artifact-root "$(PYTHON_ONSITE_ARTIFACT_DIR)"

verify-onsite-toolchain-rv: kernel-rv disk.img
	@python3 scripts/verify_onsite_toolchain.py --arch riscv64 \
		--mode "$(TOOLCHAIN_ONSITE_MODE)" --test-image "$(FINAL_2026_RV_IMAGE)" \
		--root-image disk.img --kernel kernel-rv --smp "$(TOOLCHAIN_ONSITE_SMP)" \
		--memory "$(TOOLCHAIN_ONSITE_MEMORY)" --proxy "$(TOOLCHAIN_ONSITE_PROXY)" \
		--artifact-root "$(TOOLCHAIN_ONSITE_ARTIFACT_DIR)"

verify-onsite-toolchain-versions: kernel-rv disk.img
	@$(MAKE) verify-onsite-toolchain-rv TOOLCHAIN_ONSITE_MODE=versions

verify-onsite-toolchain-build: kernel-rv disk.img
	@$(MAKE) verify-onsite-toolchain-rv TOOLCHAIN_ONSITE_MODE=build

verify-onsite-toolchain-cargo: kernel-rv disk.img
	@$(MAKE) verify-onsite-toolchain-rv TOOLCHAIN_ONSITE_MODE=cargo

verify-onsite-toolchain-python: kernel-rv disk.img
	@$(MAKE) verify-onsite-toolchain-rv TOOLCHAIN_ONSITE_MODE=python

verify-onsite-toolchain-curl: kernel-rv disk.img
	@$(MAKE) verify-onsite-toolchain-rv TOOLCHAIN_ONSITE_MODE=curl

verify-onsite-toolchain-all: kernel-rv disk.img
	@$(MAKE) verify-onsite-toolchain-rv TOOLCHAIN_ONSITE_MODE=all

verify-onsite-toolchain-la-docker:
	@test -f kernel-la || { echo "missing kernel-la; build it in the official Docker environment first"; exit 1; }
	@test -f disk-la.img || { echo "missing disk-la.img; build it in the official Docker environment first"; exit 1; }
	@sh scripts/verify_onsite_toolchain_docker.sh --arch loongarch64 \
		--mode "$(TOOLCHAIN_ONSITE_MODE)" --test-image "$(FINAL_2026_LA_IMAGE)" \
		--root-image disk-la.img --kernel kernel-la --smp 12 \
		--memory "$(TOOLCHAIN_ONSITE_MEMORY)" --proxy "$(TOOLCHAIN_ONSITE_PROXY)" \
		--artifact-root "$(TOOLCHAIN_ONSITE_ARTIFACT_DIR)"

run-final-2026-rv: kernel-rv disk.img
	@python3 scripts/run_final_2026.py --arch riscv64 \
		--mode "$(FINAL_2026_MODE)" --test-image "$(FINAL_2026_RV_IMAGE)" \
		--root-image disk.img --kernel kernel-rv --smp 8 \
		--memory "$(FINAL_2026_MEMORY)" --artifact-root "$(FINAL_2026_ARTIFACT_DIR)"

run-final-2026-la: kernel-la disk-la.img
	@python3 scripts/run_final_2026.py --arch loongarch64 \
		--mode "$(FINAL_2026_MODE)" --test-image "$(FINAL_2026_LA_IMAGE)" \
		--root-image disk-la.img --kernel kernel-la --smp 12 \
		--memory "$(FINAL_2026_MEMORY)" --artifact-root "$(FINAL_2026_ARTIFACT_DIR)"

run-final-2026-la-docker:
	@test -f kernel-la || { echo "missing kernel-la; build it in the official Docker environment first"; exit 1; }
	@test -f disk-la.img || { echo "missing disk-la.img; build it in the official Docker environment first"; exit 1; }
	@sh scripts/run_final_2026_docker.sh --arch loongarch64 \
		--mode "$(FINAL_2026_MODE)" --test-image "$(FINAL_2026_LA_IMAGE)" \
		--root-image disk-la.img --kernel kernel-la --smp 12 \
		--memory "$(FINAL_2026_MEMORY)" --artifact-root "$(FINAL_2026_ARTIFACT_DIR)"

verify-loongarch12-boot-docker:
	@test -f kernel-la || { echo "missing kernel-la; build it in the official Docker environment first"; exit 1; }
	@test -f disk-la.img || { echo "missing disk-la.img; build it in the official Docker environment first"; exit 1; }
	@sh scripts/validate_loongarch12_boot_docker.sh \
		--kernel kernel-la --root-image disk-la.img --smp 12 --memory 2G

verify-loongarch12-directed-docker:
	@test -f kernel-la || { echo "missing kernel-la; build it in the official Docker environment first"; exit 1; }
	@test -f disk-la.img || { echo "missing disk-la.img; build it in the official Docker environment first"; exit 1; }
	@sh scripts/validate_loongarch12_boot_docker.sh \
		--kernel kernel-la --root-image disk-la.img --smp 12 --memory 2G \
		--suite directed

verify-loongarch12-pre2025-docker:
	@test -n "$(TEST_DISK)" || { echo "usage: make $@ TEST_DISK=/path/to/sdcard-la.img"; exit 1; }
	@test -f "$(TEST_DISK)" || { echo "missing TEST_DISK=$(TEST_DISK)"; exit 1; }
	@test -f kernel-la || { echo "missing kernel-la; build it in the official Docker environment first"; exit 1; }
	@test -f disk-la.img || { echo "missing disk-la.img; build it in the official Docker environment first"; exit 1; }
	@sh scripts/validate_loongarch12_boot_docker.sh \
		--kernel kernel-la --root-image disk-la.img --test-image "$(TEST_DISK)" \
		--smp 12 --memory 2G --suite matrix

verify-loongarch12-signal-docker:
	@test -n "$(TEST_DISK)" || { echo "usage: make $@ TEST_DISK=/path/to/sdcard-la.img"; exit 1; }
	@test -f "$(TEST_DISK)" || { echo "missing TEST_DISK=$(TEST_DISK)"; exit 1; }
	@test -f kernel-la || { echo "missing kernel-la; build it in the official Docker environment first"; exit 1; }
	@test -f disk-la.img || { echo "missing disk-la.img; build it in the official Docker environment first"; exit 1; }
	@sh scripts/validate_loongarch12_boot_docker.sh \
		--kernel kernel-la --root-image disk-la.img --test-image "$(TEST_DISK)" \
		--smp 12 --memory 2G --suite signal

verify-onsite-2025-git-clone: kernel-rv
	@python3 scripts/verify_onsite_2025_git_clone.py \
		--base-image "$(ON_SITE_2025_DISK)" --root-disk "$(ROOT_DISK)" \
		--smp "$(QEMU_SMP)"

debug: build
	@tmux new-session -d \
		"$(QEMU_EXEC) $(QEMU_ARGS_SINGLE) $(QEMU_NET_ARGS) -s -S" && \
		tmux split-window -h "gdb -ex 'file $(KERNEL_ELF)' -ex 'target remote localhost:1234'" && \
		tmux -2 attach-session -d


gdbserver: build
	@$(QEMU_EXEC) $(QEMU_ARGS_SINGLE) $(QEMU_NET_ARGS) -s -S

gdbclient:
	@gdb -ex 'file $(KERNEL_ELF)' -ex 'set arch riscv:rv64' -ex 'target remote localhost:1234'

.PHONY: all prepare-vendor kernel-rv kernel-la kernel-rv-final-diag disk.img disk-rv.img disk-la.img build env kernel clean disasm disasm-vim run run-smp4 run-smp8 run-la-smp2 run-la-smp8 run-inner run-dual run-dual-smp4 run-la-dual-smp2 run-dual-prebuilt run-dual-smp4-prebuilt run-la-dual-smp2-prebuilt run-onsite-2025-rv onsite-2025-proxy-relay verify-onsite-2025-vim verify-onsite-2025-regressions verify-onsite-2025-network verify-onsite-2025-git-clone verify-onsite-2026-abi verify-onsite-python-rv verify-onsite-python-core verify-onsite-python-fs verify-onsite-python-concurrency verify-onsite-python-network verify-onsite-python-all verify-onsite-python-la-docker verify-onsite-toolchain-rv verify-onsite-toolchain-versions verify-onsite-toolchain-build verify-onsite-toolchain-cargo verify-onsite-toolchain-python verify-onsite-toolchain-curl verify-onsite-toolchain-all verify-onsite-toolchain-la-docker run-final-2026-rv run-final-2026-la run-final-2026-la-docker verify-loongarch12-boot-docker verify-loongarch12-directed-docker verify-loongarch12-pre2025-docker verify-loongarch12-signal-docker fs-img gdbserver gdbclient
