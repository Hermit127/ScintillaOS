# 2025 现场赛 RISC-V 验收指南

本指南只覆盖已经实现和实测的 QEMU RISC-V64 路径，不把结果外推到 LoongArch 或实体开发板。基础镜像默认位于项目相邻目录：

```text
../alpine-linux-riscv64-ext4fs.img
```

官方压缩包的已核验 SHA-256 为：

```text
0def50e343fa287b86c89346b53a835405c300fe45744a664833a4fbdb378a1c
```

## 自动回归

以下命令都会先构建最新 release 内核，并从只读 raw 基础镜像创建临时 qcow2 覆盖层：

```bash
make verify-onsite-2025-vim
make verify-onsite-2025-regressions
make verify-onsite-2025-network
```

基础回归会检查 termios `ICRNL`：开启时普通回车 `CR` 转成程序所需的 `LF`，关闭时保留原始 `CR`。它还会在规范模式下分别以 `DEL` 和 `Ctrl+H` 作为 `VERASE` 删除前一个字符并检查擦除回显，同时确认非规范模式把 `DEL` 原样交给 Vim 等应用。网络回归会启动只监听 `127.0.0.1:27899` 的一次性服务，检查未绑定和已连接 socket 的 `getsockname`，并检查阻塞与非阻塞 `recv` 在 TCP FIN 后都返回 0。

完整 Git clone 需要先在一个终端启动宿主代理中继：

```bash
make onsite-2025-proxy-relay
```

中继只监听 `127.0.0.1:27898`，默认转发到宿主 `127.0.0.1:7897`，不记录载荷。每个方向使用最大 32 MiB 的有界缓冲，避免慢速 QEMU 对宿主代理施加长时间反向背压。然后在另一终端运行：

```bash
make verify-onsite-2025-git-clone
```

只有看到 `GIT_CLONE_RC=0`、有效提交号、`GIT_CLONE_TREE_OK` 和 `qemu-img check` 无错误才算 clone 通过。

## 手工 push 和 pull 验收

先在终端 A 保持代理中继运行：

```bash
make onsite-2025-proxy-relay
```

在终端 B 启动带持久验收覆盖层和完整日志的 QEMU：

```bash
make run-onsite-2025-rv
```

进入 Rust shell 后挂载 Alpine，并配置由 DHCP 返回的 DNS 和宿主代理：

```sh
/busybox mount -t ext4 vda /mnt
/busybox chroot /mnt /bin/sh
export PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
printf 'nameserver 10.0.2.3\n' > /etc/resolv.conf
export http_proxy=http://10.0.2.2:27898
export https_proxy=$http_proxy
export GIT_PAGER=cat
```

完整克隆、提交并配置个人远程：

```sh
rm -rf /tmp/xv6-riscv
git clone https://github.com/oscomp/xv6-riscv.git /tmp/xv6-riscv
cd /tmp/xv6-riscv
git config user.name 'lyzhcm'
git config user.email 'lyzhcm@users.noreply.github.com'
printf '\nONSITE_PUSH_2025_OK\n' >> README
git add README
git commit -m 'update README for onsite 2025'
git remote add me https://github.com/lyzhcm/xv6-riscv-onsite25.git
git push -u me HEAD:main
```

GitHub 用户名输入 `lyzhcm`；密码提示处交互输入仅授予该仓库 Contents 读写权限的 fine-grained PAT。不得把 PAT 写入 URL、脚本、Git 配置或验收日志。push 通过后，在宿主执行以下只读核验，网页显示的提交号也必须相同：

```sh
git rev-parse HEAD
git ls-remote me refs/heads/main
```

随后在 GitHub 网页编辑 README，新增唯一一行 `ONSITE_PULL_2025_OK` 并提交。回到客体执行：

```sh
git pull me main
grep -Fx ONSITE_PULL_2025_OK README
git status --short
```

只有 pull 退出码为 0、`grep` 找到唯一标记且工作区干净，才算 pull 通过。

## 证据与退出

`make run-onsite-2025-rv` 会在 `artifacts/onsite2025/<UTC 时间>/` 保存基础镜像哈希、内核哈希、当前提交号、QEMU 命令、串口日志、pcap 和 qcow2 覆盖层。退出 QEMU 使用 `Ctrl-A` 后按 `X`。最后检查对应覆盖层：

```bash
qemu-img check artifacts/onsite2025/<UTC 时间>/alpine-overlay.qcow2
```
