#!/bin/sh
echo "===== OS BOOT ====="
busybox --install -s /bin 2>/dev/null
echo "===== RUNNING TESTS FROM ROOTFS ====="
busybox sh /run_all.sh
echo "===== ALL TESTS COMPLETE ====="
