# Building
export LOG ?= error
ARCH ?= riscv64
COMMA := ,

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
OBJDUMP := rust-objdump --arch-name=riscv64
OBJCOPY := rust-objcopy --binary-architecture=riscv64

# Disassembly
DISASM ?= -x

# Run usertests or usershell
TEST ?=

# LTP 集成
LTP_DIR_DEFAULT := testsuits-for-oskernel/sdcard/riscv/musl
LTP_BIN_DIR ?= $(LTP_DIR_DEFAULT)
LTP_MODE ?= subset

# ==================== Contest Submission Targets ====================

all: kernel-rv disk.img disk-rv.img kernel-la disk-la.img
	@echo "===== Build complete ====="
	@ls -lh kernel-rv kernel-la disk.img disk-rv.img disk-la.img 2>/dev/null || true

kernel-rv:
	@echo "===== Building kernel-rv (riscv64) ====="
	@$(MAKE) kernel ARCH=riscv64
	@$(OBJCOPY) target/riscv64gc-unknown-none-elf/release/os --strip-all -O binary kernel-rv
	@echo "kernel-rv ready"

kernel-la:
	@echo "===== Building kernel-la (loongarch64) ====="
	@$(MAKE) fs-img ARCH=loongarch64
	@$(MAKE) kernel ARCH=loongarch64
	@cp target/loongarch64-unknown-none/release/os kernel-la
	@echo "kernel-la ready"

disk.img: fs-img
	@if [ -f $(FS_IMG) ]; then \
		cp $(FS_IMG) disk.img; \
		echo "disk.img ready ($(FS_IMG))"; \
	fi

disk-rv.img:
	@echo "===== Building disk-rv.img (riscv64) ====="
	@make fs-img ARCH=riscv64
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
	(command -v rust-objcopy >/dev/null 2>&1 && command -v rust-objdump >/dev/null 2>&1) || cargo install cargo-binutils
	rustup component add rust-src
	rustup component add llvm-tools-preview

$(KERNEL_BIN): kernel
	@$(OBJCOPY) $(KERNEL_ELF) --strip-all -O binary $@

fs-img: $(APPS)
	@mkdir -p .cargo 2>/dev/null || true
	@cp -f cargo_config/config.toml .cargo/config.toml 2>/dev/null || true
	@cd user && make build TARGET=$(TARGET) TEST=$(TEST)
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
ifdef LM_ONLY
	@touch $(ROOTFS_DIR)/lmbench_only
endif
ifdef LM_GLIBC_ONLY
	@touch $(ROOTFS_DIR)/lmbench_glibc_only
endif
	@mkdir -p $(ROOTFS_DIR)/musl $(ROOTFS_DIR)/glibc
	@cp -f tmp/busybox-musl $(ROOTFS_DIR)/musl/busybox 2>/dev/null || true
	@cp -f tmp/busybox-glibc-riscv64 $(ROOTFS_DIR)/glibc/busybox 2>/dev/null || true
	@cp -f testsuits-for-oskernel/lua/src/lua $(ROOTFS_DIR)/musl/lua 2>/dev/null || true
	@cp -f testsuits-for-oskernel/lua/src/lua $(ROOTFS_DIR)/glibc/lua 2>/dev/null || true
	@for g in basic busybox lua; do \
		case $$g in \
			basic) \
				echo "#!/bin/sh\necho \"#### OS COMP TEST GROUP START basic-musl ####\"\ncd /bin\n./run-all.sh\necho \"#### OS COMP TEST GROUP END basic-musl ####\"" > $(ROOTFS_DIR)/musl/basic_testcode.sh ;; \
			busybox) \
				cat testsuits-for-oskernel/scripts/busybox/busybox_testcode.sh > $(ROOTFS_DIR)/musl/busybox_testcode.sh 2>/dev/null || true ;; \
			lua) \
				echo "#!/bin/sh\necho \"#### OS COMP TEST GROUP START lua-musl ####\"\nfor f in strings sort sin30 round_num max_min file_io remove date random; do ./lua ./$$f.lua && echo \"testcase lua $$f success\" || echo \"testcase lua $$f fail\"; done\necho \"#### OS COMP TEST GROUP END lua-musl ####\"" > $(ROOTFS_DIR)/musl/lua_testcode.sh ;; \
		esac; \
		case $$g in \
			basic) \
				echo "#!/bin/sh\necho \"#### OS COMP TEST GROUP START basic-glibc ####\"\ncd /bin\n./run-all.sh\necho \"#### OS COMP TEST GROUP END basic-glibc ####\"" > $(ROOTFS_DIR)/glibc/basic_testcode.sh ;; \
			busybox) \
				sed 's|\./busybox|./busybox|g' testsuits-for-oskernel/scripts/busybox/busybox_testcode.sh > $(ROOTFS_DIR)/glibc/busybox_testcode.sh 2>/dev/null || true ;; \
			lua) \
				echo "#!/bin/sh\necho \"#### OS COMP TEST GROUP START lua-glibc ####\"\nfor f in strings sort sin30 round_num max_min file_io remove date random; do ./lua ./$$f.lua && echo \"testcase lua $$f success\" || echo \"testcase lua $$f fail\"; done\necho \"#### OS COMP TEST GROUP END lua-glibc ####\"" > $(ROOTFS_DIR)/glibc/lua_testcode.sh ;; \
		esac; \
	done
	@for g in lmbench iozone; do \
		echo "#!/bin/sh\necho \"#### OS COMP TEST GROUP START $$g-musl ####\"\necho \"testcase $$g skipped (not available)\"\necho \"#### OS COMP TEST GROUP END $$g-musl ####\"" > $(ROOTFS_DIR)/musl/$${g}_testcode.sh; \
		echo "#!/bin/sh\necho \"#### OS COMP TEST GROUP START $$g-glibc ####\"\necho \"testcase $$g skipped (not available)\"\necho \"#### OS COMP TEST GROUP END $$g-glibc ####\"" > $(ROOTFS_DIR)/glibc/$${g}_testcode.sh; \
	done
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
	@echo "  main branch keeps shell initproc; ltp_runner remains available as /bin/ltp_runner"
	@dd if=/dev/zero of=$(FS_IMG) bs=1M count=512
	@mke2fs -t ext4 -b 4096 -F -O ^metadata_csum,^64bit,^orphan_file -d $(ROOTFS_DIR) $(FS_IMG)
	@e2fsck -fy $(FS_IMG) 2>/dev/null || true
	cp target/$(TARGET)/$(MODE)/fs.img fs-img.img

