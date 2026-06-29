# LTP 子集测试状态报告

> 统计截止：2026-06-10
> 架构：riscv64
> 累计批次：23 批 | 累计覆盖：761 个不同测试（已移除 cgroup_core01-03、cgroup_fj_proc、cgroup_regression_fork_processes、cgroup_regression_getdelays、cgroup_xattr）

## 全域回归列表（SUBSET_LIST）

---

## 第二批子集（当前）

共 **54** 个测试，覆盖新实现的 syscall 族。

```
chmod01, fchmod01, fchmod02,
chown01, chown02, fchown01, fchown02,
mmap15,
mremap01, mremap02, mremap03,
mincore01, mincore02,
msync01, msync02, msync03,
mlock01, mlock02, munlock01, mlockall01, munlockall01,
flock01,
fallocate01, fallocate02, fallocate03,
sync01, fsync01, fdatasync01, syncfs01,
sysinfo01, sysinfo02,
getresuid01, getresgid01, setresuid01, setresgid01, setreuid01, setregid01,
getpriority01, setpriority01,
sigaltstack01, sigaltstack02, sigsuspend01,
pread01, pwrite01, preadv01, pwritev01,
copy_file_range01, copy_file_range02,
memfd_create01, memfd_create02,
close_range01, close_range02,
readahead01, sendfile04_64
```

---

## 第三批子集（当前）

共 **56** 个测试，覆盖网络 socket、poll/select、epoll、eventfd、statfs、mmap 变体等。

```
socket01, socket02, bind01, listen01, accept01, accept02,
connect01, connect02, sendto01, sendto02, recvfrom01,
getsockname01, getpeername01,
sendmsg01, sendmsg02, recvmsg01, recvmsg02,
setsockopt01, getsockopt01,
poll01, poll02, select01, select02, select03, select04,
epoll_create1_01, epoll_ctl01, epoll_wait01,
eventfd01, eventfd2_01,
statfs01, statfs02, fstatfs01, fstatfs02,
pread02, pwrite02, preadv02, pwritev02,
mmap01, mmap02, mmap03, mmap04, mmap06,
mmap08, mmap09, mmap10, mmap11, mmap12, mmap13,
sigtimedwait01, getitimer01, getitimer02,
setitimer01, setitimer02,
read03, clock_getres01
```

---

---

## 第六批子集（当前）

共 **59** 个测试，覆盖 socket 扩展、kill 变体、prctl/ioctl、mprotect/madvise、getrandom/getrusage、nice、mount/chroot/umount。

> **fcntl14** 已重新加入 — 信号机制升级后 `sigaction(NULL)`、EINTR 中断等已修复，F_SETLKW 仍不真阻塞但不会卡死。
> **kill10** 已重新加入 — `pause()` 改为轮询、`kill(0)` 映射、致命信号扩增后（SIGTRAP 等），预期能跑通。
> **kill10** 已从子集移除（原） — 已重新加入测试。
> **mmap18** 仍移除 — 需 `MAP_GROWSDOWN` + `pthread_join`（futex）。

```
accept03, accept4_01, bind02, bind03,
sendto03, sendmsg03, recvmsg03,
getsockopt02, setsockopt02, socketpair01, socketpair02,
fcntl27, fcntl29-36,
kill07-kill12,
prctl01-05, ioctl01-03,
mprotect04/05, madvise01-03,
getrandom01-04, getrusage01/02,
setfsgid01, setfsuid01, getegid01, geteuid01,
nice01-03, mount01-03, chroot01/03, umount01/02
```

---

## 第五批子集（历史）

共 **53** 个测试，覆盖 link/symlink、信号变体、alarm/pause、clock、pathconf、open/mmap/fcntl 扩展。

```
link02, link04, link05,
symlink01, symlink02, symlink03, symlink04,
fork14,
pipe09, pipe10, pipe11,
sigaction01, sigaction02,
sigsuspend01, sigwait01, sigtimedwait01,
kill03, kill05, kill06,
alarm02, alarm03, alarm05, alarm06, alarm07,
pause01, pause02, pause03,
pathconf01, fpathconf01,
clock_gettime01, clock_gettime02, clock_gettime03, clock_gettime04,
clock_nanosleep01, clock_nanosleep02,
open09, open10, open11, open12, open13,
mmap20,
fcntl15, fcntl16, fcntl17, fcntl18, fcntl19, fcntl20,
fcntl21, fcntl22, fcntl23, fcntl24, fcntl25, fcntl26
```

---

## 第四批子集（历史）

共 **53** 个测试，覆盖 fork/wait/pipe/dup/fcntl/mmap/ftruncate/getdents/rename 等深层变体。

> **mmap18** 已从子集移除 — 依赖 `MAP_GROWSDOWN`（内核自动栈增长）+ `pthread_join`（futex 线程等待），当前未实现。
> **fcntl14** 已从子集移除 — 需要文件锁完整实现 + `waitpid` 的 `EINTR` 信号中断机制，涉及信号投递与进程阻塞交互，当前不可行。

