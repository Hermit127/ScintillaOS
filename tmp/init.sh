#!/bin/sh
echo "===== OS BOOT ====="
busybox --install -s /bin 2>/dev/null
busybox mount -t ext4 /dev/vda2 /mnt 2>/dev/null
if [ -f /mnt/run_all.sh ]; then
    echo "===== RUNNING TESTS FROM TEST DISK ====="
    busybox sh /mnt/run_all.sh
elif [ -f /run_all.sh ]; then
    echo "===== RUNNING TESTS FROM ROOTFS ====="
    busybox sh /run_all.sh
fi
echo "===== ALL TESTS COMPLETE ====="
