#!/bin/sh
echo "===== OS BOOT ====="
if [ -x /bin/busybox-glibc ]; then
    echo "===== TESTING GLIBC BUSYBOX ====="
    /bin/busybox-glibc echo "glibc busybox works!"
    /bin/busybox-glibc true
    /bin/busybox-glibc false
fi
busybox --install -s /bin 2>/dev/null
busybox mount -t ext4 /dev/vda2 /mnt 2>/dev/null
if [ -f /mnt/run_all.sh ]; then
    echo "===== RUNNING TESTS FROM TEST DISK ====="
    busybox sh /mnt/run_all.sh
else
    echo "===== RUNNING TESTS FROM ROOTFS ====="
    busybox sh /run_all.sh
fi
echo "===== ALL TESTS COMPLETE ====="