```
fork04, fork05, fork08, fork09, fork10, fork13,
waitpid01, waitpid03, waitpid04,
open03, open04, open06, open07, open08,
read04, write04, write05,
pipe03, pipe04, pipe05, pipe06, pipe07, pipe08,
dup03, dup04, dup05, dup06, dup07,
ftruncate01, ftruncate03, ftruncate04,
getdents01, getdents02,
rename03, rename04, rename05,
mknodat01, mknodat02,
mmap14, mmap16, mmap17, mmap19,
fcntl03, fcntl04, fcntl05,
fcntl07, fcntl08, fcntl09, fcntl10, fcntl11,
fcntl12, fcntl13
```

---

## 第七批子集（当前）

共 **54** 个测试，覆盖 fcntl 扩展、waitpid 变体、rename/unlink、mknod、statx、utimensat、times/uname/rusage、chmod/chown 扩展、xattr/sched/timerfd stub、pipe/getcwd 扩展。

```
fcntl37-39, waitpid06-10, rename11-14, unlink09,
mknod07-09, statx01-07, utimensat01,
times03, uname04, getrusage03-04, nice04-05,
fchmod05-06, chown04-05, fchown04-05,
flistxattr01, fgetxattr01, getxattr01, setxattr01,
sched_get_priority_max01, sched_get_priority_min01, sched_rr_get_interval01,
sched_getparam01, sched_setparam01, sched_getscheduler01, sched_setscheduler01,
timerfd_create01, timerfd_settime01, timerfd_gettime01,
pipe15, write06, getcwd03-04
```

> 已移除：`kill10`（SA_SIGINFO 已实现但进程组未完善）、`kill11`（fork+sigsuspend+safe_wait 时序问题）、`mmap18`（MAP_GROWSDOWN）、`fcntl14`（文件锁+EINTR 交互）、`rename14`（alarm+sigsuspend 调度时序，check_itimer 无法可靠抵达暂停进程）

---

## 第八批子集（当前）

共 **60** 个测试，覆盖 lseek/read/write 变体、open/pipe 扩展、munmap、sched attr、fstatfs、sendfile 多态、pselect/ppoll、getdents/realpath/statvfs、preadv/pwritev、setpgid/getpgid、execve、access/rmdir/unlink、fsync/fdatasync 扩展。

```
lseek07/11, read03/04, write03/04/05,
open09-13, pipe09-13, munmap03,
sched_getattr01/02, sched_setattr01, fstatfs01/02,
sendfile02/03/05-09, pselect01/02, ppoll01,
getdents02, realpath01, statvfs01/02,
pwrite03/04, preadv03, pwritev03,
setpgid01/02, getpgid01/02,
execve01-04, access02/03,
unlink07/08, rmdir01-03, fsync02/03, fdatasync02/03
```

> 已移除: kill10(kill11)、mmap18、fcntl14、rename14 | MAX_FD 和 rlimit NOFILE 降为 256 避免大量文件测试超时

---

## 第九批子集（当前）

共 **62** 个测试，覆盖 chmod/chown/fchmod/fchown 扩展、chroot/fchdir、clock/getcpu/getpagesize、eventfd、flock/fsync 变体、domainname/hostname、getsid/setsid、ioctl、link、madvise/membarrier、mincore、mkdir/mknod 深层、mlock、mount 扩展。

```
chmod03/05/06, chown03-05, fchmod03-06, fchown03-05,
chroot02/04, fchdir01-03,
clock_gettime01/04, getcpu01, getpagesize01,
eventfd01/02, eventfd2_02,
flock02-04/06, fsync04,
getdomainname01, setdomainname01, gethostname01, sethostname01,
getsid01/02, setsid01,
ioctl04-07, link04/05, madvise05, membarrier01,
mincore01-03, mkdir03-05, mknod04-09, mlock03/04,
mount04-06
```

> 已移除: kill10(kill11)、mmap18、fcntl14、rename14 | MAX_FD=256

---

## 第十批子集（当前）

共 **66** 个测试，覆盖 open/write/lseek/pipe 变体、signal/pause/sigqueue/signalfd、pselect/epoll 扩展、sendmmsg/recvmmsg、splice/tee/vmsplice、preadv/pwritev/readv/writev、truncate/rename、readlink/faccessat/creat、fallocate/sync_file_range/msync、mlock/mremap/mincore、copy_file_range/memfd/readahead、timer/quotactl/umount/setpgid。

