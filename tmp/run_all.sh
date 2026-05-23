#!/bin/sh
echo "===== OS AUTO TEST START ====="
/contest_runner
echo "===== BASIC DONE, STARTING BUSYBOX ====="
busybox sh /busybox_testcode.sh
echo "===== ALL TESTS COMPLETE ====="
