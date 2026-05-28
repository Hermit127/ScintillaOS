#!/bin/sh
busybox --install -s /bin 2>/dev/null

if [ -f /busybox_testcode.sh ]; then
    busybox sh /busybox_testcode.sh
fi

if [ -x /busybox-glibc ] && [ -f /busybox_glibc_testcode.sh ]; then
    busybox-glibc sh /busybox_glibc_testcode.sh
fi

busybox mount -t ext4 /dev/vda2 /mnt 2>/dev/null
if [ -f /mnt/run_all.sh ]; then
    busybox sh /mnt/run_all.sh
elif [ -f /run_all.sh ]; then
    busybox sh /run_all.sh
fi