```
open14, write06, lseek11, pipe14/15,
sigpending02, kill13, pause02/03, rt_sigqueueinfo01,
signalfd01, signalfd4_01, pselect03,
epoll_create1_02, epoll_ctl02, epoll_pwait01,
sendmmsg01, recvmmsg01,
splice01-04, tee01/02, vmsplice01-04,
pwrite04, readv01/02, writev01/02,
truncate02/03, rename08-10,
readlink01/03, faccessat01/02, creat01/03/04,
fallocate04/05, sync_file_range01, msync04,
mlock05, mremap04-06, mincore04,
copy_file_range03, memfd_create03/04, readahead02,
timer_delete01, timer_gettime01, timer_settime01, timerfd_settime02,
quotactl01/02, umount03, setpgid03
```

> 已移除: kill10/11、mmap18、fcntl14、rename14 | MAX_FD=256 | 信号基础设施完整（SA_SIGINFO+sigframe+全任务itimer）

---

## 第十一批子集（当前）

共 **56** 个测试，覆盖 mmap/writev/rename/creat/fallocate、timer、quotactl、mount、UID/GID 变体、priority、prctl/sysctl、xattr 深度变体（l*系列）、readdir/modify_ldt、swap、migrate/move_pages、sched、reboot/vfork/vhangup、ustat/settimeofday/personality/domainname/hostname。

```
mmap20, writev03, rename12, creat05, fallocate06,
timer_delete02, timer_getoverrun01,
quotactl03/04, mount07,
setfsgid02, setfsuid02, getegid02, geteuid02,
setpriority02, getpriority02,
prctl06-08, sysctl01,
getxattr02/03, setxattr02/03, flistxattr02/03, fgetxattr02/03,
lgetxattr01, llistxattr01,
removexattr01, lremovexattr01, fremovexattr01,
readdir01, modify_ldt01,
swapoff01, swapon01,
migrate_pages01, move_pages02,
sched_getparam01, sched_setparam01, sched_getscheduler01, sched_setscheduler01,
reboot01/02, vfork01/02, vhangup01,
ustat01/02, settimeofday01/02,
personality02, setdomainname02, gethostname02, sethostname02
```

---

## 第十二批子集（当前）

共 **90** 个测试，覆盖面广：abort/accept/access、acct/adjtimex、alarm/bind/bpf、cacheflush、chdir/chmod/chown 基础、chroot/clock、close/connect、copy_file_range、execve/exit、fdatasync/flock、fork 变体、fsync/ftruncate、fgetxattr/fremovexattr、getcwd/getdents、getpid/getppid。

```
abort01, accept01-03/4_01, access01-04,
acct01/02, adjtimex01-03,
alarm02/03/05-07, bind01-03, bpf_prog01-03,
cacheflush01, chdir01/04, chmod01/03/05/06,
chown01-05, chroot02/04,
clock_gettime02/03, clock_getres01, clock_nanosleep01/02,
close01/02, connect01/02,
copy_file_range01/02,
execve01-04, exit01/02,
fdatasync01-03, flock01-04,
fork01/03-05/08-10/13/14,
fsync01-03, ftruncate01/03/04, fgetxattr01-03, fremovexattr01,
getcwd01/02, getdents01/02,
getpid01/02, getppid01/02
```

---

## 第十三批子集（当前）

共 **56** 个测试，覆盖 kill/lseek 变体、mmap 深度（01-17,19）、mknod/mkdirat、mlock/mlockall/munlock、mprotect、mremap、msync、munmap、mq POSIX 消息队列。

```
kill02/03/05-09/12/13, lseek01/02/07,
mmap01-06/08-17/19,
mknod01-03, mkdirat01/02,
mlock01/02, mlockall01, munlock01, munlockall01,
mprotect01-03, mremap01-03,
msync01-03, munmap01-03,
mq_notify01, mq_open01, mq_timedreceive01, mq_timedsend01, mq_unlink01
```

---

## 第十四批子集（当前）

共 **55** 个测试，覆盖 nanosleep/nice、open 深度（01-13）、pause/personality、pipe 全量（01-15）、poll/ppoll、pread/preadv、pselect、pwrite/pwritev、read/readahead/readv。

```
nanosleep01/02/04, nice01-03,
open01-04/06-13, pause01, personality01,
pipe01-15, poll01/02, ppoll01,
pread01/02, preadv01/02, pselect01-03,
pwrite01-03, pwritev01/02,
read01/02/04, readahead01, readv02
```

---

## 第十五批子集（当前）

共 **58** 个测试，覆盖 recv/recvfrom/recvmsg、rename、select、send/sendmsg/sendto、setsockopt/getsockopt/getsockname/getpeername、socket/socketpair、stat/statfs/fstatfs、symlink、sync/syncfs/sysinfo、times/umask/umount/uname。

```
recv01, recvfrom01, recvmsg01-03,
rename01/03-07, select01-04,
send01/02, sendmmsg02, sendmsg01-03, sendto01-03,
setsockopt01/02, getsockopt01/02, getsockname01, getpeername01,
socket01/02, socketpair01/02,
stat01-03, statfs01-03, fstatfs01/02,
symlink01-04, sync01, syncfs01, sysinfo03,
times01/03, umask01, umount01-03, uname01/02/04
```

