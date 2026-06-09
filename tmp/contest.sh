#!/bin/sh
echo "===== OS AUTO TEST START ====="

# Find working busybox
BB=""
for b in /busybox /bin/busybox busybox; do
    [ -x "$b" ] && { BB="$b"; break; }
done
[ -z "$BB" ] && { echo "FATAL: no busybox"; exit 1; }
echo "using busybox: $BB"

# Mount should already be done by initproc
$BB ls /mnt/ 2>&1
echo "---"

# Copy busybox from disk to root (they might be newer/better)
[ -x /mnt/musl/busybox ] && cp /mnt/musl/busybox /busybox 2>/dev/null
[ -x /mnt/glibc/busybox ] && cp /mnt/glibc/busybox /busybox-glibc 2>/dev/null

# Copy dynamic loaders
for d in /mnt/musl/lib /mnt/glibc/lib; do
    [ -d "$d" ] || continue
    mkdir -p /lib
    $BB cp "$d"/* /lib/ 2>/dev/null
done

skip_group() {
    case "$1" in
        libctest|libcbench|unixbench|cyclictest|netperf|iperf|ltp) return 0 ;;
        *) return 1 ;;
    esac
}

# Scan each variant
for vdir in /mnt/musl /mnt/glibc; do
    [ -d "$vdir" ] || continue
    v=$(basename "$vdir")
    bb="$BB"
    [ "$v" = "glibc" ] && [ -x /busybox-glibc ] && bb=/busybox-glibc

    export LD_LIBRARY_PATH="$vdir/lib:/lib"

    for script in "$vdir"/*_testcode.sh; do
        [ -f "$script" ] || continue
        name=$(basename "$script")
        group=${name%_testcode.sh}

        if skip_group "$group"; then
            echo "[SKIP] $group ($v)"
            continue
        fi

        echo "[RUN] $group ($v)"
        cd "$vdir" && "$bb" timeout -s KILL 120 "$bb" sh "$name" 2>&1
        cd /
    done
done

echo "===== ALL TESTS COMPLETE ====="
