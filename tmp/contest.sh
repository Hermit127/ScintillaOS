#!/bin/sh
# Contest test runner - dynamically scan test disk

echo "===== OS AUTO TEST START ====="

# Find and copy busybox
for bb in /mnt/busybox /mnt/*/busybox; do
    [ -x "$bb" ] && cp "$bb" /busybox 2>/dev/null && break
done
for bb in /mnt/busybox-glibc /mnt/*/busybox-glibc /mnt/*/busybox; do
    [ -x "$bb" ] && cp "$bb" /busybox-glibc 2>/dev/null && break
done

skip_group() {
    case "$1" in
        libctest|libcbench|unixbench|cyclictest|netperf|iperf|ltp)
            return 0 ;;
        *)
            return 1 ;;
    esac
}

# Scan both /mnt/xxx_testcode.sh and /mnt/*/xxx_testcode.sh
find /mnt -maxdepth 2 -name "*_testcode.sh" 2>/dev/null | sort | while read script; do
    dir=$(dirname "$script")
    name=$(basename "$script")
    group=${name%_testcode.sh}

    # guess runtime from directory name
    runtime=$(basename "$dir")
    [ "$runtime" = "mnt" ] && runtime="musl"

    if skip_group "$group"; then
        echo "[SKIP] $group ($runtime)"
        continue
    fi

    echo "[RUN] $group ($runtime)"
    # Use appropriate busybox: musl dir -> /busybox, glibc dir -> /busybox-glibc  
    case "$runtime" in
        glibc|*glibc*) bb="/busybox-glibc" ;;
        *) bb="/busybox" ;;
    esac
    [ -x "$bb" ] || bb="/busybox"

    cd "$dir" && "$bb" sh "$name" 2>&1
    cd /
done

echo "===== ALL TESTS COMPLETE ====="