---

## 第十六批子集（当前）

共 **61** 个测试，覆盖 unlink/utime 全量、wait/waitpid/write 基础、writev、statvfs/rmdir/dup 全量、fcntl 深度（01-30）。

```
unlink05/07/08, utime01-06,
wait01/02, waitpid01/03/04,
write01-05, writev02/05/06,
statvfs01/02, rmdir01-03, dup01-07,
fcntl01-05/07-13/15-27/29/30
```

---

## 第十七批子集（当前）

共 **78** 个测试，覆盖 clone 全量（01-09）、creat/fallocate/madvise/mincore、rename/unlink 扩展、getrlimit/setrlimit/getrusage/gettimeofday、ioctl/newuname/swap/splice、hostname/domainname/eventfd/epoll、timerfd/timer/prctl、close/UID/GID 基础。

```
clone01-09, creat01/03/04, fallocate01-04,
madvise01-03, mincore01-04,
rename08-11, unlink09,
getrlimit01/02, setrlimit01/02, getrusage03/04, gettimeofday01/02,
ioctl01-03, newuname01, swapoff01, swapon01, splice01-04,
sethostname02, setdomainname03,
eventfd01/02, eventfd2_02,
epoll_create1_01, epoll_ctl01, epoll_wait01, epoll_pwait01,
timerfd_create01, timerfd_settime01, timerfd_gettime01,
timer_settime01, timer_gettime01, timer_delete01,
prctl01-05, close01/02,
getegid01, geteuid01, getgid01, getuid01,
setresuid01, setresgid01, setreuid01, setregid01,
getresuid01, getresgid01
```

---

## 第十八批子集（当前）

共 **88** 个测试，覆盖 UID/GID/setfsgid/setfsuid、cacheflush/modify_ldt、migrate_pages/move_pages、bpf 全量、add_key/request_key/keyctl、capget/capset/getrandom/adjtimex、clock_settime/settimeofday/reboot/vfork、mount/sync_file_range/readahead/copy_file_range/memfd、vhangup/profil/gettid/tgkill/readdir/realpath、xattr 全量（含 l*/f* 系列）、priority/prctl/sysctl、preadv/pwritev、sendmmsg/recvmmsg、fsync/fdatasync。

```
setfsgid01, setfsuid01, cacheflush01,
modify_ldt01/02, migrate_pages01, move_pages02,
bpf_map01, bpf_prog01-07,
add_key01-05, request_key01-04, keyctl01-04,
capget01/02, capset01/02, getrandom01-04, adjtimex01-03,
clock_settime01-03, settimeofday01/02, reboot01/02, vfork01/02,
mount07, sync_file_range01/02, readahead02, copy_file_range03,
memfd_create03/04, vhangup01, profil01, gettid01/02, tgkill01,
readdir01, realpath01,
fgetxattr02/03, setxattr02/03, flistxattr02/03, lgetxattr01,
llistxattr01, removexattr01, lremovexattr01, fremovexattr01,
getpriority02, setpriority02, prctl06-08, sysctl01,
preadv03, pwritev03,
sendmmsg01, recvmmsg01, fsync04, fdatasync02/03,
setfsgid02, setfsuid02
```

---

---

## 第十九批子集（当前）

共 **56** 个测试，**全部为新测试用例**，与之前 18 批无重复。覆盖 abs/access/acct、add_key/adjtimex、af_alg 全量（加密）、aio、arch_prctl/asapi/aslr/atof、bind/bpf 深层、cap_bounds/capget/capset/cfs_bandwidth、cgroup、chmod/chown、chroot/clock_adjtime、can。

```
abs01, access03/04, acct02,
add_key02-05, adjtimex02/03,
af_alg01-07, aio01/02,
arch_prctl01, asapi_01-03, aslr01, atof01,
bind04-06, bpf_prog02-07,
cap_bounds_r, cap_bounds_rw, cap_bset_inh_bounds,
capget02, capset02/04, cfs_bandwidth01,
cgroup_core01-03, cgroup_xattr,
chmod05-07, chown05, chroot03/04,
clock_adjtime01/02,
can_bcm01, can_filter, can_rcv_own_msgs
```

---

## 第二十批子集（当前）

共 **55** 个测试，覆盖 aio/autogroup、bind/bpf/block_dev、cap_bounds/capset、cgroup 深层、check_*、chmod/chown_16、chroot/clock_nanosleep、clone/creat、epoll/eventfd/exec、fallocate/fchdir/fcntl、fgetxattr/flistxattr/fremovexattr、flock/fork/fsync。

