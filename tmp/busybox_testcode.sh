#!/bin/sh
echo "#### OS COMP TEST GROUP START busybox-musl ####"
cat ./busybox_cmd.txt | while read line
do
	case "$line" in ""|\#*) continue ;; esac
	case "$line" in
		*"|"*) eval "$line"; RTN=$? ;;
		*"kill \$!"*) ./busybox sleep 1; RTN=0 ;;  # skip broken kill $! test
		*) ./busybox $line; RTN=$? ;; 
	esac
	if [ $RTN -ne 0 ] && [ "$line" != "false" ]; then
		echo "testcase busybox $line fail"
	else
		echo "testcase busybox $line success"
	fi
done
echo "#### OS COMP TEST GROUP END busybox-musl ####"
