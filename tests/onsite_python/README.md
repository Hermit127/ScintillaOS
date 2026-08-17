# Python 3.13 现场赛回归

本目录验证官方 2026 Ext4 镜像中未经修改的 Python 3.13。测试分为 `core`、`fs`、`concurrency`、`network` 和 `all`，分别覆盖解释器/标准库、Ext4/SQLite、进程线程/asyncio，以及经过 VirtIO-net 的 DNS、HTTPS 和远端写入副作用。

测试标记只有在结果、退出码和副作用均已由脚本检查后才输出。正式运行由 `scripts/verify_onsite_python.py` 创建全新 qcow2 覆盖层，raw 官方镜像不会被修改。

常用入口：

```bash
make verify-onsite-python-core PYTHON_ONSITE_SMP=4
make verify-onsite-python-fs PYTHON_ONSITE_SMP=4
make verify-onsite-python-concurrency PYTHON_ONSITE_SMP=4
make verify-onsite-python-network PYTHON_ONSITE_SMP=4
make verify-onsite-python-all PYTHON_ONSITE_SMP=4
```

网络模式需要先启动现有 HTTPS 代理中继：

```bash
make onsite-2025-proxy-relay
```

LoongArch 使用官方 Docker 中的 QEMU：

```bash
make verify-onsite-python-la-docker PYTHON_ONSITE_MODE=all
```

拟题、内核适配原理、手工复现、通过证据和已知边界见
`documents/Python3.13现场赛模拟题适配与验收指南.md`。