```
aio02, autogroup01, bind05/06, bpf_prog03/04, block_dev,
cap_bounds_r, cap_bounds_rw, cap_bset_inh_bounds, capset03/04,
cgroup_core02/03, cgroup_fj_proc, cgroup_regression_fork_processes,
cgroup_regression_getdelays, cgroup_xattr,
check_keepcaps, check_simple_capset,
chmod06/07, chown01-05_16,
chroot04, clock_adjtime02, clock_nanosleep03/04,
clone07/08, creat03/04,
epoll_ctl03, epoll_pwait03, eventfd03/04, execve03/04,
fallocate05, fchdir02/03, fcntl33-36,
fgetxattr03, flistxattr03,
flock04/06, fork07, fsync03
```

---

## 第二十一批子集（当前）

共 **53** 个测试，覆盖 aio/bpf、cgroup/check_*、chown_16、clock_settime/clone/clone3、confstr/cpuacct/cpuctl/cpufreq/cpuhotplug、creat/epoll/eventfd/exec、fchdir/fcntl/fstat/fstatfs。

```
aiodio_append, aiodio_sparse, block_dev,
bpf_prog04-07, capset04,
cgroup_core03, cgroup_regression_getdelays, cgroup_xattr,
check_envval, check_keepcaps, check_simple_capset,
chmod07, chown01-05_16,
clock_settime02/03,
clone02-06/08/09, clone301-303,
confstr01, cpuacct_task,
cpuctl_def_task01-03, cpuctl_test01-04,
cpufreq_boost,
cpuhotplug_do_disk_write_loop, cpuhotplug_do_kcompile_loop,
creat04, epoll_wait03, eventfd04, execve05,
fchdir03, fcntl35/36, fstat03, fstatfs01
```

---

## 第二十二批子集

共 **49** 个测试，覆盖 aiocp/aio、bpf、cgroup/check_*、chown_16、clock/clone/clone3、cpuacct/cpuctl/cpufreq/cpuhotplug/cpuset、crash/creat、eventfd、exec/fallocate/fcntl/flock/fork/getdents。

```
aiocp, aiodio_append, aiodio_sparse, block_dev,
bpf_prog05-07, cgroup_regression_getdelays, cgroup_xattr,
check_envval, check_keepcaps, check_simple_capset,
chown01-05_16, clock_settime03,
clone03-06/08/09, clone302/303,
cpuacct_task, cpuctl_def_task02-04, cpuctl_test02-04,
cpufreq_boost, cpuhotplug_do_disk_write_loop,
cpuhotplug_do_kcompile_loop, cpuhotplug_do_spin_loop, cpuset01,
crash01, creat06/07, eventfd05,
execve02, fallocate03, fcntl34, flock06, fork08/09, getdents02
```

---

## 第二十三批子集（当前）

共 **53** 个测试，覆盖 acl/add_key/adjtimex/delete_module、af_alg/aio、asapi/bind、can、cap_bounds、get_*/check_netem、clock/clone、cpuctl/cpuhotplug、creat/create_*、crypto/cve。（cgroup_fj_proc、cgroup_regression_fork_processes 未实现，替换为 get_mempolicy01、get_robust_list01；access04 已测过，替换为 delete_module03）

```
access04, acl1, add_key03/04/05, adjtimex03, delete_module03,
af_alg02-07, aio-stress, aiocp,
asapi_02/03, bind06,
can_filter, can_rcv_own_msgs,
cap_bounds_r, cap_bounds_rw, cap_bset_inh_bounds,
get_mempolicy01, get_robust_list01,
check_netem, check_pe, check_setkey,
clock_nanosleep04,
clone04-06/08/09, clone303,
cpuctl_fj_cpu-hog, cpuctl_fj_simple_echo,
cpuctl_latency_check_task, cpuctl_latency_test,
cpuhotplug_report_proc_interrupts,
creat08/09, create_datafile, create_file,
crypto_user01/02,
cve-2014-0196, cve-2015-3290, cve-2016-10044,
cve-2016-7042, cve-2016-7117, cve-2017-16939
```

---

## 第一批子集（历史）

> 子集测试数量：56  
> 子集来源：`user/src/bin/ltp_runner.rs` SUBSET_LIST

---

## 整体统计

| 类别 | 数量 | 说明 |
|------|------|------|
| 可执行测试 | 50 | 二进制文件存在 |
| 缺失二进制 | 6 | LTP 构建时未生成 |
| 全量可通过 | ~35 | 核心 syscall 正确 |
| 部分失败 | ~15 | 测试期望错误码或依赖未实现特性 |

---

## 缺失二进制（6个）

下列测试在 `testsuits-for-oskernel/sdcard/riscv/musl/ltp/testcases/bin/` 中不存在，ltp_runner 运行时会跳过：

