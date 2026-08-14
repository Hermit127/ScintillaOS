#!/usr/bin/env bash
set -euo pipefail

repo_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$repo_dir"

base_image=${ON_SITE_2025_DISK:-../alpine-linux-riscv64-ext4fs.img}
root_disk=${ON_SITE_2025_ROOT_DISK:-disk.img}
artifact_root=${ON_SITE_2025_ARTIFACT_DIR:-artifacts/onsite2025}
kernel=target/riscv64gc-unknown-none-elf/release/os.bin

for required in "$base_image" "$root_disk" "$kernel"; do
    if [[ ! -f "$required" ]]; then
        echo "missing required file: $required" >&2
        exit 1
    fi
done

run_stamp=$(date -u +%Y%m%dT%H%M%SZ)
run_dir="$artifact_root/$run_stamp"
mkdir -p "$run_dir"
overlay="$run_dir/alpine-overlay.qcow2"
serial_log="$run_dir/serial.log"
pcap="$run_dir/network.pcap"
metadata="$run_dir/metadata.txt"
command_log="$run_dir/qemu-command.txt"

qemu-img create -f qcow2 -F raw -b "$(realpath "$base_image")" "$overlay"

{
    echo "git_commit=$(git rev-parse HEAD)"
    echo "git_branch=$(git branch --show-current)"
    echo "generated_at=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "base_image=$(realpath "$base_image")"
    echo "overlay=$(realpath "$overlay")"
    sha256sum "$base_image" "$kernel"
    compressed_image="${base_image}.xz"
    if [[ -f "$compressed_image" ]]; then
        sha256sum "$compressed_image"
    fi
} > "$metadata"

qemu_command=(
    qemu-system-riscv64
    -machine virt
    -cpu max
    -kernel "$kernel"
    -display none
    -chardev "stdio,id=onsite25console,mux=on,signal=off,logfile=$serial_log"
    -serial chardev:onsite25console
    -mon chardev=onsite25console,mode=readline
    -m 1G
    -smp 1
    -drive "file=$overlay,if=none,format=qcow2,id=x0"
    -device virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0
    -drive "file=$root_disk,if=none,format=raw,id=x1,file.locking=off"
    -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1
    -netdev user,id=net0,hostfwd=tcp::5555-:5001,hostfwd=tcp::5556-:5201
    -device virtio-net-device,netdev=net0,bus=virtio-mmio-bus.2
    -object "filter-dump,id=onsite25dump,netdev=net0,file=$pcap"
)

printf '%q ' "${qemu_command[@]}" > "$command_log"
printf '\n' >> "$command_log"
echo "onsite 2025 artifacts: $run_dir"
"${qemu_command[@]}"
