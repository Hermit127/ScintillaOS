#!/bin/sh
busybox --install -s /bin 2>/dev/null

busybox mount -t ext4 /dev/vda2 /mnt 2>/dev/null
ran_tests=0

if [ -d /mnt/musl ]; then
    cd /mnt/musl
    [ -f ./basic_testcode.sh ] && busybox sh ./basic_testcode.sh
    [ -f ./busybox_testcode.sh ] && busybox sh ./busybox_testcode.sh
    [ -f ./iozone_testcode.sh ] && busybox sh ./iozone_testcode.sh
    [ -f ./libctest_testcode.sh ] && busybox sh ./libctest_testcode.sh
    [ -f ./libcbench_testcode.sh ] && busybox sh ./libcbench_testcode.sh
    [ -f ./lmbench_testcode.sh ] && busybox sh ./lmbench_testcode.sh
    [ -f ./lua_testcode.sh ] && busybox sh ./lua_testcode.sh
    [ -f ./unixbench_testcode.sh ] && busybox sh ./unixbench_testcode.sh
    [ -f ./netperf_testcode.sh ] && busybox sh ./netperf_testcode.sh
    [ -f ./iperf_testcode.sh ] && busybox sh ./iperf_testcode.sh
    [ -f ./cyclictest_testcode.sh ] && busybox sh ./cyclictest_testcode.sh
    cd /
    ran_tests=1
fi

if [ -d /mnt/glibc ]; then
    cd /mnt/glibc
    [ -f ./basic_testcode.sh ] && busybox sh ./basic_testcode.sh
    [ -f ./busybox_testcode.sh ] && busybox sh ./busybox_testcode.sh
    [ -f ./iozone_testcode.sh ] && busybox sh ./iozone_testcode.sh
    [ -f ./libctest_testcode.sh ] && busybox sh ./libctest_testcode.sh
    [ -f ./libcbench_testcode.sh ] && busybox sh ./libcbench_testcode.sh
    [ -f ./lmbench_testcode.sh ] && busybox sh ./lmbench_testcode.sh
    [ -f ./lua_testcode.sh ] && busybox sh ./lua_testcode.sh
    [ -f ./unixbench_testcode.sh ] && busybox sh ./unixbench_testcode.sh
    [ -f ./netperf_testcode.sh ] && busybox sh ./netperf_testcode.sh
    [ -f ./iperf_testcode.sh ] && busybox sh ./iperf_testcode.sh
    [ -f ./cyclictest_testcode.sh ] && busybox sh ./cyclictest_testcode.sh
    cd /
    ran_tests=1
fi

if [ "$ran_tests" = "0" ] && [ -f /mnt/run_all.sh ]; then
    cd /mnt
    busybox sh ./run_all.sh
    cd /
elif [ "$ran_tests" = "0" ] && [ -f /run_all.sh ]; then
    busybox sh /run_all.sh
fi

/bin/shutdown
