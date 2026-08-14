# 2025 现场赛 RISC-V 验收

本目录的回归测试面向官方 RISC-V Alpine Ext4 镜像。测试不会修改原始
raw 镜像；两个自动验证程序都会创建临时 qcow2 覆盖层，并在退出前执行
`qemu-img check`。

## 前置条件

- `qemu-system-riscv64` 和 `qemu-img`
- 仓库根目录下的 `disk.img`
- 默认位置 `../alpine-linux-riscv64-ext4fs.img` 的官方镜像，或通过
  `ON_SITE_2025_DISK=/path/to/image.img` 覆盖

## 自动验证

```sh
# 构建默认 release 内核，并在一个全新覆盖层中完成官方离线 70 分流程：
# Git 帮助及本地流程、Vim 帮助及真实交互编辑、GCC/Rustc 帮助及编译运行。
make verify-onsite-2025-vim

# 编译并运行 POSIX timer、TTY ioctl 和 Ext4 覆盖重命名回归。
make verify-onsite-2025-regressions

# 代理中继运行时，额外验证 Git/libcurl 依赖的 getsockname 网络路径。
make verify-onsite-2025-network

# 通过 VirtIO 网卡和代理完整克隆官方 xv6，并检查 HEAD、工作树和镜像。
make verify-onsite-2025-git-clone
```

真实交互 Vim 测试通过串口发送普通编辑按键，使用 `:startinsert` 进入插入
模式并用 `:wq` 保存；它不会预先创建目标文件，也不使用 `vim -es`。

## 可留档启动

```sh
make run-onsite-2025-rv
```

该目标强制重新构建 RISC-V release 内核，以 `-smp 1 -m 1G`、双 VirtIO
块设备和 VirtIO 网卡启动。串口日志、网络 pcap、QEMU 命令、Git 提交号及
镜像/内核哈希保存在 `artifacts/onsite2025/<UTC时间>/`。该目录被 Git
忽略，避免提交磁盘覆盖层和可能含认证输入的日志。

进入 Alpine 后的基本网络验收顺序为：

若宿主机通过 `127.0.0.1:7897` 代理访问 GitHub，先在另一个宿主机终端运行
只监听 loopback、且不记录传输内容的中继：

```sh
python3 scripts/onsite_2025_proxy_relay.py
```

```sh
printf 'nameserver 10.0.2.3\n' > /etc/resolv.conf
export http_proxy=http://10.0.2.2:27898
export https_proxy=$http_proxy
getent ahostsv4 github.com
git clone https://github.com/oscomp/xv6-riscv.git /tmp/xv6-riscv
git -C /tmp/xv6-riscv status --short
git -C /tmp/xv6-riscv rev-parse HEAD
```

GitHub push/pull 必须由仓库所有者手工验收。使用交互式凭据提示输入
fine-grained PAT，不要把令牌放入 URL、脚本、Git 配置或串口日志。未经
明确授权，自动验证程序不会执行 push。
