你当前的测试集合来自 run-all.sh，一共这些 syscall 相关程序：
brk, chdir, clone, close, dup2, dup, execve, exit, fork, fstat, getcwd, getdents, getpid, getppid, gettimeofday, mkdir_, mmap, mount, munmap, openat, open, pipe, read, sleep, times, umount, uname, unlink, wait, waitpid, write, yield。

目前的evec和linux execve存在区别 
目前还没有实现的：
brk
chdir
clone
dup2
getcwd
getppid
mkdir
mmap
munmap
openat

unlink
wait



你的内核现状入口在 mod.rs、fs.rs、process.rs。

重点先提醒一个大坑：
在 basic 的 riscv syscall 号里，dup 是 23、dup3 是 24，定义在 syscall_ids.h.in。
你当前 mod.rs 里把 dup 写成了 24，这会直接错位。

下面按功能组讲“含义 + 最小可过实现”。

文件描述符与文件 I/O

open/openat
含义：打开文件，openat 支持目录 fd + 相对路径。
最小实现：
open 可以当作 openat(AT_FDCWD, path, flags, 0) 的语法糖。
openat 至少支持 dirfd=AT_FDCWD 和 dirfd 指向目录这两种。
O_CREATE、O_RDWR、O_DIRECTORY 要识别（常量见 stddef.h）。
用例参考 open.c、openat.c。
read/write
含义：对 fd 读写字节流。
最小实现：
返回实际读写字节数，错误返回 -1。
管道和普通文件都能走同一 fd 表。
你已有雏形在 fs.rs。
close
含义：关闭 fd。
最小实现：fd 合法且已占用时返回 0，否则 -1。
用例 close.c。

dup/dup2(dup3)
含义：复制 fd；dup2(old,new) 指定目标号。
最小实现：

dup: 找最小空闲 fd。
dup2: 若 old 无效返回 -1；old==new 直接返回 new；否则先关 new 再复制。
basic 里测试程序叫 dup2，底层常映射到 dup3。
用例 dup2.c。
pipe(pipe2)
含义：创建读端/写端 fd。
最小实现：返回两个 fd，父子进程可通过 read/write 通。
用例 pipe.c。

fstat
含义：读取文件元数据到 kstat。
最小实现：至少填 st_mode、st_size，其他可给合理默认值，返回 0。
结构见 stddef.h，用例 fstat.c。

getdents64
含义：读取目录项。
最小实现：

至少返回一个合法 linux_dirent64 结构，d_reclen 对齐，d_name 非空。
失败返回 -1。
用例 getdents.c。


mkdirat/mkdir、unlinkat/unlink、chdir、getcwd
含义：目录与路径管理。
最小实现：
进程维护 cwd。
chdir 成功返回 0。
getcwd(buf,size) 返回 buf，失败返回 NULL。
mkdir 创建目录后可被 open(O_DIRECTORY) 打开。
unlink 删除目录项后再次 open 失败。
用例 chdir.c、getcwd.c、mkdir_.c、unlink.c。



进程与调度

fork
含义：复制当前进程。
最小实现：父返回子 pid，子返回 0。你已有实现框架在 process.rs。

clone
含义：更灵活创建进程/线程。
basic 最小可过策略：先做 fork 兼容语义（至少返回 pid 且可 wait 回收），后续再补 flag 细节。
用例 clone.c。

execve
含义：用新程序替换当前进程映像。
最小实现：加载成功不返回；失败返回 -1。你已有基础在 process.rs。
用例 execve.c。

wait/waitpid(wait4)
含义：等待子进程并取退出码。
最小实现：

wait 等价 waitpid(-1,...,0)。
waitpid 返回子 pid；无子进程返回 -1。
正确写入 wstatus，WEXITSTATUS 可读出。
用例 wait.c、waitpid.c。
exit
含义：退出当前进程并触发父进程 wait 可见。你已有。

getpid/getppid
含义：取当前 pid/父 pid。
最小实现：都应大于 0。getppid 不能固定 0。
用例 getppid.c。

sched_yield
含义：主动让出 CPU。
最小实现：触发调度并返回 0。
用例 yield.c。

内存与时间

brk
含义：进程堆顶管理。
最小实现：
brk(0) 返回当前 break。
brk(new) 成功后返回新 break。
用例 brk.c。
mmap/munmap
含义：建立/解除内存映射。
最小实现：
至少支持文件映射 MAP_SHARED|PROT_READ|PROT_WRITE，offset=0。
失败返回 MAP_FAILED。
用例 mmap.c。
gettimeofday/sleep/times
含义：时间读取、睡眠、进程时间统计。
最小实现：
gettimeofday 或 get_time 返回递增时间。
sleep(1) 约等于 1 秒后返回 0。
times 返回非负并写入 tms 结构。
用例 gettimeofday.c、sleep.c、times.c。
系统信息与挂载

uname
含义：填充 utsname。
最小实现：返回 0，字段填非空字符串即可。
用例 uname.c。

mount/umount2
含义：挂载和卸载文件系统。
最小实现：

对测试里的路径和设备返回成功并维护挂载状态。
mount 后 umount 要能成功。
用例 mount.c、umount.c。
实现上的三条硬建议

先修 syscall 号对齐
对照 syscall_ids.h.in 重排 mod.rs。

未实现 syscall 不要 panic
现在默认 panic 会中断整轮测试。改成返回 -ENOSYS 或 -1，保证测试继续跑并能看到完整缺口。

先做最小可通过语义，再做完整语义
比如 clone、mount、times 可以先做兼容层，先让 basic 通过，再逐步完善。