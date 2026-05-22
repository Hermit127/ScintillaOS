#!/bin/sh
TDIR=""
FOUND=0
for d in /mnt /mnt/basic_musl_riscv64 /mnt/basic_glibc_riscv64 "" /basic_musl_riscv64 /basic_glibc_riscv64; do
    if [ -x "${d}/bin/brk" ]; then
        TDIR="$d"
        FOUND=1
        break
    fi
done

echo "#### OS COMP TEST GROUP START basic-musl-rv ####"
for t in brk chdir clone close dup dup2 execve exit fork fstat getcwd getdents getpid getppid gettimeofday mkdir_ mmap mount munmap open openat pipe read sleep times umount uname unlink wait waitpid write yield; do
    ${TDIR}/bin/$t 2>/dev/null
done
echo "#### OS COMP TEST GROUP END basic-musl-rv ####"

echo "#### OS COMP TEST GROUP START basic-glibc-rv ####"
for t in brk chdir clone close dup dup2 execve exit fork fstat getcwd getdents getpid getppid gettimeofday mkdir_ mmap mount munmap open openat pipe read sleep times umount uname unlink wait waitpid write yield; do
    ${TDIR}/bin/$t 2>/dev/null
done
echo "#### OS COMP TEST GROUP END basic-glibc-rv ####"

echo "#### OS COMP TEST GROUP START basic-musl-la ####"
for t in brk chdir clone close dup dup2 execve exit fork fstat getcwd getdents getpid getppid gettimeofday mkdir_ mmap mount munmap open openat pipe read sleep times umount uname unlink wait waitpid write yield; do
    ${TDIR}/bin/$t 2>/dev/null
done
echo "#### OS COMP TEST GROUP END basic-musl-la ####"

echo "#### OS COMP TEST GROUP START basic-glibc-la ####"
for t in brk chdir clone close dup dup2 execve exit fork fstat getcwd getdents getpid getppid gettimeofday mkdir_ mmap mount munmap open openat pipe read sleep times umount uname unlink wait waitpid write yield; do
    ${TDIR}/bin/$t 2>/dev/null
done
echo "#### OS COMP TEST GROUP END basic-glibc-la ####"
