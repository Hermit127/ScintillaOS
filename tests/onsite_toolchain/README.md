# 六工具现场赛回归

本目录使用官方镜像中未经修改的 GNU Make、CMake、Ninja、Cargo、Python 3.13 和 curl。测试不以版本输出代替实际功能，而会真实生成工程、并行编译、链接、运行、执行测试、下载固定 HTTPS 内容并向宿主提交唯一回执。

模式：

- `versions`：六工具加载与版本输出；
- `build`：Make、CMake+Make、Ninja、CMake+Ninja；
- `cargo`：离线新建、release 编译、运行、测试和 metadata；
- `python`：数据处理、SQLite、线程和 venv；
- `curl`：证书校验 HTTPS、固定哈希和宿主 POST 副作用；
- `all`：以上全部，并增加 Make→Python→CMake→Ninja→Cargo 组合构建。

正式入口由 `scripts/verify_onsite_toolchain.py` 和 Makefile 提供。运行证据保存在被 Git 忽略的 `artifacts/onsite-toolchain/`。

完整题目、实现原理、自动及手工复测命令见
[`documents/六工具现场赛模拟题适配与验收指南.md`](../../documents/六工具现场赛模拟题适配与验收指南.md)。
