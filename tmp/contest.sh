#!/bin/sh
echo "===== OS AUTO TEST START ====="

# Bind dynamic loaders from contest disk
for dir in musl glibc; do
    [ -d "/mnt/$dir/lib" ] || continue
    mkdir -p /lib
    for src in "/mnt/$dir/lib/"*; do
        name=$(basename "$src")
        [ -f "/lib/$name" ] || cp "$src" "/lib/$name" 2>/dev/null
    done
done

# Copy busybox from contest disk
[ -x /mnt/musl/busybox ] && cp /mnt/musl/busybox /busybox 2>/dev/null
[ -x /mnt/glibc/busybox ] && cp /mnt/glibc/busybox /busybox-glibc 2>/dev/null

skip_group() {
    case "$1" in
        libctest|libcbench|unixbench|cyclictest|netperf|iperf|ltp) return 0 ;;
        *) return 1 ;;
    esac
}

# Scan /mnt/musl/ and /mnt/glibc/ for test scripts
for variant_dir in /mnt/musl /mnt/glibc; do
    [ ! -d "$variant_dir" ] && continue
    variant=$(basename "$variant_dir")
    [ "$variant" = "glibc" ] && bb="/busybox-glibc" || bb="/busybox"
    [ -x "$bb" ] || bb="/busybox"

    # List all *_testcode.sh in this variant directory
    for script in "$variant_dir"/*_testcode.sh; do
        [ -f "$script" ] || continue
        name=$(basename "$script")
        group=${name%_testcode.sh}

        if skip_group "$group"; then
            echo "[SKIP] $group ($variant)"
            continue
        fi

        echo "[RUN] $group ($variant)"
        export LD_LIBRARY_PATH="$variant_dir/lib:/lib"
        cd "$variant_dir" && "$bb" timeout -s KILL 60 "$bb" sh "$name" 2>&1
        cd /
    done
done

echo "===== ALL TESTS COMPLETE ====="