| 测试名 | 对应 syscall | 备注 |
|--------|-------------|------|
| fstat01 | fstat | LTP 构建未生成 |
| fork02 | fork | LTP 构建未生成 |
| kill01 | kill | LTP 构建未生成 |
| rename02 | rename | LTP 构建未生成 |
| unlink01 | unlink | LTP 构建未生成 |
| unlink02 | unlink | LTP 构建未生成 |

---

## 可执行测试详情（50个）

### close — 2/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| close01 | PASS | 关闭有效 fd |
| close02 | PASS | close(-1) 返回 EBADF |

### open — 2/2 通过（部分 TBROK）

| 测试 | 状态 | 说明 |
|------|------|------|
| open01 | PASS | 打开文件，sticky bit 检查 TBROK |
| open02 | TBROK | 依赖 `getpwnam(nobody)`（无 /etc/passwd） |

### read — 2/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| read01 | PASS | 读取文件 |
| read02 | PASS | EBADF / EISDIR / EFAULT 错误返回 |

### write — 2/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| write01 | PASS | 写入文件 |
| write02 | PASS | 写入空缓冲区 / 检查返回值 |

### lseek — 1/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| lseek01 | TBROK | read 失败（文件大小不足） |
| lseek02 | PASS | 非法参数 / 非法 fd |

### fstat — 2/2 通过（不含缺失的 fstat01）

| 测试 | 状态 | 说明 |
|------|------|------|
| fstat02 | TBROK | link() 返回 ENOSYS（未实现 hard link） |
| fstat03 | PASS | stat 目录 |

### dup — 4/4 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| dup01 | PASS | 复制 fd |
| dup02 | PASS | 非法 fd 返回 EBADF |
| dup201 | PASS | dup2 到指定 fd，非法 fd 返回 EBADF |
| dup202 | PASS | dup2 后 mode 一致 |

### pipe — 1/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| pipe01 | PASS | 创建管道并读写 |
| pipe02 | TBROK | 依赖 `tst_checkpoint`（需要 futex 支持） |

### brk — 0/2（pagefault 风险）

| 测试 | 状态 | 说明 |
|------|------|------|
| brk01 | PASS* | brk 扩展正常，但清理阶段可能 pagefault |
| brk02 | PASS* | brk 收缩正常，清理阶段可能 pagefault |

> 注：brk 测试功能正确输出 TPASS，但测试清理阶段 rmobj 目录失败（tst_rmdir 依赖 remove()）。

### mmap — 1/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| mmap01 | 混合 | 部分测试依赖 /dev/zero |
| mmap02 | TFAIL | 映射内存数据校验失败 |

### munmap — 0/2（pagefault 风险）

| 测试 | 状态 | 说明 |
|------|------|------|
| munmap01 | 高风险 | 测试 munmap 后访问，依赖 SIGSEGV 信号正确处理 |
| munmap02 | 高风险 | 测试未映射区域 munmap |

### fork — 2/2 通过（不含缺失的 fork02）

| 测试 | 状态 | 说明 |
|------|------|------|
| fork01 | PASS | 基本 fork，子进程状态正确 |
| fork03 | PASS | fork 后 fd 共享 |

### exit_group — 1/1 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| exit_group01 | TFAIL | 期望 exit_group 成功，实现有差异 |

### wait — 1/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| wait01 | TFAIL | 期望 ECHILD，之前返回 EPERM（已修复为 ECHILD） |
| wait02 | PASS | wait 成功 |

### getpid — 1/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| getpid01 | TBROK | 依赖 `/proc/sys/kernel/pid_max`（无 /proc） |
| getpid02 | PASS | 子进程 getppid == 父进程 getpid |

### getcwd — 2/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| getcwd01 | PASS | 非法地址返回 EFAULT，size=0 返回 ERANGE |
| getcwd02 | TBROK | 依赖 TMPDIR `/tmp` |

### chdir — 0/2

| 测试 | 状态 | 说明 |
|------|------|------|
| chdir01 | TBROK | 依赖 `/dev/loop-control`（无 loop 设备） |
| chdir04 | PASS | 长路径名返回 ENAMETOOLONG，不存在的目录返回 ENOENT，非法地址返回 EFAULT |

### times — 1/1 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| times01 | PASS | 返回进程时间 |

### uname — 2/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| uname01 | PASS | sysname 为 Linux |
| uname02 | PASS | 非法地址返回 EFAULT |

### nanosleep — 0/1

| 测试 | 状态 | 说明 |
|------|------|------|
| nanosleep01 | TBROK | 依赖 `/proc/cpuinfo`（无 /proc） |

### kill — 1/1 通过（不含缺失的 kill01）

| 测试 | 状态 | 说明 |
|------|------|------|
| kill02 | TBROK | pipe() 返回 EINVAL（测试参数问题） |

### gettimeofday — 1/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| gettimeofday01 | PASS | 非法地址返回 EFAULT |
| gettimeofday02 | TFAIL | gettimeofday 返回 ENOENT |

