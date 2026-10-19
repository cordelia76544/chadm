# chadm

源码仓库：[cordelia76544/chadm](https://github.com/cordelia76544/chadm)。NixOS 26.05 可直接通过 GitHub flake 部署，见下方快速入口和 [完整 NixOS 部署方案](docs/nixos.md)。多网卡及已有实例冷添加网卡见 [使用手册](HOW-TO-USE.MD#网络与-ip-地址)。

专门管理 Cloud Hypervisor 的轻量 Linux CLI。使用本地 TOML 元数据和 systemd，不依赖 libvirt、数据库或常驻 chadm daemon。

完整操作说明见 [HOW-TO-USE.MD](HOW-TO-USE.MD)，包含首次安装、file/ZFS、镜像、cloud-init、Windows、实例启停、磁盘热插拔、JSON 和故障恢复。

已补充[磁盘快照与落盘挂起](docs/snapshot-suspend.md)：`snapshot create/restore/delete/list/show` 管理停止实例的全部磁盘；`instance suspend/resume` 保存 RAM/CPU/设备状态并退出/恢复 VMM。ZFS 使用原生快照/rollback，file 优先 reflink、回退复制。本次需要同步更新共享 systemd 模板以支持较长 RAM 恢复窗口。

当前实现到 **Phase 10**，已补充[冷挂载、固定 firmware 与默认 DHCP](docs/cold-attach.md)：file/ZFS repo、file raw sparse volume 和 zvol、本地 raw/QCOW2 image，以及 instance 的 create/delete/list/show。实例创建准备独立 root/data 卷和持久化 MAC，不自动启动 VM；Phase 7 已提供共享 systemd 模板及 start/stop/reboot。架构见 [docs/architecture.md](docs/architecture.md)，分阶段实现与验收见 [Phase 2](docs/phase2.md)、[Phase 3](docs/phase3.md)、[Phase 4](docs/phase4.md)、[Phase 5](docs/phase5.md)、[Phase 6](docs/phase6.md)、[Phase 7](docs/phase7.md)、[Phase 8](docs/phase8.md)、[Phase 9](docs/phase9.md) 和 [Phase 10](docs/phase10.md)。

实例管理使用共享 `chadm-instance@.service` 模板，例如 `chadm-instance@test01.service`。`instance show` 对运行实例按网卡显示观测到的 IPv4/IPv6 地址；未发现为 unknown，查询失败为 unavailable，停止实例为 N/A。具体边界和阶段安排见[补充设计](docs/architecture.md#systemd-模板与-instance-show-ip补充需求)。安装一次共享模板和匹配的 chadm binary，服务统一读取 /etc/chadm/config.toml；不创建每实例 unit 文件。

## Nix / NixOS 快速开始

在启用了 `nix-command` 和 `flakes` 的 Linux 上：

```bash
nix build github:cordelia76544/chadm#chadm -L
./result/bin/chadm version
# 仅查看帮助：
nix run github:cordelia76544/chadm -- --help
```

支持 `x86_64-linux` 和 `aarch64-linux`。构建测试要求临时文件系统支持 `user.*` 扩展属性；本次验证的环境限制见 [构建验证记录](docs/nixos.md#github-入口验证2026-10-19)。构建命令只提供软件；NixOS 宿主机应在现有 flake 中加入：

```nix
inputs.chadm.url = "github:cordelia76544/chadm";
inputs.chadm.inputs.nixpkgs.follows = "nixpkgs";
```

将 `chadm` 加入 `outputs` 参数，并将 `chadm.nixosModules.default` 加入目标主机的 `modules`，设置 `virtualisation.chadm.enable = true;`。完整配置、repo、固件、网桥、锁文件及升级步骤见 [docs/nixos.md](docs/nixos.md)。Nix 管理宿主机路径、存储、固件和共享 systemd 模板；实例仍由 CLI/TOML 管理。chadm 不创建网桥，只通过 `--net bridge=<已有网桥名>` 将实例接入宿主机已有的 Linux bridge。

## 构建与验证

以下是本地 Cargo 构建方式；使用上面的 Nix 构建时无需单独安装 Rust。需要 Linux 和带 Cargo 的 Rust 工具链；已在 Rust 1.85.1 上验证，尚未测试更低版本。保留 `Cargo.lock` 以固定 CLI 的依赖版本。

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release

cargo run -- --help
cargo run -- version
cargo run -- doctor
```

只读命令不要求 root，但受目录和设备权限限制。doctor 检查的是当前用户权限；普通用户无法打开 KVM 时会返回 FAIL。它不创建配置目录、不启动 VM、不修改网络或存储。默认 `/var/lib/chadm` 下的 repo 写入通常需要 root；测试使用普通用户拥有的隔离目录。

## 启动普通文件磁盘的 VM

Phase 7 的安装、firmware 配置、启停、关机超时及宿主机验收见 [docs/phase7.md](docs/phase7.md)；崩溃清理、`instance recover` 和 `doctor --json` 见 [Phase 10](docs/phase10.md)。通过 NixOS module 或手工方式安装好共享模板后（NixOS 使用 `sudo chadm`，手工安装示例如下）：

```bash
sudo /usr/local/bin/chadm instance start test01
sudo /usr/local/bin/chadm instance show test01
sudo /usr/local/bin/chadm instance reboot test01
sudo /usr/local/bin/chadm instance stop test01
# Guest 不响应电源按钮时，显式强制停止：
sudo /usr/local/bin/chadm instance stop test01 --force
```

VM 使用独立可写 raw volume，基础 image 保持只读。已补充 [cloudinit file repo 和 raw seed 只读挂载](docs/cloud-init.md)，支持默认 default.raw、显式选择及跳过。已提供 `instance console`（Ctrl+] 退出）和 `inspect --json`，详见 [Phase 8](docs/phase8.md)。尚不生成 seed，登录条件由镜像、串口配置和 seed 内容决定。

## 配置目录

默认读取 `/etc/chadm/config.toml`；此默认文件不存在时使用内置默认值。`--config PATH` 指定的文件必须存在，未知字段和非法路径报错。help/version 无需读取配置。

```toml
data_dir = "/var/lib/chadm"
runtime_dir = "/run/chadm"
cloud_hypervisor = "cloud-hypervisor"
# 按实际安装位置设置，不能照抄一个不存在的文件：
# cloud_hypervisor = "/opt/cloud-hypervisor/cloud-hypervisor"
```

metadata 子目录为 `instances/`、`volumes/`、`images/`、`repos/`；写命令按需创建对应元数据目录，raw 文件放在 repo 指定位置。显式 binary 路径必须是绝对路径；默认 binary 在固定系统目录查找，不使用当前目录或用户 PATH。

宿主机 KVM 已由用户确认可用；执行沙箱可能屏蔽设备或 systemd 访问，doctor 诊断只代表其运行上下文。firmware boot 固定使用 `<data_dir>/CLOUDHV.fd`（默认 `/var/lib/chadm/CLOUDHV.fd`），不再接受 chadm `--firmware`。安装和替换方法见 [冷挂载与固定 firmware](docs/cold-attach.md)。

## File volumes

先创建 file repo，再创建独立空卷：

```bash
sudo ./target/release/chadm volume create --name data --repo local --size 30G
sudo ./target/release/chadm volume list
sudo ./target/release/chadm volume show data --json
sudo ./target/release/chadm volume delete data
```

`30G` 表示 30 GiB，raw 使用 sparse file，不预留全部容量。独立 volume create 和实例 data 卷为空块设备内容，没有分区或文件系统；实例 root 卷复制 image 内容。file backend 要求文件系统支持 `user.*` 扩展属性，用于验证文件归属；不支持时创建失败并回滚。instance create 已支持 root 镜像克隆；`volume attach/detach` 在停止时修改配置，在运行时执行磁盘热插拔，适用于 file 和 ZFS 管理卷；resize、CPU/内存/网卡热插拔和独立 clone 命令尚未实现。用法及拔盘、超时恢复说明见 [磁盘热插拔](docs/hotplug.md)。

## 本地镜像导入

已安装好的 Windows 模板可在 `image create` 时添加 `--windows`。实例继承 Windows profile，自动启用 `kvm_hyperv=on,max_phys_bits=39`，要求 firmware boot。raw/qcow2 和 file/ZFS 根卷均沿用原存储流程；标记不会安装 Guest 驱动。使用方法见 [Windows 镜像](docs/windows.md)。

```bash
sudo ./target/release/chadm image create --name debian13 --path ./debian13.raw --repo local
sudo ./target/release/chadm image list
sudo ./target/release/chadm image show debian13 --json | jq .
```

QCOW2 使用 qemu-img 转换；raw 导入不依赖它。仅接受离线普通文件，拒绝 backing chain、外部 data file 和加密 QCOW2。输入扩展名不决定格式；导入不修改源文件，基础镜像保存为 0400 的独立 raw。暂存复制可能需要额外空间；实例 root volume 已通过 Phase 5 克隆；VM 启动见 [Phase 7](docs/phase7.md)。详细限制、测试和删除验收见 [Phase 4](docs/phase4.md)。

## 实例配置与磁盘准备

已有 debian13 image 时：

```bash
sudo ./target/release/chadm instance create \
  --name test01 --image debian13 --boot firmware --cpus 2 --ram 2G \
  --volume root,size=20G --volume data,size=30G \
  --net bridge=virbr0
sudo ./target/release/chadm instance show test01 --json | jq .
```

目标 repo 默认与 image 相同，也可显式 `--repo local`。root 复制镜像，data 为空 sparse raw。instance create 仍只准备元数据和存储，不创建 TAP/unit/runtime 目录；网络模块在准备 TAP 时验证 bridge，doctor 检查实例引用的 bridge。未注册 systemd 的实例标注 metadata only；首次启动登记后，从 systemd/API 派生状态。运行实例查询 IP；runtime 痕迹或查询失败会阻止危险操作。`instance delete` 保留卷；引用中的卷/镜像不能直接删除。direct boot、错误与回滚边界见 [Phase 5](docs/phase5.md)。

已支持在现有 zpool `tank` 下创建独立 ZFS repo 和 zvol，详见 [Phase 9](docs/phase9.md)。LVM VG `ch` 保留供后续使用，当前不支持 LVM。

## doctor 与退出码

doctor 检查 `/dev/kvm`、Cloud Hypervisor 版本、systemd system manager、iproute2、qemu-img、固定 firmware 文件的可读性，以及 file/ZFS repo/volume/image 和实例配置及依赖存储的归属与可用性。它不验证 firmware 架构或实际 VM 启动能力。还检查已配置 Linux bridge、TUN/TAP 节点和共享模板。无对应资源配置时显示 N/A；配置 ZFS repo 后检查 zfs 工具、内核可用性、dataset 归属和设备。

- `0`：命令成功，doctor 无 FAIL；WARN 不导致失败。
- `1`：配置/IO 错误，或 doctor 存在 FAIL。
- `2`：CLI 用法错误。

缺少固定 firmware 文件、缺少 qemu-img 是 WARN，分别影响未来 firmware boot 和 qcow2 导入。缺少核心运行依赖是 FAIL。repo/volume/image/instance list/show 支持 `--json`，包含 `available` 和 `reason` 字段；查询到不可用资源时命令仍成功，脚本检查 `available`。非法或损坏的元数据返回 1。

## 手工验收

```bash
# 即使此配置不存在，version 仍成功：
cargo run -- --config /nonexistent/chadm.toml version
# 显式缺失配置报错，exit 1：
cargo run -- --config /nonexistent/chadm.toml doctor
# 未实现命令报用法错误，exit 2：
cargo run -- registry list
```

在 `/tmp` 创建含上述默认目录的临时 TOML，用 `--config` 运行 doctor；检查这些目录没有被创建。已有宿主机环境缺少依赖时应显示 FAIL/WARN，而非 panic。测试自动覆盖隔离配置目录无副作用。

## Phase 1 验证记录

在 Cargo/rustc 1.85.1 上完成以下检查，依赖已下载，本次编译使用离线模式：

- `cargo fmt --check`：通过。
- `cargo test --offline`：5 项单元测试、4 项集成测试全部通过。
- `cargo clippy --offline --locked --all-targets -- -D warnings`：通过。
- `cargo build --offline --locked --release`：通过，`target/release/chadm version` 输出 `chadm 0.1.0`。
- 手工验证 `--help`、`version`、`--version` 返回 0，缺失显式配置返回 1，未实现命令返回 2。

测试覆盖配置校验、TOML 解析、超大配置文件与 symlink 拒绝、进程超时、CLI 错误和 doctor 不创建资源目录。

Phase 1 验收时，执行环境缺少 `/dev/kvm` 和 Cloud Hypervisor，无法联系 systemd system manager；实际运行 doctor 正确显示 FAIL 并返回 1，iproute2/qemu-img 显示 OK，未配置 firmware 显示 WARN。Phase 2 已检测到安装后的 Cloud Hypervisor v52.0.0；这不代表当前沙箱已具备运行 VM 的条件。

后续验证记录见 [Phase 2](docs/phase2.md)、[Phase 3](docs/phase3.md)、[Phase 4](docs/phase4.md)、[Phase 5](docs/phase5.md)、[Phase 6](docs/phase6.md)、[Phase 7](docs/phase7.md)、[Phase 8](docs/phase8.md)、[Phase 9](docs/phase9.md) 和 [Phase 10](docs/phase10.md)。当前实现停在 Phase 10。稳定性、受保护的 stale runtime/TAP 恢复和宿主机 crash 验收见 Phase 10 文档。
