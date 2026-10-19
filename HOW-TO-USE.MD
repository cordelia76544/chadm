# chadm 详细使用手册

本手册介绍当前代码已实现的功能，示例面向使用 systemd 的 Linux 宿主机。阶段文档保留了开发时的历史行为；日常操作以本手册、当前程序的 `--help` 和对应专题文档为准。

chadm 使用本地 TOML 元数据、systemd 模板、Cloud Hypervisor API 和 Linux 原生网络/存储能力。默认以 root 管理资源，没有数据库或常驻 chadm daemon。

目录：

- [基本概念与约定](#基本概念与约定)
- [首次安装与宿主机配置](#首次安装与宿主机配置)
- [NixOS 部署](#nixos-部署)
- [快速启动一台普通文件磁盘虚拟机](#快速启动一台普通文件磁盘虚拟机)
- [管理存储仓库](#管理存储仓库)
- [导入和管理基础镜像](#导入和管理基础镜像)
- [创建实例与选择启动方式](#创建实例与选择启动方式)
- [网络与 IP 地址](#网络与-ip-地址)
- [cloud-init raw seed](#cloud-init-raw-seed)
- [Windows 模板与实例](#windows-模板与实例)
- [数据卷与冷热挂载](#数据卷与冷热挂载)
- [实例启停与串口控制台](#实例启停与串口控制台)
- [快照与落盘挂起](#快照与落盘挂起)
- [查看配置与脚本化](#查看配置与脚本化)
- [删除资源与引用关系](#删除资源与引用关系)
- [故障排查与恢复](#故障排查与恢复)
- [更新程序与备份](#更新程序与备份)
- [命令速查与功能边界](#命令速查与功能边界)

## 基本概念与约定

| 资源 | 含义 | 示例 |
| --- | --- | --- |
| repo | 存储后端及其受管位置 | file repo `local`、ZFS repo `fast` |
| image | 不可写的基础镜像，存储格式为 raw | `ubuntu-test`、`windows-template` |
| volume | 实例可写的独立磁盘 | `test01-root`、`extra` |
| instance | 计算、启动、卷引用及网卡配置 | `test01` |
| cloudinit repo | 专门保存只读 raw seed 的 file repo | 名称固定为 `cloudinit` |

`driver` 属于 repo；volume 和 image 通过 `--repo` 选择存储位置。比如 `volume create --repo fast`，不使用 `volume --driver zfs`。

创建实例会从 image 生成独立 root volume，VM 写入不会修改基础 image。多个实例可引用同一 image；一个可写 volume 同时只能归属一个实例。image、volume、repo、instance 的名称各自在对应资源类型中全局唯一，不以 repo 划分命名空间。

本手册使用以下示例环境，按实际情况替换路径和名称：

| 项目 | 示例值 |
| --- | --- |
| 源码目录 | `~/chadm` |
| chadm 安装位置 | `/usr/local/bin/chadm`，共享模板固定使用此位置 |
| Cloud Hypervisor | 已安装的 `cloud-hypervisor` |
| bridge | 已配置好的 `virbr0` |
| file repo | `local`，路径 `/var/lib/chadm/storage` |
| ZFS repo | `fast`，dataset 路径 `tank/chadm` |
| 基础镜像 | 本地离线镜像 `./ubuntu.img`，导入名称 `ubuntu-test` |

各章节的创建命令是按需选择的示例，不是一份需要从头到尾重复执行的脚本。资源已存在时先执行对应的 list/show；create 不会覆盖已有同名资源。

名称采用 `[a-z][a-z0-9-]{0,62}`：小写字母开头，后续只允许小写字母、数字和短横线。带网卡的实例还受 TAP 名称 15 字节限制，建议实例名不超过 9 个字符；例如 `test01`。实际 TAP 为 `ch-test01-0`，多网卡索引增加后也必须满足长度限制。

容量和内存使用正整数，支持 `B`、`K/KiB`、`M/MiB`、`G/GiB`、`T/TiB`；不带后缀表示字节。`20G` 等于 20 GiB，不支持 `1.5G`、`20GB` 或带空格的数值。

## 首次安装与宿主机配置

### 检查依赖

| 组件 | 用途 |
| --- | --- |
| Rust、Cargo | 编译 chadm |
| Cloud Hypervisor | 实际运行 VM；当前实现按 v52 CLI/API 核对 |
| `/dev/kvm` | 硬件虚拟化 |
| systemd system manager、cgroup v2 | 管理每个 VM 的进程和清理 |
| iproute2、`/dev/net/tun` | 管理 TAP 并加入现有 Linux bridge |
| `CLOUDHV.fd` | firmware boot 使用的兼容固件 |
| `qemu-img` | 导入 qcow2 时转换格式；仅导入 raw 不依赖它 |
| OpenZFS 工具和内核模块 | 使用 ZFS repo/zvol 时需要 |
| `jq` | 可选，用于处理 JSON 输出 |

宿主机需能够访问对应设备和 systemd。容器或受限沙箱中的 doctor 结果不代表宿主机的实际能力。普通用户运行 doctor 也可能因无设备权限而失败；以下宿主机资源操作统一使用 sudo。

### 编译和安装

在源码目录执行：

```bash
cd ~/chadm
cargo build --locked --release
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm
sudo install -m 0644 packaging/systemd/chadm-instance@.service /etc/systemd/system/chadm-instance@.service
sudo systemd-analyze verify /etc/systemd/system/chadm-instance@.service
sudo systemctl daemon-reload
/usr/local/bin/chadm version
```

依赖已经缓存时，构建可以使用 `cargo build --offline --locked --release`。运行 `chadm --help` 可查看全部公开命令。

只安装一份 `chadm-instance@.service`。实例 `test01` 对应 `chadm-instance@test01.service`，无需创建每实例 service 文件。模板的 helper 从实例 TOML 读取参数；不要修改模板内容、为实例复制 unit 或添加 drop-in 来传递启动参数，当前程序会校验模板是否匹配。

### 宿主机配置文件

默认读取 `/etc/chadm/config.toml`。该默认文件不存在时使用内置默认值；首次部署可以不创建它。

需要显式配置时，文件内容为：

```toml
data_dir = "/var/lib/chadm"
runtime_dir = "/run/chadm"
cloud_hypervisor = "cloud-hypervisor"
```

首次创建配置目录可以执行：

```bash
sudo install -d -m 0755 /etc/chadm
sudoedit /etc/chadm/config.toml
```

用编辑器写入上述 TOML 并保存。已有部署应沿用原来的 data/runtime 目录；改变路径不会迁移原资源。

Cloud Hypervisor 安装在非标准位置时，把 `cloud_hypervisor` 改为实际可执行文件的绝对路径。默认名称在固定系统目录查找，不使用用户自定义 PATH。firmware 路径由 data_dir 派生，不在 host config 中设置。

`--config` 可作为全局选项指定其他文件：

```bash
sudo chadm --config /etc/chadm/config.toml doctor
```

显式指定的配置文件必须存在。systemd 模板始终读取 `/etc/chadm/config.toml`；生命周期操作使用的目录和 Cloud Hypervisor 路径必须与服务配置一致，单独传 `--config` 不会改变模板读取的位置。

### 安装固定固件

在源码目录已有兼容 `CLOUDHV.fd` 时执行：

```bash
sudo install -d -m 0700 /var/lib/chadm
sudo install -m 0644 ./CLOUDHV.fd /var/lib/chadm/CLOUDHV.fd
```

使用自定义 data_dir 时，目标改为 `<data_dir>/CLOUDHV.fd`，文件名和大小写固定。文件需为 root 所有的非空普通文件，不使用 symlink，不能允许 group/other 写入。替换固件前先停止使用它的实例。

chadm 没有 `--firmware` 选项。选择固件启动用 `--boot firmware`；兼容别名 `fireware`，建议统一使用正确拼写。doctor 检查文件安全性和可读性，不证明其架构或 Guest 引导兼容性。

### 准备 bridge 并诊断

```bash
ip -d link show virbr0
sudo chadm doctor
```

bridge 由宿主机预先配置并持久化。chadm 管理 TAP，不创建 bridge、DHCP server、NAT 或宿主机 IP 配置。`virbr0` 存在也不代表其后面一定有 DHCP server；需要确认该网络能够给 Guest 分配地址。

缺少 firmware 或 qemu-img 可以显示 WARN，但相应启动/导入功能仍不可用。解决所需依赖后，再进入实例启动步骤。

## NixOS 部署

仓库提供基于 NixOS 26.05 的 flake 和 lock，以及可单独导入的 module。Nix 只管理宿主机软件、repo、固件、路径和网桥；实例的 CPU、RAM、磁盘、MAC、网卡和 seed 继续由 CLI/TOML 管理。

```nix
imports = [ /home/davyjones/chadm/nix/module.nix ];
virtualisation.chadm = {
  enable = true;
  repos.local.path = "/srv/chadm/storage";
  repos.fast = { driver = "zfs"; pool = "tank/chadm"; };
  bridges.virbr0.interfaces = [ ];
  # 已有 bridge 时省略 bridges；ZFS pool/内核/导入需宿主机提前配置。
  # 本地固件可设置 firmware.enable = true 和 firmware.source。
  # 自动下载则设置 firmware.github.rev 和 firmware.github.hash，详见部署文档。
};
```

module 通过一次性 `chadm-host-setup` 服务准备宿主机配置和 repo，默认用 options 维护普通文件 `/etc/chadm/config.toml`。重跑不会覆盖 VM 配置、磁盘或同内容固件；移除声明也不会删除存储。不要执行传统的 `/usr/local/bin`/systemd 模板手工安装步骤。

```bash
nix build .#chadm -L
sudo nixos-rebuild switch
sudo systemctl status chadm-host-setup.service
sudo chadm doctor
```

`dataDir`、`runtimeDir`、`manageHostConfig`、`repos`、`bridges`、固件 Release 固定版本/hash 及完整测试步骤见 [NixOS 部署说明](docs/nixos.md)。更新软件或固件前先恢复挂起实例并正常关机；不能依靠 NixOS generation rollback 回退虚拟磁盘。

## 快速启动一台普通文件磁盘虚拟机

下面使用已有 `./ubuntu.img`；实际输入按内容识别 raw/qcow2，不依赖扩展名。模板需支持 virtio 磁盘及网卡。启动条件与登录条件分别由固件/磁盘和 Guest 配置决定；未预置账号或 SSH 密钥的 cloud image，请先阅读 cloud-init 章节。

```bash
sudo chadm repo create --name local --driver file --path /var/lib/chadm/storage
sudo chadm image create --name ubuntu-test --path ./ubuntu.img --repo local

sudo chadm instance create \
  --name test01 --image ubuntu-test --repo local \
  --boot firmware --cpus 2 --ram 2G \
  --volume root,size=20G --volume data,size=30G \
  --net bridge=virbr0 --no-cloud-init

sudo chadm inspect test01
sudo chadm instance start test01
sudo chadm instance show test01
sudo chadm instance console test01
```

按 `Ctrl+]` 退出 console，VM 继续运行。串口没有输出时，先确认 Guest 配置了串口；Linux 常用内核参数为 `console=ttyS0`，交互登录还需要相应串口登录服务。

本例明确跳过 seed，便于独立验证 VM 启动。需要 cloud-init 初始化账户时，应在 create 时选择 seed，当前没有给已有实例更换 seed 的命令。

创建结果包括：

```text
实例配置：/var/lib/chadm/instances/test01/config.toml
root 卷：test01-root，复制 ubuntu-test 的内容
data 卷：test01-data，空白 30 GiB 数据盘
原始存储：/var/lib/chadm/storage/volumes/test01-root.raw
          /var/lib/chadm/storage/volumes/test01-data.raw
systemd：chadm-instance@test01.service
TAP：ch-test01-0，启动时创建、退出时清理
```

`instance create` 只生成配置和存储，不启动 VM；`instance start` 才准备 TAP 并通过 systemd 启动 Cloud Hypervisor。数据盘不会自动分区、格式化或挂载到 Guest 目录。

## 管理存储仓库

### File repo

```bash
sudo chadm repo create --name local --driver file --path /var/lib/chadm/storage
sudo chadm repo list
sudo chadm repo show local
sudo chadm repo show local --json
```

目标路径可以不存在，也可以是可验证的空目录；程序不接管非空目录。底层文件系统需要支持 `user.*` 扩展属性，用于验证文件归属。路径及父目录应受保护，不通过 `chmod 777` 解决权限错误。

典型内容：

```text
/var/lib/chadm/storage/
  .chadm-repo.toml
  images/ubuntu-test.raw
  images/ubuntu-test.chadm.toml
  volumes/test01-root.raw
  volumes/test01-root.chadm.toml
```

raw volume 使用 sparse file，逻辑大小不等于已占用空间。`volume show` 的容量/allocated 信息可以辅助观察；VM 后续写入仍需有实际可用空间。

### ZFS repo

只有已有 zpool `tank` 即可，不必手工创建 `tank/chadm`：

```bash
sudo zpool status tank
sudo chadm repo create --name fast --driver zfs --pool tank/chadm
sudo chadm repo show fast
```

`--pool tank/chadm` 指定的是 repo 的新 dataset 路径，父 filesystem 需存在。若指定更深的 `tank/vm/chadm`，需先由宿主机准备父 filesystem `tank/vm`。不要提前创建目标 repo dataset；chadm 拒绝接管已有 dataset。

chadm 创建并管理：

```text
tank/chadm                           repo filesystem
tank/chadm/volumes                   volume 容器 filesystem
tank/chadm/volumes/test02-root       root zvol，创建对应实例后存在
/dev/zvol/tank/chadm/volumes/test02-root
/var/lib/chadm/zfs-repos/fast/       repo filesystem 的受管挂载位置
```

image 在 ZFS repo 中仍以只读 raw 文件保存；volume 使用 zvol。ZFS repo 不接受 `--path`，挂载位置由 data_dir 派生。显式 zvol 容量需要按 16 KiB 对齐，`1G`、`20G` 等整 GiB 容量满足要求。当前 zvol 使用正常容量 reservation，创建前需要足够可预留空间。

已有 VG `ch` 暂不参与这些操作，当前没有 LVM driver。

### 删除 repo

```bash
sudo chadm repo delete local
```

仅对确认不需要的空 repo 执行。先删除 image/volume 及其引用；有未知存储内容、归属不明对象或未完成事务时也会拒绝删除。file repo 删除会保留其根目录；ZFS repo 会删除经过归属/GUID 校验的受管 dataset，保留外部 pool 和父 filesystem。不使用递归 destroy 接管或清除其他数据。

## 导入和管理基础镜像

### Raw 和 qcow2

```bash
sudo chadm image create --name ubuntu-test --path ./ubuntu.img --repo local
sudo chadm image create --name debian13 --path ./debian13.qcow2 --repo local
sudo chadm image list
sudo chadm image show debian13
sudo chadm image show debian13 --json
```

只接收本地、离线的普通文件；当前没有 `--url`。导入会复制源文件，不修改原文件。qcow2 经 qemu-img 转为 raw，拒绝 backing chain、外部 data file 和加密 qcow2。源文件快照和转换会需要额外临时空间。

受管基础镜像保存为独立只读 raw，通常权限为 0400，不应手工写入。需要更新基础镜像时，用新名称导入新版本并为它创建实例。

### Qcow2 模板配合 zvol 实例磁盘

镜像 repo 和实例 volume repo 可以不同：

```bash
sudo chadm image create --name debian13 --path ./debian13.qcow2 --repo local
sudo chadm instance create \
  --name test02 --image debian13 --repo fast \
  --boot firmware --cpus 2 --ram 2G \
  --volume root,size=20G --net bridge=virbr0 --no-cloud-init
sudo chadm instance start test02
```

流程是 qcow2 → 只读 raw image → 独立 root zvol，实际设备为 `/dev/zvol/tank/chadm/volumes/test02-root`。复制 image 到 zvol 当前采用完整字节复制，不使用 ZFS snapshot/clone。

`qemu-img` 只作为格式转换工具，VM 进程由 Cloud Hypervisor 运行。

## 创建实例与选择启动方式

### 常用参数

| 参数 | 行为 |
| --- | --- |
| `--name` | 实例名称，必填 |
| `--image` | 已导入的基础 image 名称，必填 |
| `--repo` | 新建 root/data volume 的目标 repo；缺省使用 image 所在 repo |
| `--boot` | `firmware` 或 `direct`，默认 firmware |
| `--cpus` | 正整数，默认 2 |
| `--ram` | 内存，默认 `2G` |
| `--volume root,size=20G` | root 容量；缺省使用 image 虚拟容量，ZFS 按需对齐 |
| `--volume data,size=30G` | 创建额外空白卷，可重复 |
| `--net bridge=virbr0` | 创建网卡配置，可重复；未传时没有网卡 |
| `--cloud-init NAME` | 选择 cloudinit repo 中已有 seed |
| `--no-cloud-init` | 明确跳过 seed，与 `--cloud-init` 互斥 |

root 容量必须至少等于基础 image 的虚拟容量。增大 root 块设备不会自动扩展 Guest 分区或文件系统，需要在 Guest 内完成相应扩容操作。每实例最多 32 块可写盘，包括 root；还需满足实际 Cloud Hypervisor/Guest 的设备限制。

所有 `--volume` 新卷使用同一个目标 repo。需要把另一 repo 的卷接入实例时，先 `volume create`，再 `volume attach`。`instance create --volume` 不引用已有卷。

### Firmware boot

```bash
sudo chadm instance create \
  --name test03 --image ubuntu-test --boot firmware \
  --cpus 2 --ram 2G --volume root,size=20G \
  --net bridge=virbr0 --no-cloud-init
```

固件使用 `<data_dir>/CLOUDHV.fd`。基础镜像自身需有适合该固件的引导配置；安装固件文件不等于镜像一定能够启动。

### Direct kernel boot

仅用于支持直接引导的非 Windows 镜像：

```bash
sudo chadm instance create \
  --name direct01 --image ubuntu-test --boot direct \
  --kernel /var/lib/chadm/boot/vmlinux \
  --initramfs /var/lib/chadm/boot/initrd.img \
  --cmdline "root=/dev/vda1 console=ttyS0" \
  --cpus 2 --ram 2G --net bridge=virbr0 --no-cloud-init
```

先在宿主机准备这些实际文件。kernel 必须是 Cloud Hypervisor 支持的可引导格式，不能假定任意发行版的 `/boot/vmlinuz` 都兼容。kernel/initramfs 使用安全的普通文件，避免 symlink。`--kernel`、`--cmdline` 在 direct 模式必填，`--initramfs` 可省略。root 参数应与镜像的真实分区布局一致，`/dev/vda1` 只是示例。

direct boot 不依赖 CLOUDHV.fd。show/inspect 会隐藏 kernel cmdline 内容，排障时需在受保护的实例 TOML 中核对。

## 网络与 IP 地址

### DHCP 和 MAC

```bash
sudo chadm instance create \
  --name net01 --image ubuntu-test \
  --net bridge=virbr0 --no-cloud-init

sudo chadm instance create \
  --name net02 --image ubuntu-test \
  --net bridge=virbr0,mac=52:54:00:12:34:56,dhcp --no-cloud-init
```

`bridge=virbr0` 和 `bridge=virbr0,dhcp` 等价。DHCP 表示 Guest 向 bridge 后面的 DHCP server 获取地址；chadm 不启动 DHCP 服务。Guest 还需要配置网卡以使用 DHCP。

省略 MAC 时会生成 locally administered MAC 并保存，停止/启动不会重新生成。指定 MAC 时会检查格式和受管实例之间的冲突。未传 `--net` 时不会自动选择 bridge 或添加网卡。

多个网卡可以重复 `--net`，例如：

```bash
sudo chadm instance create \
  --name multinet --image ubuntu-test \
  --net bridge=virbr0 --net bridge=br1 --no-cloud-init
```

两个 bridge 都需由宿主机准备。每实例最多 16 个网卡配置；不支持通过 CLI 配置 static IP、Guest network-config、OVS、SR-IOV、vhost-user 或 vDPA。

### 给已有实例添加第二块网卡

目前没有网卡 attach/detach 命令，也没有网卡热插拔。已有实例需先完全停止，再编辑配置；操作期间不要同时执行其他管理命令。如果实例有磁盘快照，先删除这些快照；如果处于落盘挂起状态，先恢复并正常停止。快照保存完整配置，不能直接修改其中引用的实例网络。

```bash
sudo chadm instance stop test01
sudo chadm instance show test01
sudoedit /var/lib/chadm/instances/test01/config.toml
```

确认状态为 stopped 后，保留现有 `[[network]]`，在配置文件末尾追加：

```toml
[[network]]
bridge = "br1"
tap = "ch-test01-1"
mac = "52:54:00:12:34:57"
mode = "dhcp"
```

这里假设已有一块网卡，第二块的索引为 `1`。TAP 必须采用 `ch-<实例名>-<索引>`，不要手动创建；bridge 必须已经存在，也可以继续使用第一块网卡的 `virbr0`。手工编辑时需要自行选择唯一的 locally administered、单播 MAC，不能复制其他网卡的 MAC；自动生成并记录 MAC 是 `instance create --net` 的行为。

```bash
sudo chadm inspect test01
sudo chadm instance start test01
sudo chadm instance show test01
```

Guest 内仍需启用新网卡并配置 DHCP；具体接口名以 Guest 中的 `ip link` 为准。chadm 不会自动修改 Guest 网络设置，也不提供 DHCP server。

### TAP 生命周期

```text
Guest virtio-net → ch-test01-0 → virbr0 → 宿主机已有网络
```

start 验证 bridge，创建 TAP、设置 UP 并加入 bridge。VMM 退出时通过共享模板的清理入口删除受管 TAP；再次 start 会重建 TAP。遇到同名但不属于该实例的接口会报错，不会直接删除它。

### 查看 IP

```bash
sudo chadm instance show test01
sudo chadm instance show test01 --json | jq '.network[] | {bridge, tap, mac, ip_status, ip_addresses}'
```

IP 来自宿主机邻居表/FDB 的被动观测，并与 bridge、TAP、MAC 匹配；不是 Guest agent 上报，也不是 DHCP 租约查询。

| 显示 | 含义 |
| --- | --- |
| 一个或多个地址 | 已观测到匹配的 IPv4/IPv6 地址，不保证是完整或实时地址集合 |
| `unknown` | 当前没有发现匹配地址，不表示 Guest 一定没有 IP |
| `unavailable` | 观测查询失败，查看 reason |
| `N/A` | 实例停止，未查询运行时地址 |

已启动但 IP unknown 时，可先让 Guest 发出网络流量，并在 Guest 内查看地址，或检查外部 DHCP server 的租约。IP 观测失败不会把 VM 状态改成 stopped。

## cloud-init raw seed

### 创建专用 repo 并导入 seed

先准备兼容 Guest 的 NoCloud raw seed。它通常是包含 `user-data`、`meta-data` 的 FAT/ext 等磁盘文件，文件系统标签和内容需要符合 Guest 初始化工具的要求。chadm 不生成、编辑或验证内部配置。

```bash
sudo chadm repo create \
  --name cloudinit --kind cloudinit --driver file \
  --path /var/lib/chadm/cloudinit-storage

sudo chadm image create --name default --repo cloudinit --path ./cloudinit.raw
sudo chadm image show default
```

repo 名必须为 `cloudinit`，driver 只能是 file。省略 `--kind cloudinit` 时，保留名称也会被识别为该用途。该 repo 不保存普通可写 volume 或 VM 基础系统盘。

输入必须以 `.raw` 结尾且内容为 raw；不接受 ISO 或 qcow2，改后缀不能绕过格式检查。以 `--name default` 导入后，受管文件是 `<repo.path>/images/default.raw`。因此“不提供自定义名称时选 default”指 image 名称，不是任意放到目录中的未登记文件。

image 名称全局唯一；其他 repo 中若已有名为 `default` 的 image，需先处理该命名冲突。

### 默认选择、显式选择和跳过

| 实例创建条件 | 结果 |
| --- | --- |
| 无 cloudinit repo，未指定 seed | 不挂载 seed |
| 有 cloudinit repo，未指定 seed | 自动选择 `default`，缺失或不可用则创建失败 |
| 指定 `--cloud-init custom` | 选择 cloudinit repo 的 `custom` image |
| 指定 `--no-cloud-init` | 跳过 seed |

显式选择的示例：

```bash
sudo chadm image create --name custom --repo cloudinit --path ./custom.raw
sudo chadm instance create \
  --name seeded01 --image ubuntu-test --net bridge=virbr0 \
  --cloud-init custom
```

若要使用 default，create 时省略 `--cloud-init` 和 `--no-cloud-init` 即可。传入的是 seed 名称，不是 `.raw` 文件名或路径。

seed 在所有可写磁盘之后以只读磁盘接入；选择和 generation ID 保存到实例配置。默认选择仅发生在 create 时，创建 cloudinit repo 不会为已有实例补挂 seed，当前也不提供已有实例的 seed 更换或热插拔命令。

多个实例共享同一 seed 时，其中的 instance-id、hostname、凭据也会被复用。需要逐实例初始化内容时，分别准备并导入不同名称的 seed。不要在共享 default 中放入打算只给单个实例使用的身份配置。

Guest 中可以执行 `cloud-init status --long` 查看实际初始化结果。chadm 的 show/inspect 不输出 seed 内容；seed 有引用时不能删除。完整规则见 [cloud-init 专题](docs/cloud-init.md)。

## Windows 模板与实例

### 导入时标记

```bash
sudo chadm image create \
  --name windows-template --path ./windows.qcow2 --repo local --windows
sudo chadm image show windows-template
```

支持 raw 和 qcow2；`--windows` 会把布尔标记写入 image 元数据。模板应为已经安装好的 Windows 系统，支持 UEFI，具有 virtio-blk 和网络驱动。此标记设置虚拟硬件配置，不负责 Windows 安装、驱动注入、TPM 或远程登录设置。

### 继承 Windows profile

```bash
sudo chadm instance create \
  --name win01 --image windows-template --repo fast \
  --boot firmware --cpus 4 --ram 8G \
  --volume root,size=80G --net bridge=virbr0 --no-cloud-init
sudo chadm inspect win01
sudo chadm instance start win01
```

新实例自动继承并持久化 `windows = true`，CPU 参数变为：

```text
--cpus boot=4,max=4,kvm_hyperv=on,max_phys_bits=39
```

39 位物理地址是保守兼容默认值；CPU 数量和 RAM 仍使用用户指定值。profile 当前面向 x86_64/KVM，要求 firmware boot，拒绝 direct boot。固件仍为固定的 CLOUDHV.fd。

Windows 中的串口交互依赖事先启用 SAC/EMS，常规图形访问需要 Guest 预先配置相应远程服务。Windows 是否识别 seed 取决于其初始化工具；示例明确跳过 seed。

旧镜像和旧实例缺少 `windows` 字段时按 false 读取。已经导入但未标记的 Windows 模板可以用新名称重新导入并指定 `--windows`；已有实例不会追溯修改。详细参数依据与限制见 [Windows 专题](docs/windows.md)。

## 数据卷与冷热挂载

### 创建独立卷

```bash
sudo chadm volume create --name extra --repo local --size 10G
sudo chadm volume create --name zdata --repo fast --size 20G
sudo chadm volume list
sudo chadm volume show zdata --json
```

`extra` 是 raw sparse file，`zdata` 是 `tank/chadm/volumes/zdata` zvol。空卷没有分区或文件系统；attach 不会格式化数据，也不等于在 Guest 内挂载目录。

不能通过路径直接附加任意 `/dev/zvol/...` 或 block device。先用受管 repo/volume 创建并登记，然后按 volume 名 attach；当前不提供接管已有外部 zvol 的命令。

### 停止状态下挂载

已启动过的 test01 可以先正常停止：

```bash
sudo chadm instance stop test01
sudo chadm volume attach extra --instance test01
sudo chadm instance show test01
sudo chadm instance start test01
```

从未启动过且已确认 stopped 的实例可直接 attach，不必先执行 stop。附加卷可以来自与 root 不同的 repo，例如：

```bash
sudo chadm volume attach zdata --instance test01
```

运行中执行这条命令会使用下一节的热插拔流程。root 卷不能 detach；同一可写卷不能同时被多个实例引用。

### 运行状态下热插拔

```bash
sudo chadm volume create --name hotdata --repo fast --size 1G
sudo chadm volume attach hotdata --instance test01
```

test01 为 running 时，程序调用 Cloud Hypervisor API 并保存最终配置。Guest 需要支持对应的 virtio/PCI 热插拔；Linux 中用 `lsblk -o NAME,SIZE,TYPE,MOUNTPOINTS` 查看新盘，Windows 可用 `Get-Disk` 查看并按需使磁盘 online。

不要假定新增盘一定是 `/dev/vdb`；添加/删除磁盘及 seed 都可能影响盘符。Guest 内持久化挂载宜引用文件系统 UUID 等稳定标识。

### 拔除和删除

热拔盘前，先在 Guest 内停止依赖该盘的应用，卸载文件系统，并解除 swap/device mapper 等使用关系，完成写入后再执行：

```bash
sudo chadm volume detach hotdata --instance test01
sudo chadm volume show hotdata
```

程序等待 Guest PCI eject 完成后才解除引用。detach 保留卷及内容；确认不再需要该数据时才单独执行：

```bash
sudo chadm volume delete hotdata
```

冷 detach 同样保留内容。paused、启动/停止转换、pending systemd job 或状态不明时拒绝热插拔。运行中根盘和 cloud-init seed 都不支持拔插，volume detach 没有 `--force`。

### 热插拔超时

API 回复丢失或 Guest 未及时 eject 时，程序保留 `hotplug.toml` 事务，并继续保留修改前后的卷引用。不要因 Guest 中暂时看不到盘就直接删除卷。

同一 VM invocation 仍在运行时，重试原 attach/detach 命令；程序会先核对实际状态。无法完成时：

```bash
sudo chadm instance stop test01
sudo chadm instance recover test01
sudo chadm instance show test01
```

正常关机无效时，可显式选择 `instance stop test01 --force`。recover 证明 VM 已停止并清理 runtime/TAP 后，保留最后发布的 config.toml，再解除 intent。未提交的 attach 被取消；未提交的 detach 仍保留卷引用。核对 show 后，在 stopped 状态重做所需修改。

pending intent 存在时不能 start，不能对其他卷提交新的磁盘修改。不要手工删除 intent 或通过 ch-remote 改变 chadm 管理的磁盘。完整机制见 [热插拔专题](docs/hotplug.md)。

## 实例启停与串口控制台

```bash
sudo chadm instance start test01
sudo chadm instance stop test01
sudo chadm instance reboot test01
```

start 检查配置和存储、恢复可确认的 stale 资源、准备 TAP，再启动 systemd 模板实例。重复启动运行中的实例会报错，不创建第二个 VMM。

stop 通过 API 触发 Guest 电源按钮，等待其正常关机。Guest 需要响应 ACPI 电源事件。超时会保留 VM 运行，不会自动升级为强杀。

reboot 的实现是正常 stop 后 start；这是一次完整的 VMM 生命周期重建，持久化配置、磁盘和 MAC 保留。

需要明确强制结束时：

```bash
sudo chadm instance stop test01 --force
sudo chadm instance reboot test01 --force
```

force 由 systemd 结束该 unit 的进程组/cgroup，Guest 没有正常关机机会。只在接受该后果时使用；它不跳过存储归属检查，也不是删除资源的强制选项。

### Console

```bash
sudo chadm instance console test01
```

连接实例 serial socket，按 `Ctrl+]` 断开；不会关机。一个实例同时只允许一个 chadm console session。连接失败或无登录提示时，分别检查 VM 状态、串口 socket 和 Guest 串口配置；console 不是图形控制台。

### systemd 与日志

```bash
sudo systemctl status chadm-instance@test01.service --no-pager
sudo journalctl -u chadm-instance@test01.service -n 100 --no-pager
sudo journalctl -u chadm-instance@test01.service -f
```

日常启停优先使用 chadm。直接 `systemctl stop` 会使用模板的强制终止语义，不等于 chadm 的 graceful stop。

模板设置 `Restart=no`，当前没有开机自启命令和 `[Install]` 配置。宿主机重启后手动 `instance start`；不要把 `systemctl enable` 当成已支持的 chadm 开机启动接口。

## 快照与落盘挂起

本功能需要安装当前 binary 及匹配的共享 systemd 模板，安装方法见首次安装章节。快照存在时配置及卷引用需要保持不变；不能 attach/detach 或删除实例和被引用资源。

停止 VM 后保存全部可写磁盘：

```bash
sudo chadm instance stop test01
sudo chadm snapshot create --name before-upgrade --instance test01
sudo chadm snapshot list
sudo chadm snapshot show before-upgrade --json
sudo chadm instance start test01
```

需要恢复磁盘时再次停止 VM。restore 会覆盖快照之后的卷写入，不保存或恢复 RAM：

```bash
sudo chadm instance stop test01
sudo chadm snapshot restore before-upgrade
sudo chadm instance start test01
```

明确不再需要回滚点时，`sudo chadm snapshot delete before-upgrade` 保留活动卷并删除备份。ZFS 使用原生快照，回滚要求最新快照且无 bookmark；冲突时不会自动清除新快照或 clone。file 使用 reflink 或 sparse copy，回滚保留原卷身份。跨盘恢复不保证原子完成，失败会保留 restoring 状态并阻止启动，需重试完成。

运行中的 VM 可以落盘挂起：

```bash
sudo chadm instance suspend test01
sudo chadm instance show test01
sudo chadm snapshot list
sudo chadm instance resume test01
```

suspend 先暂停 Guest，保存后端磁盘检查点及 RAM/CPU/设备状态，再结束 VMM 并清理 TAP/runtime，状态为 suspended；resume 恢复磁盘并在新 VMM 中接续执行，成功后清理挂起记录。RAM 默认保存在 `<data_dir>/snapshots/<checkpoint-name>/state/`，需要额外空间，文件应按敏感数据保护。

保留挂起记录时，普通 start 被拒绝，以免冷启动磁盘与保存内存不匹配。需要保持同一 Cloud Hypervisor binary、宿主机 kernel release、启动文件和兼容 CPU 环境；挂起期间不要升级/替换这些组件。内存保存和恢复允许较长等待，普通冷启动仍保留短超时。

如需明确放弃执行状态，先确认 VM 已停止，再用 snapshot list 中的实际挂起名称执行 `chadm snapshot delete NAME --discard`，之后才能冷启动。这保留当前活动卷，但丢失未落盘的 Guest 内存内容。恢复已尝试后不会再次自动回放旧 RAM；失败和取消流程见 [快照与挂起专题](docs/snapshot-suspend.md)。当前只提供 stopped 磁盘快照，没有 Guest agent 文件系统冻结或跨主机迁移接口。

## 查看配置与脚本化

### List、show 和 inspect

```bash
sudo chadm instance list
sudo chadm instance show test01
sudo chadm inspect test01
```

list 适合概览，show 展示实例计算资源、启动方式、磁盘、MAC/IP 和 runtime 路径，inspect 展示解析后的配置及 Cloud Hypervisor argv。inspect 是只读诊断，不启动 VM，也不会修改资源。

实例首次登记给 systemd 之前，状态来源为 metadata-only；首次 start 后依据 systemd/API 查询运行状态。API、systemd 或所有权验证失败时不能把 unknown 当作 stopped 后继续删除资源。

### JSON 输出

以下命令支持 `--json`：

```bash
sudo chadm repo list --json
sudo chadm repo show local --json
sudo chadm image list --json
sudo chadm image show ubuntu-test --json
sudo chadm volume list --json
sudo chadm volume show test01-root --json
sudo chadm instance list --json
sudo chadm instance show test01 --json
sudo chadm inspect test01 --json
sudo chadm doctor --json
```

list 返回数组，show 返回单个资源对象。常用读取方式：

```bash
sudo chadm instance list --json | jq '.[] | {name, state, available, windows}'
sudo chadm instance show test01 --json | jq '.disk'
sudo chadm instance show test01 --json | jq '.runtime'
sudo chadm volume show test01-root --json | jq '{name, repo, path, available, reason}'
sudo chadm image show ubuntu-test --json | jq '{name, windows, available, reason}'
```

list/show 成功返回 JSON 不代表底层资源全部可用。脚本还要检查 `available`、`reason` 和实例 `state`。ZFS GUID 为十进制字符串，避免脚本丢失 64 位精度。

### 退出码和 pipeline

| 退出码 | 含义 |
| --- | --- |
| 0 | 命令成功；doctor 没有 FAIL，允许有 WARN |
| 1 | 配置、存储、权限、运行状态等操作错误；或 doctor 检出 FAIL |
| 2 | CLI 参数/用法错误 |

在 Bash pipeline 中保留 chadm 的失败状态：

```bash
set -o pipefail
sudo chadm doctor --json | jq .
```

### 配置和运行时目录

```text
/etc/chadm/config.toml
/var/lib/chadm/
  CLOUDHV.fd
  instances/test01/config.toml
  instances/test01/systemd.toml       首次登记到 systemd 后存在
  instances/test01/hotplug.toml       仅未完成热插拔时保留
  repos/local/config.toml
  images/ubuntu-test/config.toml
  volumes/test01-root/config.toml
/run/chadm/test01/
  owner.toml
  api.sock
  console.sock
```

元数据目录和存储 payload 的位置不同，不能只复制 `/var/lib/chadm/instances` 就当作完整 VM 备份。不要手工删除 generation ID、归属 marker、xattr 或事务文件；这些都是引用与删除保护的依据。

需要编辑实例 TOML 时先停止 VM，保留完整结构和归属字段；涉及卷引用的修改使用 attach/detach。运行状态不写成持久化 `running=true`，也不使用 PID 文件作为进程归属依据。

## 删除资源与引用关系

先确认实例和数据都不再需要。以快速启动示例为例：

```bash
sudo chadm instance stop test01
sudo chadm instance show test01
sudo chadm instance delete test01
sudo chadm volume list
```

`instance delete` 删除实例元数据，保留 root/data volume 和 seed；不会删除共享模板。

确认对应卷不再需要后，显式删除：

```bash
sudo chadm volume delete test01-data
sudo chadm volume delete test01-root
```

如果另外 attach 过 `extra`/`zdata`，它们也仍保留，由用户分别决定去向。卷是否可删除由所有实例引用、generation 和实际存储归属共同决定。

所有实例和克隆卷均不再引用基础 image 后，才可删除它：

```bash
sudo chadm image delete ubuntu-test
```

即使实例已删除，保留的 root volume 仍记录 source image 引用。因此通常需要先处理该 root volume，才能删除基础 image。default/custom seed 也必须解除实例引用后再删除。

repo 空且无其他引用时才可删除：

```bash
sudo chadm repo delete local
```

这些删除命令不会覆盖安全检查；当前没有级联删除或 `delete --force`。有其他实例仍引用示例资源时，拒绝删除是预期结果。

## 故障排查与恢复

### 标准排查顺序

```bash
sudo chadm doctor
sudo chadm instance show test01
sudo chadm inspect test01
sudo systemctl status chadm-instance@test01.service --no-pager
sudo journalctl -u chadm-instance@test01.service -n 100 --no-pager
```

inspect 会隐藏 kernel cmdline 和 seed 内容，但仍包含宿主机路径、MAC、名称等运行信息，分享日志前按实际需要处理这些信息。

| 现象 | 检查和处理 |
| --- | --- |
| `bridge ... does not exist` | 检查实例配置引用的 bridge，先在宿主机准备好它 |
| `/dev/kvm` 或 `/dev/net/tun` 不可用 | 检查设备、内核能力和执行权限，用宿主机 root 环境重跑 doctor |
| 模板缺失/不匹配 | 安装与 binary 对应的共享模板，daemon-reload；检查是否有自定义 unit/drop-in |
| CLI 配置与 service 配置不同 | 核对 `/etc/chadm/config.toml` 的 data/runtime/Cloud Hypervisor 路径 |
| 固件缺失或权限不安全 | 安装固定的 root-owned 普通 `CLOUDHV.fd`，检查父目录和写权限 |
| VM running 但 IP unknown | 检查 Guest DHCP、外部租约及 Guest 自身地址，不能只根据 IP 观测判定 VM 未启动 |
| console 连接成功但没有登录提示 | 检查 Guest 串口输出和登录服务；Windows 检查 SAC/EMS |
| 正常 stop 超时 | 查看 Guest 是否处理电源按钮；必要时显式使用 `--force` |
| 卷已被引用 | show/list 查明使用者，先 detach 或删除实例，再决定是否删除卷 |
| cloudinit default 缺失 | 导入有效 `default` raw seed，或创建实例时明确 `--no-cloud-init` |
| 热插拔 pending/超时 | 重试原命令；无法确认时 stop 后 recover，再核对已发布配置 |
| lock 冲突 | 等待持锁命令结束并检查其状态，不删除 `.lock` 绕过锁 |
| ZFS target dataset 已存在 | 检查实际存储状态；当前不接管已有 dataset，不直接递归删除重试 |
| ZFS 工具不可信 | 核对包管理器安装路径、root 归属和目录/目标权限；合法受保护的 symlink 链可用 |
| GUID `out-of-range value for u64 type` | 更新程序；当前 repo/volume GUID 已使用兼容完整 u64 的字符串格式 |

### Crash、stale TAP 和 runtime

确认实例应已停止后执行：

```bash
sudo chadm instance recover test01
sudo chadm instance show test01
sudo chadm instance start test01
```

recover 不会为了清理资源而关机或杀死 VM。它先证明 systemd unit、helper、job 和 cgroup 都已静止，再校验归属并清理本实例的 runtime/TAP。运行中或无法确认时会拒绝操作。未知文件、同名 foreign TAP、错误 owner/generation 都会保留并报错。

VMM crash 后，模板会尝试退出清理，但 `Restart=no` 不会自动重启。system reboot 后 `/run` 状态丢失，持久化配置、MAC 和存储仍保留；恢复并核对后手动 start。

### 未完成存储事务

`.create-*`、`.delete-*`、`.config-*`、`rollback incomplete` 或损坏的 intent 表示需要检查中断证据。`instance recover` 处理可验证的 runtime/TAP，以及已停止实例的有效 hotplug intent，不是通用存储修复器。

保留完整错误、元数据和原生存储状态。可先做只读检查：

```bash
sudo ls -la /var/lib/chadm/repos /var/lib/chadm/volumes /var/lib/chadm/images /var/lib/chadm/instances
sudo zfs list -r -t all tank/chadm
sudo zpool status tank
```

按实际存在的目录/dataset 调整命令。不要用 `rm -rf` 或 `zfs destroy -r` 清空事务后重试，也不要移除归属标记让程序“重新认领”资源。

## 更新程序与备份

### 更新程序

涉及 binary、模板、配置或 firmware 的整套更新，先安排停止相关 VM，并保留原版本和存储备份。然后在源码目录执行：

```bash
cargo build --locked --release
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm
sudo install -m 0644 packaging/systemd/chadm-instance@.service /etc/systemd/system/chadm-instance@.service
sudo systemd-analyze verify /etc/systemd/system/chadm-instance@.service
sudo systemctl daemon-reload
sudo chadm doctor
```

template 和 binary 必须匹配，已移除的 CLI 参数不要继续用于新版本。检查 `sudo` 下使用的确实是新程序，可直接执行 `/usr/local/bin/chadm version`，必要时在后续命令中使用完整路径。

### 备份范围

需要同时保存宿主机配置、受管元数据、firmware、image、volume payload、存储 sidecar 和相关归属属性。file repo 的复制方式应保留权限、xattr 和完整数据；ZFS 备份需覆盖相应数据与属性。备份应在实例停止后进行，或由用户建立一致的存储/Guest 备份流程。

chadm 的本地 snapshot/restore 和落盘挂起不是异地备份或迁移工具。当前没有备份导出、导入、接管外部卷或独立 clone CLI，不能把单独拷贝 raw 文件到目录视为已经完成登记。备份还应覆盖快照清单、其后端 payload 和保存的 RAM 状态，并单独验证身份一致性。宿主机升级前先恢复挂起 VM 并正常关机，或明确 discard 后再处理。

## 命令速查与功能边界

统一形式是 `chadm <resource> <action>`；doctor、inspect、version 为辅助命令。

| 命令 | 用途 |
| --- | --- |
| `chadm version` | 程序版本 |
| `chadm doctor [--json]` | 只读宿主机/资源诊断 |
| `chadm inspect NAME [--json]` | 解析后的配置、磁盘路径、启动参数和 runtime |
| `chadm repo create --name NAME --driver file --path PATH` | 创建 file repo |
| `chadm repo create --name NAME --driver zfs --pool DATASET` | 在已有父 filesystem 下创建 ZFS repo |
| `chadm repo delete NAME` | 删除空受管 repo |
| `chadm repo list [--json]` | 列出 repo |
| `chadm repo show NAME [--json]` | 查看 repo |
| `chadm image create --name NAME --repo REPO --path PATH [--windows]` | 导入 raw/qcow2，或按专用规则导入 seed |
| `chadm image delete NAME` | 删除无引用的 image |
| `chadm image list [--json]` | 列出 image |
| `chadm image show NAME [--json]` | 查看 image |
| `chadm volume create --name NAME --repo REPO --size SIZE` | 创建空 raw/zvol |
| `chadm volume attach NAME --instance INSTANCE` | 冷挂载或运行中热插入 |
| `chadm volume detach NAME --instance INSTANCE` | 冷卸载或等待热拔除，保留卷 |
| `chadm volume delete NAME` | 删除无引用的受管卷 |
| `chadm volume list [--json]` | 列出卷 |
| `chadm volume show NAME [--json]` | 查看卷 |
| `chadm instance create --name NAME --image IMAGE ...` | 准备实例配置和独立磁盘 |
| `chadm instance start NAME` | 启动 |
| `chadm instance stop NAME [--force]` | 正常/强制停止 |
| `chadm instance reboot NAME [--force]` | stop 后 start |
| `chadm instance suspend NAME` | 保存 RAM/CPU/设备和磁盘检查点，退出 VMM |
| `chadm instance resume NAME` | 从落盘挂起恢复执行 |
| `chadm snapshot create --name NAME --instance INSTANCE` | 停止状态保存全部可写磁盘 |
| `chadm snapshot restore NAME` | 停止状态恢复全部磁盘，保留实例配置 |
| `chadm snapshot delete NAME [--discard]` | 删除快照；丢弃挂起执行状态需显式 discard |
| `chadm snapshot list [--json]` | 查看快照和挂起记录 |
| `chadm snapshot show NAME [--json]` | 查看记录、后端及可用性 |
| `chadm instance console NAME` | 串口连接，Ctrl+] 退出 |
| `chadm instance recover NAME` | 验证已停止状态后恢复受管遗留资源 |
| `chadm instance delete NAME` | 删除已停止实例元数据，保留卷 |
| `chadm instance list [--json]` | 实例列表 |
| `chadm instance show NAME [--json]` | 实例详情和可观测 IP |

查看任意动作的精确参数：

```bash
chadm --help
chadm instance create --help
chadm volume attach --help
chadm image create --help
chadm repo create --help
```

当前支持 file/ZFS、firmware/direct boot、TAP/Linux bridge、raw/qcow2 导入、只读 raw seed、Windows profile、磁盘冷热挂载、停止状态磁盘快照、落盘挂起/恢复、systemd 生命周期、console、inspect 和 JSON。

当前未实现 LVM、registry、独立 clone/resize CLI、运行中磁盘快照/Guest 文件系统冻结、CPU/内存/网卡热插拔、SR-IOV、OVS、vhost-user、vDPA、static IP 配置、seed 生成/替换、镜像 URL 下载和开机自启管理。已有 LVM VG 不会被当前版本使用。

专题文档：[架构](docs/architecture.md)、[固定 firmware 与冷挂载](docs/cold-attach.md)、[cloud-init](docs/cloud-init.md)、[Windows](docs/windows.md)、[磁盘热插拔](docs/hotplug.md)、[ZFS](docs/phase9.md)、[恢复机制](docs/phase10.md)。历史阶段文档中的旧限制请结合当前专题和本手册阅读。

## NixOS：为新实例设置默认网桥

```nix
virtualisation.chadm.defaultBridge = "virbr0";
```

部署后创建实例可省略 `--net`，自动创建一块 DHCP 网卡，或使用 `--net dhcp`。
例如（镜像 `ubuntu-test` 已导入）：

```bash
sudo chadm instance create --name test01 --image ubuntu-test \
  --boot firmware --cpus 2 --ram 2G --volume root,size=20G
```

默认网桥必须已存在、为 Linux bridge 且处于 UP 状态，否则创建命令在分配磁盘前报错。
该选项不创建网桥、不提供 DHCP。显式 `--net bridge=br1` 覆盖默认值。
已有实例仍使用其自身配置中的网桥，不随默认值变化。
未配置默认网桥时，省略 `--net` 仍表示无网卡。
非 NixOS 可在 `/etc/chadm/config.toml` 设置 `default_bridge = "virbr0"`。
更多部署及测试说明见 [NixOS 文档](docs/nixos.md)。
