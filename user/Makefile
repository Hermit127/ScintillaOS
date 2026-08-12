TARGET := riscv64gc-unknown-none-elf
MODE := release
APP_DIR := src/bin
TARGET_DIR := ../target/$(TARGET)/$(MODE)
APPS := $(wildcard $(APP_DIR)/*.rs)
ELFS := $(patsubst $(APP_DIR)/%.rs, $(TARGET_DIR)/%, $(APPS))
BINS := $(patsubst $(APP_DIR)/%.rs, $(TARGET_DIR)/%.bin, $(APPS))

RUST_SYSROOT := $(shell rustc --print sysroot)
LLVM_TOOLS_DIR := $(RUST_SYSROOT)/lib/rustlib/$(shell rustc -vV | sed -n 's/^host: //p')/bin
OBJDUMP ?= $(if $(wildcard $(LLVM_TOOLS_DIR)/llvm-objdump),$(LLVM_TOOLS_DIR)/llvm-objdump,rust-objdump)
OBJCOPY ?= $(if $(wildcard $(LLVM_TOOLS_DIR)/llvm-objcopy),$(LLVM_TOOLS_DIR)/llvm-objcopy,rust-objcopy)
CP := cp 

TEST ?= 

elf: $(APPS)
	@cargo build --target $(TARGET) --release
ifeq ($(TEST), 1)
	@$(CP) $(TARGET_DIR)/usertests $(TARGET_DIR)/initproc
endif

binary: elf
	@set -e; \
	for elf in $(ELFS); do \
		$(OBJCOPY) "$$elf" --strip-all -O binary "$$elf.bin"; \
	done

build: binary

clean:
	@cargo clean

.PHONY: elf binary build clean
