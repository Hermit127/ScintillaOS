ARCH ?= riscv64
MODE ?= release
CHAPTER ?= 7
app_dir = src
src_dir := build/$(ARCH)
bin_dir := build/bin
out_dir := target
cmake_build_args := -DARCH=$(ARCH) -DCHAPTER=$(CHAPTER)
cmake_build_args += -DCMAKE_SYSTEM_NAME=Generic
cmake_build_args += -DCMAKE_SYSTEM_PROCESSOR=$(ARCH)
cmake_build_args += -DCMAKE_CROSSCOMPILING=TRUE
cmake_build_args += -DCMAKE_C_COMPILER=$(ARCH)-linux-gnu-gcc
cmake_build_args += -DCMAKE_ASM_COMPILER=$(ARCH)-linux-gnu-gcc
cmake_build_args += -DCMAKE_C_FLAGS="-std=gnu17 -Wno-int-conversion -Wno-incompatible-pointer-types -Wno-implicit-function-declaration -fno-builtin -nostdinc -fno-stack-protector -ggdb -Wall -fno-pie -fno-pic"
cmake_build_args += -DCMAKE_C_LINK_EXECUTABLE="<CMAKE_C_COMPILER> <FLAGS> <CMAKE_C_LINK_FLAGS> <LINK_FLAGS> <OBJECTS> -o <TARGET> <LINK_LIBRARIES>"
cmake_build_args += -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY
ifeq ($(MODE), release)
cmake_build_args += -DCMAKE_BUILD_TYPE=Release
else ifeq ($(MODE), debug)
cmake_build_args += -DCMAKE_BUILD_TYPE=Debug
endif

# riscv64-unknown-elf-
OBJCOPY := $(ARCH)-unknown-elf-objcopy
OBJDUMP := $(ARCH)-unknown-elf-objdump
CP := cp
CH_TESTS :=  

CH2_TESTS := ch2_ ch2t_
CH3_TESTS_BASE := $(CH2_TESTS) ch3_0_ ch3t_
CH4_TESTS := ch2_ ch3_0_ ch4_
CH5_TESTS := $(CH4_TESTS) ch5_
CH6_TESTS := $(CH5_TESTS) ch6_
CH7_TESTS := $(CH6_TESTS) ch7_

ifeq ($(CHAPTER), 2)
	CH_TESTS += $(CH2_TESTS)
else ifeq ($(CHAPTER), 3_0)
	CH_TESTS += $(CH3_TESTS_BASE)
else ifeq ($(CHAPTER), 3_1)
	CH_TESTS += ch3_1_
else ifeq ($(CHAPTER), 3_2)
	CH_TESTS += ch3_2_
else ifeq ($(CHAPTER), 4)
	CH_TESTS += $(CH4_TESTS)
else ifeq ($(CHAPTER), 5)
	CH_TESTS += $(CH5_TESTS)
else ifeq ($(CHAPTER), 6)
	CH_TESTS += $(CH6_TESTS)
else ifeq ($(CHAPTER), 7)
	CH_TESTS += $(CH7_TESTS)
endif

# CH_TESTS += oscomp/open oscomp/write

binary:
	@mkdir -p build
	@cd build && cmake $(cmake_build_args) .. && make
	@$(CP) src/oscomp/run-all.sh src/oscomp/text.txt build/${ARCH}/
	@mkdir build/${ARCH}/mnt && touch build/${ARCH}/mnt/test_mount

pre:
	@rm -rf $(out_dir)
	@mkdir -p $(out_dir)/bin/
	@mkdir -p $(out_dir)/elf/

all: binary

clean:
	@rm -rf build target asm
