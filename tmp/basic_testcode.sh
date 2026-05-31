#!/bin/sh
BDIR=""
[ -f /mnt/bin/brk ] && BDIR="/mnt"
[ -f /bin/brk ] && BDIR=""

echo "#### OS COMP TEST GROUP START basic-musl-rv ####"
for t in brk chdir clone close dup dup2 execve exit fork fstat getcwd getdents getpid getppid gettimeofday mkdir_ mmap mount munmap open openat pipe read sleep times umount uname unlink wait waitpid write yield; do
    ${BDIR}/bin/$t 2>/dev/null
done
echo "#### OS COMP TEST GROUP END basic-musl-rv ####"

echo "#### OS COMP TEST GROUP START basic-glibc-rv ####"
for t in brk chdir clone close dup dup2 execve exit fork fstat getcwd getdents getpid getppid gettimeofday mkdir_ mmap mount munmap open openat pipe read sleep times umount uname unlink wait waitpid write yield; do
    ${BDIR}/bin/$t 2>/dev/null
done
echo "#### OS COMP TEST GROUP END basic-glibc-rv ####"

echo "#### OS COMP TEST GROUP START basic-musl-la ####"
for t in brk chdir clone close dup dup2 execve exit fork fstat getcwd getdents getpid getppid gettimeofday mkdir_ mmap mount munmap open openat pipe read sleep times umount uname unlink wait waitpid write yield; do
    ${BDIR}/bin/$t 2>/dev/null
done
echo "#### OS COMP TEST GROUP END basic-musl-la ####"

echo "#### OS COMP TEST GROUP START basic-glibc-la ####"
for t in brk chdir clone close dup dup2 execve exit fork fstat getcwd getdents getpid getppid gettimeofday mkdir_ mmap mount munmap open openat pipe read sleep times umount uname unlink wait waitpid write yield; do
    ${BDIR}/bin/$t 2>/dev/null
done
echo "#### OS COMP TEST GROUP END basic-glibc-la ####"