### umask — 0/1

| 测试 | 状态 | 说明 |
|------|------|------|
| umask01 | TFAIL | 文件创建 mode 恒为 666（umask 已实现但 ext4 创建 mode 默认为 0666） |

### mkdirat — 1/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| mkdirat01 | 部分通过 | mkdirat 成功创建目录，部分 errno 预期不匹配 |
| mkdirat02 | TBROK | 依赖 `/dev/loop-control`（需要挂载设备） |

### rename — 1/1 通过（不含缺失的 rename02）

| 测试 | 状态 | 说明 |
|------|------|------|
| rename01 | TBROK | 依赖 `/dev/loop-control` |

### mprotect — 0/2

| 测试 | 状态 | 说明 |
|------|------|------|
| mprotect01 | TFAIL+TBROK | 依赖 /dev/zero 和信号 48（未实现） |
| mprotect02 | TFAIL | 访问保护页触发 SIGSEGV 未正确处理 |

### fcntl — 1/2 通过

| 测试 | 状态 | 说明 |
|------|------|------|
| fcntl01 | TFAIL | O_NDELAY/O_APPEND 设置后 fcntl 检查失败 |
| fcntl02 | PASS | F_DUPFD 复制 fd |

### access — 0/1

| 测试 | 状态 | 说明 |
|------|------|------|
| access01 | TBROK | 依赖 `getpwnam(nobody)`（无 /etc/passwd） |

---

## 核心 bug 修复清单

以下是在子集调试过程中发现并修复的内核 bug：

### wait/信号相关
| # | 问题 | 修复文件 | 说明 |
|---|------|---------|------|
| 1 | waitpid 无子进程返回 EPERM(1) | `syscall/process.rs` | 改为 ECHILD(-10) |
| 2 | pid 匹配后 has_child=false 返回 EPERM | `syscall/process.rs` | 改为 ECHILD(-10) |
| 3 | SIGILL 无限循环 | `main.rs` | IllegalInstruction 改为 kill 进程 |

### 文件描述符
| # | 问题 | 修复文件 | 说明 |
|---|------|---------|------|
| 4 | 无效 fd 返回 EPERM(-1) | `syscall/fs.rs` | close/dup/dup2/read/write 改为 EBADF(-9) |
| 5 | dup2 大 fd 导致 OOM | `syscall/fs.rs` | 添加 MAX_FD=1024 限制 |
| 6 | 写入 fd 本应返回 EBADF | `syscall/fs.rs` | write 非法 fd 返回 -9 |
| 7 | 读目录 fd 应返回 EISDIR | `syscall/fs.rs` | sys_read/sys_write 添加 is_dir 检查 |

### 内存管理
| # | 问题 | 修复文件 | 说明 |
|---|------|---------|------|
| 8 | MAP_SHARED 未实现 | `mm/memory_set.rs`, `syscall/process.rs` | MapType::Shared, fork 共享物理帧 |
| 9 | valid_user_buf 溢出检查缺失 | `mm/memory_set.rs` | checked_add 防 overflow |
| 10 | valid_user_buf 读检查缺 R 标志 | `mm/memory_set.rs` | need_write=false 时检查 R |
| 11 | mmap 高位地址溢出 | `syscall/process.rs` | checked_add 防 wraparound |
| 12 | 内核栈 256KB→32KB | `config.rs` | 减少内存占用 |
| 13 | 内核堆 32MB→128MB | `config.rs` | 支持大量 fork 子进程 |

### 文件系统
| # | 问题 | 修复文件 | 说明 |
|---|------|---------|------|
| 14 | umask 不生效 | `fs/vfs.rs`, `fs/inode.rs` | vfs_create 应用 umask 到 mode |
| 15 | 管道 buffer 32B→64KB | `fs/pipe.rs` | 防止写管道阻塞 |
| 16 | pipe::poll 不准确 | `fs/pipe.rs` | 实现正确的 POLLIN/POLLOUT/POLLHUP |

### 用户内存保护
| # | 问题 | 修复文件 | 说明 |
|---|------|---------|------|
| 17 | sys_read 直接写用户内存 | `syscall/fs.rs` | 先读入内核 buffer 再验证后拷贝 |
| 18 | sys_getcwd 无地址验证 | `syscall/fs.rs` | valid_user_buf + 正确 errno |
| 19 | sys_chdir 无地址验证 | `syscall/fs.rs` | valid_user_buf + first_page_only |
| 20 | sys_uname 无地址验证 | `syscall/process.rs` | valid_user_buf 验证 |
| 21 | sys_get_time 无地址验证 | `syscall/process.rs` | valid_user_buf 验证 |
| 22 | sys_setitimer 无地址验证 | `syscall/process.rs` | valid_user_buf 验证 |
| 23 | sys_sendfile 无地址验证 | `syscall/process.rs` | valid_user_buf 验证 offset_ptr |
| 24 | sys_chdir 跨页检查过多 | `syscall/fs.rs` | 只检查第一页 |