$(APPS):

kernel:
	@echo Platform: $(BOARD)
	@mkdir -p .cargo 2>/dev/null || true
	@cp -f cargo_config/config.toml .cargo/config.toml 2>/dev/null || true
	@cargo build --release --target $(TARGET)

clean:
	@cargo clean

disasm: kernel
	@$(OBJDUMP) $(DISASM) $(KERNEL_ELF) | less

disasm-vim: kernel
	@$(OBJDUMP) $(DISASM) $(KERNEL_ELF) > $(DISASM_TMP)
	@vim $(DISASM_TMP)
	@rm $(DISASM_TMP)

run: run-inner

QEMU_EXEC += -nographic \
				-m $(QEMU_MEM) \
				-smp 1

# Uncomment for QEMU debug log:
# 				-D qemu.log -d in_asm,int,pcall,cpu_reset,guest_errors

# Single-disk mode (same disk on both bus 0 and bus 1, for local dev)
# ROOT_BLOCK_DEVICE uses bus 1, mount test & contest use bus 0
QEMU_ARGS_SINGLE := -drive file=$(FS_IMG),if=none,format=raw,id=x0,file.locking=off \
			-device $(VIRTIO_BLK_DEVICE),drive=x0,bus=$(VIRTIO_BLK_BUS) \
			-drive file=$(FS_IMG),if=none,format=raw,id=x1,file.locking=off \
			-device $(VIRTIO_BLK_DEVICE),drive=x1,bus=$(VIRTIO_BLK_BUS2)

# Test disk path (user can override)
TEST_DISK ?= fs-img.img

# Dual-disk mode (test disk bus 0, disk.img bus 1, for contest)
QEMU_ARGS_DUAL := -drive file=$(TEST_DISK),if=none,format=raw,id=x0 \
			-device $(VIRTIO_BLK_DEVICE),drive=x0$(if $(filter loongarch64,$(ARCH)),,bus=$(VIRTIO_BLK_BUS)) \
			-drive file=disk.img,if=none,format=raw,id=x1 \
			-device $(VIRTIO_BLK_DEVICE),drive=x1$(if $(filter loongarch64,$(ARCH)),,bus=$(VIRTIO_BLK_BUS))

# virtio-net network configuration (SLIRP user-mode, riscv64/aarch64)
ifeq ($(ARCH), riscv64)
  QEMU_NET_ARGS := -netdev user,id=net0,hostfwd=tcp::5555-:5001,hostfwd=tcp::5556-:5201 \
			-device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2
else ifeq ($(ARCH), aarch64)
  QEMU_NET_ARGS := -netdev user,id=net0,hostfwd=tcp::5555-:5001,hostfwd=tcp::5556-:5201 \
			-device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2
else
  QEMU_NET_ARGS :=
endif

run-inner: build
	$(QEMU_EXEC) $(QEMU_ARGS_SINGLE) $(QEMU_NET_ARGS)

run-dual: build disk.img
	@echo "Running with dual-disk: $(TEST_DISK) + disk.img"
	$(QEMU_EXEC) $(QEMU_ARGS_DUAL) $(QEMU_NET_ARGS)

debug: build
	@tmux new-session -d \
		"$(QEMU_EXEC) $(QEMU_ARGS_SINGLE) $(QEMU_NET_ARGS) -s -S" && \
		tmux split-window -h "gdb -ex 'file $(KERNEL_ELF)' -ex 'target remote localhost:1234'" && \
		tmux -2 attach-session -d


gdbserver: build
	@$(QEMU_EXEC) $(QEMU_ARGS_SINGLE) $(QEMU_NET_ARGS) -s -S

gdbclient:
	@gdb -ex 'file $(KERNEL_ELF)' -ex 'set arch riscv:rv64' -ex 'target remote localhost:1234'

.PHONY: all kernel-rv kernel-la disk.img disk-rv.img disk-la.img build env kernel clean disasm disasm-vim run-inner run-dual fs-img gdbserver gdbclient
