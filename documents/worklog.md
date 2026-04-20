2026/04/20 Hermit
添加fstat功能：
为文件生成Kstat结构体并返回该结构体到stat_buf地址
(其中,Kstat的部分参数是默认值，没有填入真实值)
目前没有做stat_buf的指针安全性检查