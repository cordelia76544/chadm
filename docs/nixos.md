# NixOS 部署与宿主机 options

Nix 管理 chadm 软件、宿主机路径、存储 repo 和固件。Linux bridge 由宿主机独立配置；chadm 只将实例 TAP 接入已有 bridge，不创建或修改 bridge。虚拟机仍通过 CLI 创建，其 CPU、内存、磁盘、网卡、MAC、cloud-init 等配置保存在本地 TOML；module 不生成、导入或覆盖任何实例配置。

## 从 GitHub 构建

源码仓库：[cordelia76544/chadm](https://github.com/cordelia76544/chadm)。需要 Linux，并启用 Nix 的 `nix-command` 和 `flakes` 功能；NixOS 可设置 `nix.settings.experimental-features = [ "nix-command" "flakes" ];`。

无需先克隆仓库即可构建或查看 CLI：

```bash
nix build github:cordelia76544/chadm#chadm -L
./result/bin/chadm version
nix run github:cordelia76544/chadm -- --help
```

构建会运行 Rust 测试，构建环境中的临时文件系统必须支持 `user.*` 扩展属性（xattr）；运行时 file repo 的文件系统也有同样要求。若测试报 `cannot mark raw payload ownership: Operation not supported`，应检查实际 Nix builder 的构建目录文件系统，而不只是源码所在目录；远程 builder 也需满足此条件。

只验证构建、不创建 `result` 链接时，追加 `--no-link`。这些命令只获取软件，不安装 NixOS 服务、固件或配置网络；宿主机部署使用下面的 module。

仓库的 `flake.nix` 指向 `github:NixOS/nixpkgs/nixos-26.05`，`flake.lock` 固定依赖版本。包支持 `x86_64-linux` 和 `aarch64-linux`。直接构建使用仓库锁文件；宿主机配置采用下文的 `follows` 后，module 和软件依赖使用宿主机锁定的 nixpkgs。

## NixOS flake 部署

在宿主机 `/etc/nixos/flake.nix` 中引入 GitHub input，并将 module 加到现有主机的 `modules`。下面的 `myhost` 替换为自己的配置名称；已有 flake 时合并这些条目，保留现有磁盘、启动和硬件配置：

```nix
{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    chadm.url = "github:cordelia76544/chadm";
    chadm.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { nixpkgs, chadm, ... }: {
    nixosConfigurations.myhost = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux"; # ARM 使用 aarch64-linux
      modules = [
        ./configuration.nix
        chadm.nixosModules.default
        ./chadm.nix
      ];
    };
  };
}
```

新建 `/etc/nixos/chadm.nix`：

```nix
{ ... }: {
  virtualisation.chadm = {
    enable = true;
    dataDir = "/var/lib/chadm";
    runtimeDir = "/run/chadm";

    repos = {
      local = { driver = "file"; path = "/srv/chadm/storage"; };
      cloudinit = { driver = "file"; path = "/srv/chadm/cloudinit"; };
      # 已配置并导入 tank 后再启用；详细条件见后文。
      # fast = { driver = "zfs"; pool = "tank/chadm"; };
    };

    # firmware boot 前启用并提供真实固件；GitHub 固定下载配置见后文。
    # firmware = {
    #   enable = true;
    #   source = ./firmware/CLOUDHV.fd;
    # };
  };

  # 按实际 CPU 选择；AMD 使用 kvm-amd，ARM 按硬件配置 KVM。
  boot.kernelModules = [ "kvm-intel" ];
  # 使用 ZFS 时再启用，并沿用宿主机现有 hostId/pool 导入配置。
  # boot.supportedFilesystems = [ "zfs" ];
}
```

此最小配置准备 CLI、共享模板和 file repo。启动 Guest 前，还需准备镜像、固件或 direct boot 文件，以及宿主机已有的 bridge、网络和 cloud-init seed，见 [使用手册](../HOW-TO-USE.MD)。

在宿主机生成或更新锁文件，然后先构建、再切换：

```bash
cd /etc/nixos
# 如果 /etc/nixos 使用 Git，先将新配置及实际使用的本地固件加入索引。
# git add flake.nix configuration.nix chadm.nix firmware/CLOUDHV.fd
sudo nix flake lock
sudo nixos-rebuild build --flake .#myhost
sudo nixos-rebuild switch --flake .#myhost
sudo systemctl status chadm-host-setup.service
sudo chadm doctor
sudo chadm repo list
```

将宿主机的 `flake.lock` 一并纳入版本控制，它固定 chadm 和 nixpkgs 的具体 revision。普通 rebuild 沿用锁定版本；更新 chadm 时，在 `/etc/nixos` 执行 `sudo nix flake update chadm`，检查锁文件差异后重新 build/switch。切换前的 VM 停机要求见后文“更新、故障和测试”。也可将 input 写成 `github:cordelia76544/chadm/<完整提交 SHA>` 固定源码提交。

## 本地开发和非 flake 宿主机

需要修改源码时先克隆：

```bash
git clone https://github.com/cordelia76544/chadm.git
cd chadm
nix build .#chadm -L
```

在 Git 工作树中使用 flake 时，新的 Nix 文件必须已加入 Git 索引；否则 Nix 可能报告文件不存在。不要把本地 Guest 镜像或运行数据放进源码版本控制。

非 flake 宿主机可将仓库检出到 `/etc/nixos/chadm`，在 `configuration.nix` 中加入 `imports = [ ./chadm/nix/module.nix ];`，并设置上述 `virtualisation.chadm` options，随后运行 `sudo nixos-rebuild switch`。请将检出版本固定到审核过的提交。此方式使用宿主机的 pkgs，不读取仓库 `flake.lock`；宿主机 nixpkgs 应使用 26.05。`default.nix` 也可供传统 `nix-build` 使用，采用调用方的 `<nixpkgs>`。

`chadm-host-setup` 是 Type=oneshot 服务，完成后保留 active (exited) 状态，不运行常驻进程。它在本地文件系统、tmpfiles 和可选的 ZFS import target 之后执行，并等待声明的文件 repo 路径所需挂载。共享 `chadm-instance@.service` 仍来自同一构建；不会生成每个实例的 unit/drop-in，也不会自动启动 VM。

## Options

所有选项位于 `virtualisation.chadm`：

| Option | 默认值 | 含义 |
| --- | --- | --- |
| `enable` | `false` | 安装 CLI、共享模板和宿主机初始化服务 |
| `package` | 本仓库构建 | 自定义 chadm 包，必须附带匹配模板和工具路径 |
| `cloudHypervisorPackage` | `pkgs.cloud-hypervisor` | 编译绑定的 VMM 包；自定义 package 时由该包自行处理 |
| `dataDir` | `/var/lib/chadm` | 可写元数据和固定固件目录 |
| `runtimeDir` | `/run/chadm` | 必须在 `/run/` 下，重启由 tmpfiles 创建 |
| `manageHostConfig` | `true` | 维护普通的 `/etc/chadm/config.toml`，不管理实例 TOML |
| `enableZfs` | 声明 ZFS repo 时自动为 true | 包含与 `boot.zfs.package` 配套的用户态工具 |
| `repos.<name>.driver` | `file` | `file` 或 `zfs` |
| `repos.<name>.path` | `null` | file repo 的绝对目录 |
| `repos.<name>.pool` | `null` | ZFS repo 的目标 dataset，例如 `tank/chadm` |
| `firmware.enable` | `false` | 安装固件为真实的 `<dataDir>/CLOUDHV.fd` 文件 |
| `firmware.source` | `null` | 本地文件或 fetchurl derivation，优先于 GitHub 设置 |
| `firmware.github.owner` | `cloud-hypervisor` | GitHub owner |
| `firmware.github.repo` | `edk2` | 发布固件的仓库 |
| `firmware.github.rev` | 空 | 固定 Release tag，禁止 `latest` |
| `firmware.github.hash` | 空 | Release 资产的 SHA-256 SRI hash |
| `firmware.github.asset` | 按架构选择 | x86_64 为 `CLOUDHV.fd`；ARM 为 `CLOUDHV_EFI.fd` |

路径必须规范、绝对、不含 `..`，数据与运行目录不能重叠。module 当前接受的宿主机路径字符为字母、数字、下划线、短横线、点和 `/`。repo 名称遵循 chadm 的严格校验。

`manageHostConfig = true` 时，宿主机配置按 options 更新为普通的 root 所有文件，不使用 `/etc` 到 Nix store 的 symlink。若希望保留手工配置，设置 false；此时只在配置缺失时初始化，并要求现有 data_dir/runtime_dir 与 options 一致。不要额外设置 `environment.etc."chadm/config.toml"`，它会与普通文件检查冲突。

## GitHub 自动获取固件

UEFI 固件来自 Cloud Hypervisor 的 edk2 Release。先在 [官方 Release 页面](https://github.com/cloud-hypervisor/edk2/releases) 选定一个明确的 tag；不要使用 latest。取得这个资产的 hash，例如：

```bash
# 替换为选定的真实 Release tag。
CHADM_FW_TAG='<release-tag>'
nix store prefetch-file --json \
  "https://github.com/cloud-hypervisor/edk2/releases/download/${CHADM_FW_TAG}/CLOUDHV.fd"
```

将输出的 `hash` 填入配置：

```nix
virtualisation.chadm.firmware = {
  enable = true;
  github = {
    rev = "<release-tag>";
    hash = "sha256-<prefetch 输出的 base64>";
    # ARM 默认自动选择 CLOUDHV_EFI.fd；预取 hash 时也要使用相应资产。
  };
};
```

这里使用 `pkgs.fetchurl`：`CLOUDHV.fd` 是 Release 中的二进制文件；`fetchFromGitHub` 获取源码归档，不能直接当作固件。也可以给 `firmware.source` 设置自行编译生成的固件文件，或自定义 fetchurl 的产物。官方文件名与架构说明见 [Cloud Hypervisor UEFI 文档](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/docs/uefi.md)。

下载在 Nix 构建阶段完成，要求 tag 与内容 hash 都匹配；启动时不联网。部署时复制为普通文件，即使 ARM 下载的是 CLOUDHV_EFI.fd，本地文件名也固定为 CLOUDHV.fd。内容相同时不替换文件，保留 inode/mtime，避免普通 rebuild 破坏已保存内存的启动文件身份。

固件内容变化前，初始化服务会检查实例状态、操作锁和 systemd 进程/任务。必须先恢复落盘挂起的实例并正常停止所有 VM；running、suspended、状态不明或正在执行管理操作时会拒绝更新。不要用禁用 `firmware.enable` 的方式删除旧固件；禁用声明会保留已部署文件。

## Repo、ZFS 和网桥的边界

repo 初始化通过原有 `chadm repo create` 执行，沿用归属标记、路径检查和资源锁。存在时检查 driver、path/pool 和可用性；重跑不会重建 repo 或清空其内容。移除声明不删除 repo、volume、image 或 dataset。声明与已有资源不同会失败，不会执行迁移、接管、rollback 或 destroy。修正声明后可运行：

```bash
sudo systemctl restart chadm-host-setup.service
```

`pool = "tank/chadm"` 指的是 repo 的新 dataset，不是创建名为 tank 的 zpool。pool 必须已导入；父 dataset 必须存在，不要预先创建 chadm 将管理的目标 dataset。已有受管 repo 可按同名、同 pool 声明；未受管的同名 dataset 会被拒绝。`enableZfs` 不创建 pool、不设置 hostId、不替代 `boot.supportedFilesystems`、`boot.zfs.extraPools` 或宿主机 ZFS 配置。

`cloudinit` 是保留名称，只能使用 file；声明后仍需通过 CLI 导入 default.raw 或显式指定 seed。若暂时没有 seed，创建实例时使用 `--no-cloud-init`。

chadm 不提供 `virtualisation.chadm.bridges` 选项，不创建 bridge、不添加物理上联网口，也不管理 IP、DHCP、NAT 或防火墙。网桥由宿主机现有网络配置维护；实例通过 CLI 的 `--net bridge=<已有网桥名>` 选择接入目标。启动时 chadm 验证目标是已存在的 Linux bridge，再创建实例 TAP 并接入；目标缺失或不是 bridge 时失败，不会自动创建。

从旧配置迁移时，删除 `virtualisation.chadm.bridges`，并确认对应网桥已由宿主机独立配置接管后再 rebuild，以免旧声明移除后网桥消失。下面假设宿主机已有 `br0` 和 `br1`：

```bash
sudo chadm instance create \
  --name test01 --image ubuntu-test --boot firmware \
  --net bridge=br0 --net bridge=br1 --no-cloud-init
```

不要为实例的 CPU、RAM、磁盘或网卡添加 Nix 声明。已有实例添加网卡仍按 [手册](../HOW-TO-USE.MD#网络与-ip-地址) 冷修改配置。

## 更新、故障和测试

切换软件/内核/固件版本前先恢复挂起的 VM，再正常关机。数据目录已有 repo、image、volume、实例或快照时，初始化服务拒绝更改 dataDir/runtimeDir；需要单独安排迁移，不能用改 option 自动搬家。NixOS generation rollback 不会回退 `/var/lib/chadm`、VM 的磁盘或其 TOML。配置和固件发布使用原子替换；多个 repo 的创建并非一个原子事务，失败后保留已成功的资源，再次执行会校验后继续。

从旧版 module 更新时注意：旧版只初始化宿主机配置，新版默认 `manageHostConfig = true`。已有自定义 cloud_hypervisor 路径时，先停机并确认使用的构建，或设置 `manageHostConfig = false` 保留该配置。

以下验证命令在本地仓库根目录运行；验证 GitHub 版本时可将 `.#` 替换为 `github:cordelia76544/chadm#`：

```bash
# 包编译和 Rust 测试。
nix build .#checks.x86_64-linux.package -L
# NixOS options、合法/非法配置求值。
nix build .#checks.x86_64-linux.module -L
# 一次性部署脚本：使用真实 chadm、临时文件存储，不操作宿主机资源。
nix build .#checks.x86_64-linux.host-setup -L
# 完整 NixOS 测试机，需要可运行测试 VM 的环境。
nix build .#checks.x86_64-linux.deployment -L
# 全部检查；ARM 将系统名换成 aarch64-linux。
nix flake check -L --keep-going
```

兼容非 flake 的求值与测试机入口：

```bash
nix-instantiate --eval --strict nix/eval-tests.nix
nix-build nix/tests.nix
```

部署测试机检查配置、工具路径、共享模板、宿主机独立提供的 bridge、file/cloudinit repo、firmware、元数据实例及重跑不覆盖 VM 配置。fixture 固件不可启动；测试不启动 Guest，不操作真实 ZFS pool。Python 测试另外覆盖锁冲突、链接攻击、声明冲突、保留卷数据及 running/suspended 拒绝更新。

此前已用锁定的 26.05 源码完成 x86_64/aarch64 模块求值，以及 package/flake checks/NixOS 测试机 derivation 实例化；Python 集成测试使用真实 chadm 通过。该记录只代表求值和脚本测试，不代表完整 Nix 编译或 NixOS 测试机运行验收。Nix daemon 连通或其他包构建成功，也不能替代本项目上述检查。

### GitHub 入口验证（2026-10-19）

对提交 `19ed677ee2822c89db87c201bd3b02c342366644` 执行：

```bash
nix build github:cordelia76544/chadm#chadm \
  github:cordelia76544/chadm#checks.x86_64-linux.module --no-link -L
```

GitHub 源码获取、锁定依赖解析和 `module` 检查通过。`chadm` release 编译完成，但测试阶段为 54 通过、24 失败、2 忽略；24 项失败均报告构建临时文件系统不支持文件归属标记所需的扩展属性（`Operation not supported`）。因此本次包构建退出码为 1，未获得构建通过的结论；完整 NixOS 测试机未运行。需要在支持 xattr 的构建环境重试，不能以关闭测试代替验收。