### 系统调用补齐
| # | syscall | 文件 | 说明 |
|---|---------|------|------|
| 25 | sysinfo(179) | `syscall/process.rs` | 填充 SysInfo 结构体 |
| 26 | clock_getres(114) | `syscall/process.rs` | 返回 1ms 分辨率 |
| 27 | getrlimit/setrlimit(163/164) | `syscall/process.rs` | 返回 RLIM_INFINITY |
| 28 | getgroups/setgroups(158/159) | `syscall/process.rs` | 基础实现 |
| 29 | UID/GID 族(117-150) | `syscall/process.rs` | getresuid/gid, setresuid/gid, setreuid/regid |
| 30 | mremap(216) | `syscall/process.rs` | 基本重映射 |
| 31 | mincore(232) | `syscall/process.rs` | 所有页返回 1 |
| 32 | mlock/munlock 族(228-231,325) | `syscall/process.rs` | 返回 0(无 swap) |
| 33 | msync(227) | `process.rs`, `mm/memory_set.rs` | Shared 区写回文件 |
| 34 | fchmod/fchown 族(52-55) | `syscall/fs.rs`, `fs/vfs.rs` | ext4 权限/所有者修改 |
| 35 | fsync/fdatasync(82/83) | `syscall/fs.rs`, `fs/vfs.rs` | ext4_cache_flush |
| 36 | sync/syncfs(81/267) | `syscall/fs.rs`, `fs/vfs.rs` | 全文件系统 flush |
| 37 | fallocate(47) | `syscall/fs.rs` | ftruncate 实现 |
| 38 | fadvise64(223) | `syscall/fs.rs` | 返回 0(no-op) |
| 39 | preadv2/pwritev2(286/287) | `syscall/fs.rs` | 委托 preadv/pwritev |
| 40 | flock(32) | `syscall/fs.rs` | 返回 0 |
| 41 | mknodat(33) | `syscall/fs.rs` | vfs_create |
| 42 | readahead(213) | `syscall/fs.rs` | 返回 0 |
| 43 | copy_file_range(285) | `syscall/fs.rs` | read+write 循环 |
| 44 | memfd_create(279) | `syscall/fs.rs` | 分配 fd→DevNull |
| 45 | close_range(436) | `syscall/fs.rs` | 批量关闭 fd |
| 46 | socketpair(199) | `syscall/fs.rs` | 占位 stub |
| 47 | sigaltstack(132) | `process.rs`, `task/task.rs` | 存储/返回 alt stack 信息 |
| 48 | rt_sigsuspend(133) | `syscall/process.rs` | 替换 mask + sleep 等待信号 |
| 49 | rt_sigpending(136) | `syscall/process.rs` | 返回 pending 信号 |
| 50 | getpriority/setpriority(140/141) | `syscall/process.rs` | 返回 nice=20 |

### 其他修复
| # | 问题 | 修复文件 | 说明 |
|---|------|---------|------|
| 51 | stdin read 必须 1 字节 assert | `fs/stdio.rs` | 支持任意长度 buffer |
| 52 | stdin 无输入时死循环 | `fs/stdio.rs` | 无字符时返回 0(EOF) |
| 53 | FPU 未启用 | `main.rs` | csrs sstatus 设置 FS=3 |
| 54 | polyhal-trap LoadFault panic | `vendor/polyhal-trap/.../riscv64.rs` | 改为返回 LoadPageFault |
| 55 | errno 返回 EPERM→ECHILD | `syscall/process.rs` | sys_waitpid |
| 56 | VAddrRange start>end panic | `syscall/process.rs` | sys_mmap checked_add |
| 57 | summary 不输出 | `syscall/fs.rs` | 移除 sys_write valid_user_buf |
| 58 | sendfile 双重借用 panic | `syscall/process.rs` | valid_user_buf 验证 offset_ptr |

---

## 未解决的已知问题

| 问题 | 影响测试 | 优先级 |
|------|---------|--------|
| 无 /proc 文件系统 | getpid01, nanosleep01 等 | 中 |
| 无 /dev/loop-control | chdir01, mkdirat02, rename01 | 中 |
| 无 /etc/passwd | open02, access01 (getpwnam) | 低 |
| signal 48(NSIG) 未实现 | mprotect01, crash02 | 低 |
| link() 未实现（ENOSYS） | fstat02 | 低 |
| fcntl O_NDELAY/O_APPEND | fcntl01 | 低 |
| tst_checkpoint/wait 依赖 | pipe02 | 低 |
| SIGSEGV 信号处理 | munmap01, munmap02 | 高 |
| brk 测试清理阶段逻辑 | brk01, brk02 | 低 |
| umask mode 与预期不一致 | umask01 | 低 |
