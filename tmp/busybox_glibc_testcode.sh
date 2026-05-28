#!/busybox-glibc sh

./busybox-glibc echo "#### OS COMP TEST GROUP START busybox-glibc ####"

./busybox-glibc cat ./busybox_glibc_cmd.txt | while read line
do
    cmd=$(printf "%s" "$line" | ./busybox-glibc sed "s#\./busybox#./busybox-glibc#g")
    eval "./busybox-glibc $cmd"
    RTN=$?
    if [ "$RTN" -ne 0 ] && [ "$line" != "false" ]; then
        echo "testcase busybox $line fail"
	else
		echo "testcase busybox $line success"
	fi
done

./busybox-glibc echo "#### OS COMP TEST GROUP END busybox-glibc ####"
