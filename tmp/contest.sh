#!/bin/sh
# Contest test runner - dynamically scan test disk

echo "===== OS AUTO TEST START ====="

# Debug: show mount
echo "[DBG] listing /mnt:"
/busybox ls /mnt/ 2>&1 | head -5
echo "[DBG] listing /:"
/busybox ls / 2>&1 | head -5

# Find and copy busybox
for bb in /mnt/busybox /mnt/*/busybox /busybox /bin/busybox; do
    [ -x "$bb" ] && { echo "[DBG] found busybox at $bb"; cp "$bb" /busybox 2>/dev/null; break; }
done

skip_group() {
    case "$1" in
        libctest|libcbench|unixbench|cyclictest|netperf|iperf|ltp)
            return 0 ;;
        *)
            return 1 ;;
    esac
}

found=0
# Scan both /mnt/xxx_testcode.sh and /mnt/*/xxx_testcode.sh
for pattern in "/mnt/*_testcode.sh" "/mnt/*/*_testcode.sh"; do
    for script in $pattern; do
        [ -f "$script" ] || continue
        found=1
        dir=$(dirname "$script")
        name=$(basename "$script")
        group=${name%_testcode.sh}
        runtime=$(basename "$dir")
        [ "$runtime" = "mnt" ] && runtime="musl"

        if skip_group "$group"; then
            echo "[SKIP] $group ($runtime)"
            continue
        fi

        echo "[RUN] $group ($runtime)"
        case "$runtime" in
            glibc|*glibc*) bb="/busybox-glibc" ;;
            *) bb="/busybox" ;;
        esac
        [ -x "$bb" ] || bb="/busybox"

        cd "$dir" && "$bb" sh "$name" 2>&1
        cd /
    done
done

[ $found -eq 0 ] && echo "[DBG] no test scripts found!"
echo "===== ALL TESTS COMPLETE ====="
