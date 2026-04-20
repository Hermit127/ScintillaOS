#!/usr/bin/env bash
set -euo pipefail

# 在仓库根目录执行；可用 LOG_LEVEL=warn 覆盖日志级别
# RUN_MODE=nographic (默认) 或 RUN_MODE=gui
LOG_LEVEL="${LOG_LEVEL:-info}"
RUN_MODE="${RUN_MODE:-nographic}"
TARGET="riscv64gc-unknown-none-elf"
MODE="release"
KERNEL_BIN="target/${TARGET}/${MODE}/os.bin"
FS_IMG="target/${TARGET}/${MODE}/fs.img"

command -v qemu-system-riscv64 >/dev/null || {
  echo "未找到 qemu-system-riscv64，请先安装并加入 PATH"
  exit 1
}

make build LOG="${LOG_LEVEL}"

[ -f "${KERNEL_BIN}" ] || { echo "缺少内核文件: ${KERNEL_BIN}"; exit 1; }
[ -f "${FS_IMG}" ] || { echo "缺少文件系统镜像: ${FS_IMG}"; exit 1; }

if [ "${RUN_MODE}" = "gui" ]; then
  qemu-system-riscv64 \
    -machine virt \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    -kernel "${KERNEL_BIN}" \
    -drive file="${FS_IMG}",if=none,format=raw,id=x0 \
    -smp 1 \
    -serial mon:stdio
else
  qemu-system-riscv64 \
    -nographic \
    -machine virt \
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0 \
    -kernel "${KERNEL_BIN}" \
    -drive file="${FS_IMG}",if=none,format=raw,id=x0 \
    -smp 1
fi