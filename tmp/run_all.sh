#!/bin/sh
echo "===== OS AUTO TEST START ====="
/contest_runner
echo "===== BASIC DONE, STARTING BUSYBOX ====="
busybox sh /busybox_testcode.sh
if [ -x /busybox-glibc ] && [ -f /busybox_glibc_testcode.sh ]; then
    busybox-glibc sh /busybox_glibc_testcode.sh
fi
echo "===== BUSYBOX DONE, STARTING LUA ====="
if [ -f /lua_musl_testcode.sh ]; then
    busybox sh /lua_musl_testcode.sh
fi
if [ -x /lua-glibc ] && [ -f /lua_glibc_testcode.sh ]; then
    busybox-glibc sh /lua_glibc_testcode.sh
fi
echo "===== ALL TESTS COMPLETE ====="
