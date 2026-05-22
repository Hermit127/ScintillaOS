#!/bin/sh
echo "===== OS AUTO TEST START ====="
busybox sh /basic_testcode.sh
echo "===== BASIC DONE, STARTING BUSYBOX ====="
busybox sh /busybox_testcode.sh
echo "===== ALL TESTS COMPLETE, SHUTTING DOWN ====="
busybox halt -f 2>/dev/null || busybox poweroff -f 2>/dev/null || busybox reboot -f 2>/dev/null
