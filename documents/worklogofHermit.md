2026/04/20 Hermit
添加sys_fstat 功能(不成熟)：
为文件生成Kstat结构体并返回该结构体到stat_buf地址

其中,Kstat的部分参数是默认值，没有填入真实值
目前没有做stat_buf的指针安全性检查

2026/04/21 Hermit
添加sys_getdents64 功能：读取文件的目录项，并返回到userbuf
添加sys_sleep
添加dup2

Todo: 跑一边basic的样例，看看原操作系统实现的系统调用如何

2026/04/23 Hermit
添加getppid

2026/04/27 Hermit
修改fork和clone的调用号,使之适配于测试用例
修复 panic 问题：未识别 syscall 从 panic 改为返回 -1
查看basic 测试操作流程文档：documents/basic_test_operational_flow.md
加 SYSCALL_CHDIR=49 SYSCALL_GETCWD=17 常量 + sys_chdir sys_getcwd 函数

2026/04/29 Hermit
basic 测试适配：
exec() 栈布局修复：push argc到栈顶，trap_cx.sp指向argc，兼容C crt.S
sys_pipe2 从写usize(8B)改为u32(4B)，Rust侧pipe fd数组同步改为[u32;2]，修复C int fd[2]被覆盖
编写 basic_runner.rs 一键测试，user_shell加cd/.路径支持