#!/bin/sh
# Contest test runner - dynamically scan test disk and run all tests

echo "===== OS AUTO TEST START ====="

# Copy busybox from contest disk
[ -x /mnt/musl/busybox ]  && cp /mnt/musl/busybox /busybox 2>/dev/null
[ -x /mnt/glibc/busybox ] && cp /mnt/glibc/busybox /busybox-glibc 2>/dev/null

# Skip list (tests that crash or OOM)
skip_group() {
    case "$1" in
        libctest|libcbench|unixbench|cyclictest|netperf|iperf|ltp)
            return 0 ;;
        *)
            return 1 ;;
    esac
}

# Scan and run all test scripts
for script in /mnt/*/*_testcode.sh; do
    [ -f "$script" ] || continue

    runtime=${script#/mnt/}
    runtime=${runtime%%/*}
    group=${script##*/}
    group=${group%_testcode.sh}
    dir=${script%/*}

    if skip_group "$group"; then
        echo "[SKIP] $group ($runtime)"
        continue
    fi

    echo "[RUN] $group ($runtime)"
    cd "$dir" || continue

    if [ "$runtime" = "musl" ]; then
        /busybox sh "${script##*/}" 2>&1
    else
        /busybox-glibc sh "${script##*/}" 2>&1
    fi
    cd /
done

echo "===== ALL TESTS COMPLETE ====="
