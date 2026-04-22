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