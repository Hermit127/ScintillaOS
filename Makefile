# Building
export LOG ?= error
ARCH ?= riscv64

ifeq ($(ARCH), x86_64)
  TARGET := x86_64-unknown-none
  QEMU_EXEC += qemu-system-x86_64 \
				-machine q35 \
				-kernel $(KERNEL_ELF) \
				-cpu IvyBridge-v2
  BUS := pci
  VIRTIO_BLK_BUS := virtio-mmio-bus.0
  VIRTIO_BLK_DEVICE := virtio-blk-device
else ifeq ($(ARCH), riscv64)
  TARGET := riscv64gc-unknown-none-elf
  QEMU_EXEC += qemu-system-$(ARCH) \
				-machine virt \
				-kernel $(KERNEL_BIN)
  VIRTIO_BLK_BUS := virtio-mmio-bus.0
  VIRTIO_BLK_DEVICE := virtio-blk-device
else ifeq ($(ARCH), aarch64)
  TARGET := aarch64-unknown-none-softfloat
  QEMU_EXEC += qemu-system-$(ARCH) \
				-cpu cortex-a72 \
				-machine virt \
				-kernel $(KERNEL_BIN)
  VIRTIO_BLK_BUS := virtio-mmio-bus.0
  VIRTIO_BLK_DEVICE := virtio-blk-device
else ifeq ($(ARCH), loongarch64)
  TARGET := loongarch64-unknown-none
  QEMU_EXEC += qemu-system-$(ARCH) -kernel $(KERNEL_ELF) -m 1G
  BUS := pci
  VIRTIO_BLK_BUS := virtio-mmio-bus.0
  VIRTIO_BLK_DEVICE := virtio-blk-pci
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

# ==================== Contest Submission Targets ====================

all: kernel-rv disk.img kernel-la
	@echo "===== Build complete ====="
	@ls -lh kernel-rv kernel-la disk.img 2>/dev/null || true

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

# ==================== Local Development Targets ====================

build: env fs-img $(KERNEL_BIN) 

env:
	(rustup target list | grep "$(TARGET) (installed)") || rustup target add $(TARGET)
	cargo install cargo-binutils
	rustup component add rust-src
	rustup component add llvm-tools-preview

$(KERNEL_BIN): kernel
	@$(OBJCOPY) $(KERNEL_ELF) --strip-all -O binary $@

fs-img: $(APPS)
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
	@mkdir -p $(ROOTFS_BIN_DIR) $(ROOTFS_MNT_DIR)
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
	@cp -f tmp/hello_riscv64_musl $(ROOTFS_BIN_DIR)/hello 2>/dev/null || true
	@cp -f tmp/busybox-musl $(ROOTFS_BIN_DIR)/busybox 2>/dev/null || true
	@cp -f tmp/busybox_cmd.txt $(ROOTFS_DIR)/busybox_cmd.txt 2>/dev/null || true
	@cp -f tmp/busybox_testcode.sh $(ROOTFS_DIR)/busybox_testcode.sh 2>/dev/null || true
	@dd if=/dev/zero of=$(FS_IMG) bs=1M count=128
	@mke2fs -t ext4 -b 4096 -F -d $(ROOTFS_DIR) $(FS_IMG)
	cp target/$(TARGET)/$(MODE)/fs.img fs-img.img

$(APPS):

kernel:
	@echo Platform: $(BOARD)
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
				-smp 1

# Uncomment for QEMU debug log:
# 				-D qemu.log -d in_asm,int,pcall,cpu_reset,guest_errors

# Single-disk mode (same disk on both bus 0 and bus 1, for local dev)
# ROOT_BLOCK_DEVICE uses bus 1, mount test & contest use bus 0
QEMU_ARGS_SINGLE := -drive file=$(FS_IMG),if=none,format=raw,id=x0,file.locking=off \
			-device $(VIRTIO_BLK_DEVICE),drive=x0,bus=$(VIRTIO_BLK_BUS) \
			-drive file=$(FS_IMG),if=none,format=raw,id=x1,file.locking=off \
			-device $(VIRTIO_BLK_DEVICE),drive=x1,bus=virtio-mmio-bus.1

# Dual-disk mode (test disk bus 0, disk.img bus 1, for contest)
QEMU_ARGS_DUAL := -drive file=$(TEST_DISK),if=none,format=raw,id=x0 \
			-device $(VIRTIO_BLK_DEVICE),drive=x0,bus=$(VIRTIO_BLK_BUS) \
			-drive file=disk.img,if=none,format=raw,id=x1 \
			-device $(VIRTIO_BLK_DEVICE),drive=x1,bus=virtio-mmio-bus.1

# Test disk path (user can override)
TEST_DISK ?= fs-img.img

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

.PHONY: all kernel-rv kernel-la disk.img build env kernel clean disasm disasm-vim run-inner run-dual fs-img gdbserver gdbclient
