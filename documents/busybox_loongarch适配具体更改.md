LoongArch64 适配与 BusyBox/glibc 支持总结

本次修改主要围绕 LoongArch64 启动、块设备、BusyBox 测试脚本、glibc BusyBox 运行稳定性和 COW 内存管理展开。最终 LoongArch64 下 musl/glibc BusyBox 基础测试可以继续运行，之前的卡死、脚本生成错误、kill/sleep 失败和内核堆 panic 均已定位并修复。

1. LoongArch64 启动与 QEMU 运行

为 LoongArch64 增加了独立启动入口，绕过原有 polyhal_boot::define_entry 在该架构下不适配的问题。
初始化 LoongArch64 必要 CSR、启动栈、DTB、percpu、FPU、平台设备和 HAL driver。
调整 LoongArch64 QEMU 参数，使用 PCI virtio-blk 设备，解决原先块设备无法正常挂载的问题。
修复 QEMU -serial stdio 冲突和重复 -m 参数问题。
LoongArch64 内存配置提高到更适合 glibc 测试的容量，避免大型用户程序运行时过早耗尽物理页。
2. BusyBox 测试脚本生成修复

修复 Makefile 中生成 busybox_testcode.sh 和 busybox_glibc_testcode.sh 时 $line、$?、$RTN 被 make 提前展开的问题。
将脚本变量统一改成 $$line、$$?、$$RTN，保证进入 rootfs 后脚本内容正确。
调整脚本执行逻辑，统一使用 eval "./busybox $line" / eval "./busybox-glibc $line"，保证带引号、管道、重定向的命令可以按预期执行。
移除或覆盖原先会把 tmp/busybox_glibc_testcode.sh 重新拷回 rootfs 的逻辑，避免生成好的 glibc 脚本被旧文件覆盖。
3. BusyBox syscall 与基础功能修复

修复 statx 和 epoll_create1 syscall 编号冲突，补充 statx 的基本实现，解决 BusyBox/glibc 对新接口的依赖。
修复 du、which、ls 等命令依赖的目录读取和文件状态相关行为。
修复 kill 对 SIGTERM/SIGKILL 的处理，收到终止信号时正确设置任务 killed 状态。
修复 sleep/nanosleep 在被 kill 后仍不退出的问题，使测试项：
sh -c 'sleep 5' & ./busybox kill $!
可以通过。
修复 open/write/append/truncate/stat/getdents 等文件系统路径上的若干兼容问题，使 BusyBox 文件操作测试可以正常完成。
4. glibc BusyBox 与 COW fork 修复

原始 fork 会完整复制 glibc BusyBox 地址空间，导致 LoongArch64 下快速耗尽物理页。
引入 Copy-On-Write fork：
fork 时父子进程共享物理页；
对原本可写页清除 PTE 写权限；
写缺页时根据物理页引用计数决定恢复写权限或复制新页。
修复 COW 元数据错误：MapArea.map_perm 必须保留原始写权限，不能因为 PTE 临时只读而删除逻辑写权限。
修复 COW refcount 设计：
维护物理页共享引用计数；
禁止出现活跃映射但 refcount 为 0 的状态；
避免 child exit 释放 parent 仍在使用的物理页。
修复 LoongArch64 下 COW/TLB 更新后父子进程地址空间互相破坏导致的卡死问题。
5. clone / wait / futex 相关兼容

修正 LoongArch64/RISC-V Linux ABI 下 clone(flags, stack, ptid, ctid, tls) 参数语义。
补充 CLONE_CHILD_CLEARTID、CLONE_CHILD_SETTID 的基础处理。
改进 waitpid：
支持 WNOHANG；
避免递归等待；
正确处理 zombie child 回收和父进程唤醒。
补充 futex wait/wake 的基本行为，满足 glibc pthread/child_tid 清理路径的最低需求。
6. 内核堆 panic 修复

glibc BusyBox 体积较大，sys_exec 中原先通过 read_all() 反复扩容 Vec 读取整个 ELF，容易触发 4MiB 连续内核堆分配失败：
Heap allocation error, layout size = 0x400000。
优化 ELF 读取路径：
根据 inode size 预分配 buffer；
增大单次读取块大小；
减少反复扩容和 kernel heap 碎片。
清理调试阶段引入的大量 DBG 输出，避免逐字节 stdout 打印严重拖慢 LoongArch64 QEMU。
7. 当前状态与剩余问题

LoongArch64 下 BusyBox/glibc BusyBox 基础命令测试已经可以继续运行并通过主要测试项。
相比 RISC-V，LoongArch64 运行仍明显更慢，主要原因包括：
macOS/arm64 上通过 amd64 Docker 再跑 LoongArch64 QEMU，模拟开销很大；
glibc BusyBox 体积更大，fork/exec/COW 压力明显高于 musl；
ext4 与用户程序频繁 exec/read/write 放大了 QEMU TCG 的性能差异。
后续可继续优化：
减少 glibc BusyBox 反复整文件读取，可考虑 ELF 缓存或按需加载；
优化 COW refcount 数据结构；
降低 ext4 小块读写和目录操作开销；
优化 stdout/console 输出路径。