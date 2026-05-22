#!/bin/sh
busybox --install -s /bin 2>/dev/null
echo "#### OS COMP TEST GROUP START busybox-musl ####"
busybox cat ./busybox_cmd.txt | while read line
do
	case "$line" in ""|\#*) continue ;; esac
	case "$line" in
		*"|"*) eval "$line"; RTN=$? ;;
		*) busybox $line; RTN=$? ;;
	esac
	if [ $RTN -ne 0 ] && [ "$line" != "false" ]; then
		echo "testcase busybox $line fail"
	else
		echo "testcase busybox $line success"
	fi
done
echo "#### OS COMP TEST GROUP END busybox-musl ####"
