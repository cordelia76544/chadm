# chadm 架构审查与分阶段设计

## 结论

采用单个 Rust package（库加 CLI binary）、本地 TOML 元数据、systemd 管理 VMM、独立 storage/network 模块。当前不需要多 package workspace、tokio、数据库或常驻 daemon。本文记录初始架构审查，后续类型和模块按阶段实现；Phase 1 提供 help、version、doctor，已完成阶段见 [Phase 2](phase2.md)、[Phase 3](phase3.md)、[Phase 4](phase4.md)、[Phase 5](phase5.md)、[Phase 6](phase6.md) 与 [Phase 7](phase7.md)。

## 需要修正或明确的设计

1. **优雅关机与 VMM 停止不同。** 官方源码的 `vm.shutdown` 调用 VM shutdown，不能承诺 Guest 刷盘。正常 stop 用 `PUT /api/v1/vm.power-button` 通知 Guest，再等待 Guest/VMM 退出；Guest 需支持并处理电源按钮。超时保留 VM，返回清晰错误和 `--force` 提示。正常 reboot 可先优雅 stop 再 start；`vm.reboot` 会重建 VM，不代表 Guest 优雅重启。强制操作通过 systemd 的 unit/cgroup 结束进程，不相信 PID 文件。
2. **Phase 7 必须先实现正常 stop 所需 API。** 原计划把 graceful shutdown 放在 Phase 8，但 Phase 7 的 stop 不能伪装为优雅停止。Phase 7 做最小 power-button 请求和退出等待；Phase 8 做 console、inspect 和 API 错误处理完善。阶段不合并。
3. **serial socket 是真实接口。** 当前 CLI 用 `--serial socket=/run/chadm/test01/console.sock`，HTTP API 用 `--api-socket /run/chadm/test01/api.sock`。两种 socket 协议不能混用。serial 默认可能不启用，须显式配置。后续启动前检查本机 `--help`/版本是否支持所需参数；console 客户端做 terminal raw mode、双向转发和 Ctrl+] 退出，并在错误/信号退出时恢复终端。
4. **firmware 使用固定文件。** 按最新需求，chadm firmware boot 固定使用 data_dir/CLOUDHV.fd，不接受 CLI 或实例/宿主机路径覆盖。Cloud Hypervisor 原生 argv 仍使用其真实 --firmware 参数；doctor 校验文件安全与可读性，不能保证架构或实际启动兼容。
5. **direct boot 不保证任意 `/boot/vmlinuz` 都可用。** 内核镜像格式、架构、virtio 驱动、initramfs 和 root 路径必须匹配。initramfs 可以是可选项，但 kernel 和合适 cmdline 必须明确；不修改宿主机 kernel。固件启动也要求 cloud image 具有可用引导程序。
6. **TAP 名称上限。** Linux 接口名称最多 15 字节。保持 `ch-<instance>-<index>`，创建实例时按实际网卡数量验证；长名称明确报错，不悄悄截断造成碰撞。普通资源名称建议限定 `[a-z][a-z0-9-]{0,62}`，禁止路径分隔符、点号和控制字符。
7. **MAC 示例需要解释。** `52:54:xx:xx:xx:xx` 的首字节满足 locally administered/unicast 条件；未指定时使用 OS 随机源生成并持久化，检查 chadm 管理范围内的重复值。不承诺避免桥外部主机的 MAC 冲突。DHCP 只是 Guest 配置约定，不会替 Guest 自动设置网络。
8. **instance create 缺少目标 repo。** 建议 Phase 5 增加 `--repo local`，所有新卷默认使用它；未指定时使用 image 所属 repo，绝不猜测多个 repo 中的第一个。`root` 定义为从 image 克隆的启动卷，放在磁盘列表第一项；省略 `--volume root` 时默认使用 image 的 virtual size。用户给出的 size 必须不小于 image virtual size。
9. **扩容 raw/zvol 不等于扩容 Guest。** root/data 卷容量与 Guest 分区、文件系统容量不同。chadm MVP 只扩展块设备容量，Guest 自己处理分区和文件系统。不要宣称 20G 参数会把 Guest 文件系统自动变成 20G。
10. **cloud image 可启动不等于可登录。** 不做 cloud-init 就不能保证 SSH 密钥、密码或用户名可用。MVP 明确不生成 cloud-init；用户提供已经可登录的镜像。现有 `cloudinit.raw` 不是本项目自动生成的资源。
11. **systemd 使用共享模板。** Phase 7 安装一个 `chadm-instance@.service`，实例 test01 对应 `chadm-instance@test01.service`；不为每个实例生成 service 文件。实例参数从各自 TOML 读取，有限内部入口负责准备资源、exec Cloud Hypervisor 和退出清理；无需常驻进程。清理覆盖 Guest 自行关机、崩溃和启动失败。暂不 enable，宿主机重启后默认为 stopped；Restart 默认关闭。详细边界见下文补充设计。
12. **模板参数仍需校验。** 模板通过 `%i` 传实例名，内部入口重新验证名称；不使用会反转义名称的 `%I`，也不按路径转义实例名。`Command::args` 可以防 shell 注入，但模板的固定路径仍受 systemd 引号、反斜杠与 `%` specifier 规则约束。VM 参数由 Rust 构造 argv，不能拼接 shell 命令。
13. **状态不能持久化为真相。** 不保存 `running=true` 或依赖 PID；从 systemd ActiveState/SubState 与 API 查询派生 `running/stopped/starting/stopping/failed/unknown`。manager/API 不可达时不冒充 stopped，拒绝 attach/detach/delete。运行中数据卷 attach/detach 的后续实现采用官方 API、稳定设备 ID 和持久化 intent，等待 Guest eject 后才释放引用；边界和恢复见 [磁盘热插拔](hotplug.md)。
14. **删除语义要明确。** instance delete 默认只删除 stopped 实例的元数据及经过归属校验的运行时资源，不删除共享 systemd 模板，不隐式删 root/data 卷；卷独立删除且必须未被引用。repo delete 默认仅允许空 repo，绝不递归删除 repo 根目录。image 有引用时拒绝删除，保留来源可追踪性。后续若加级联删除，需要显式参数和单独审查。
15. **安全基础不能全部等到 Phase 10。** 名称校验、持有锁的检查与修改、原子写配置、资源归属验证和失败回滚应从首次写资源的 Phase 2 开始；Phase 10 扩大故障注入和恢复测试。对可写目录要验证 root 所有权/权限，防 symlink/TOCTOU；仅字符串 starts_with 不够。
16. **ZFS 管理边界。** repo 的 `pool` 实际可以是 dataset（如 `tank/chadm`），名称应明确说明。要求专属 parent dataset，标记 chadm 所有权，创建并管理其 `volumes/<name>`，删除时验证 dataset 名、类型、标记与 metadata 一致；不用 `zfs destroy -r`，不直接 unlink 或擦除 `/dev/zvol`。
17. **QCOW2 输入不可信。** Phase 4 显式识别格式和 virtual size，拒绝带外部 backing chain 的导入；调用 qemu-img 时传参数数组、显式格式和受管输出路径。不要仅按扩展名判断 raw/qcow2。运行时一律 raw，不启动 qemu-system。

Phase 5 已实现 metadata-only 的实例配置、独立卷与 MAC。此阶段没有 systemd/API 查询，输出明确包含 metadata_only/lifecycle_supported=false；runtime 痕迹导致 unknown 和删除拒绝。上文运行状态派生与优雅生命周期要求在 Phase 7 接入，不能把当前 metadata-only 状态用于管理已启动 VM。用户已准备 VG `ch` 与 zpool `tank`，记录为后续环境，本阶段没有修改或注册它们。

## 官方接口依据

核对时间：2026-10-04，官方 main 源码与文档；不把 main 当作宿主机已安装版本。

- [Cloud Hypervisor CLI 源码](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/cloud-hypervisor/src/main.rs)：firmware、kernel、initramfs、api-socket 参数；serial 选项由 ConsoleConfig 解析。
- [官方命令说明](https://www.cloudhypervisor.org/docs/prologue/commands/)：serial 支持 socket、pty、tty 等模式。
- [官方 ConsoleConfig 解析实现](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/src/config.rs)：socket 路径解析及校验。
- [VMM 生命周期实现](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/src/lib.rs)：vm.shutdown、vm.reboot、GuestExit 与 VMM 退出的区别。
- [VM power button 实现](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/src/vm.rs)：通知 Guest 电源按钮；支持程度仍随架构变化。
- [OpenAPI](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/main/vmm/src/api/openapi/cloud-hypervisor.yaml)：vm.power-button、vm.shutdown、vm.reboot、vmm.shutdown。

## systemd 模板与 instance show IP（补充需求）

本节记录后续阶段的要求，当前实现已到 Phase 8；TAP/bridge、共享 systemd 模板、start/stop/reboot、console、inspect 和运行实例 show 的地址观测已实现。

### 一个模板管理所有实例

模板名固定为 `chadm-instance@.service`，实例 unit 名为 `chadm-instance@<name>.service`。模板随安装提供；创建、删除实例不写独立 service 文件，不用每实例 drop-in 或 shell 环境变量复制 VM 配置。实例配置的唯一来源仍是 `/var/lib/chadm/instances/<name>/config.toml`。

Phase 7 中模板通过受限内部命令完成生命周期：`ExecStartPre` 校验并准备存储/TAP；`ExecStart` 读取配置、构造 argv，使用 Rust `Command::exec` 替换为 Cloud Hypervisor，使 VMM 成为 systemd 主进程；`ExecStopPost` 做幂等清理，包括启动失败路径。内部入口同样验证名称、实例归属和路径，不信任调用者传入任意路径。外部 CLI 正常 stop 先发送 power-button 并等待，确认 Guest 已停止后才停止 unit；超时保持运行并返回错误，`--force` 才强制停止。

模板及 CLI 必须使用一致的宿主配置。Phase 7 明确共享模板的配置路径选择；若 `--config` 与服务配置不一致，启动应拒绝并解释原因，不能悄悄回到默认目录。等待 systemd job 时不能持有内部准备/清理入口也需要的锁，以免死锁；仍需通过资源锁和 systemd job 状态防止 start/delete/attach 并发竞态。

systemd 的模板与 `%i`/`%I` 语义以[官方 unit 文档](https://github.com/systemd/systemd/blob/main/man/systemd.unit.xml)为依据。删除某个实例绝不移除共享模板；将来支持开机自启时，也只处理该实例的 enable 链接。

### 显示 IP，区分观测结果与未知状态

`chadm instance show` 必须按网卡显示 IP，支持多个 IPv4/IPv6 地址。IP 属于运行时查询结果，不作为 DHCP 配置字段持久化，也不能在停止/重启后把旧地址当作当前地址。

Cloud Hypervisor v52.0 的 `vm.info` 返回 VM 配置、状态等信息，没有报告 Guest DHCP 地址的接口；不能用配置中的网络字段冒充 Guest 实际 IP。依据：[对应版本的官方 OpenAPI](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/api/openapi/cloud-hypervisor.yaml)。

MAC 是 chadm 管理的持久化网卡标识。Phase 5 创建实例时，每张网卡优先使用用户指定的 MAC；未指定则通过 OS 随机源生成 locally administered、unicast MAC（如 `52:54:xx:xx:xx:xx`）。统一格式、校验有效性，并在锁内检查与受管网卡的冲突，再写入实例 TOML 的 `[[network]].mac`。Phase 7 每次启动都把此值传给 Cloud Hypervisor，停止、重启或重新创建 TAP 不重新生成 MAC；show 同时显示 MAC 和查询到的 IP。

查询关联为 `instance → network → bridge + MAC → observed IP addresses`。MAC 本身不包含 IP，普通 Linux bridge 的 FDB 只提供 MAC 与端口关系；需要邻居表的 IP/MAC 映射才能显示地址。若未来接入外部 DHCP 租约源，可用 MAC 作为匹配依据之一，但需验证租约有效期和服务器标识规则（部分服务器使用 client identifier），不能仅凭固定 MAC 承诺固定 IP。MVP 不增加租约查询后端。

MVP 采用按需查询：读取宿主机 bridge 的邻居表，按持久化 MAC 匹配地址，同时校验 TAP 的 bridge 归属与 bridge FDB 的 MAC/端口关联。使用 netlink 或 `ip -j neigh show dev <bridge>`、`bridge -j fdb show` 的参数数组，不执行 shell、不主动扫描网络、不增加 DHCP server 或常驻进程。

这只能提供观测结果：Guest 未与宿主机通信、bridge 没有三层地址或缓存为空时，可能无法发现 IP。显示 `unknown`，不把未知当成无地址。过滤 FAILED/INCOMPLETE 等没有可用映射的条目；STALE 条目标记缓存状态，不宣称当前可达。JSON 为每个网卡提供地址数组、查询状态、来源、邻居状态和查询时间；空数组须配合 `unknown`、`unavailable` 或 `not_applicable`，不能丢失查询失败原因。查询时间不是 DHCP 租期或 Guest 最后在线时间。

示例输出（字段值仅示意）：

```text
Network:
  net0
    Bridge: virbr0
    TAP: ch-test01-0
    MAC: 52:54:00:11:22:33
    IP: 192.168.122.42 (observed, neighbor: REACHABLE)
Runtime:
  Systemd unit: chadm-instance@test01.service
```

停止状态显示 `IP: N/A (stopped)`；运行但未发现时显示 `IP: unknown`；权限或查询失败显示 `IP: unavailable` 及原因。IP 查询失败不会把运行中的 VM 标为 stopped。

阶段安排：Phase 5 的 show 先提供 IP 字段并显示 stopped/N/A；Phase 6 增加地址发现模块和解析测试，覆盖 MAC/bridge/TAP 匹配、多地址、STALE、未知及查询失败；Phase 7 接入运行实例的查询。若以后要求保证获得完整 Guest IP，需另行设计 Guest 上报或受支持的外部 DHCP 租约查询，不能承诺仅靠 bridge/TAP 就可靠获取。

## 建议目录结构

```text
Cargo.toml                 # 一个 package；未来有真实需要再拆 workspace
src/
  main.rs                  # stderr、exit code
  lib.rs
  cli.rs                   # clap parsing
  config.rs                # host config 与目录约定
  error.rs
  command.rs               # 受限进程调用
  doctor.rs
  model.rs                 # Phase 2 起逐个加入资源模型
  metadata.rs              # Phase 2：校验、锁、原子 TOML 写入
  repo.rs                  # Phase 2
  volume.rs                # Phase 3
  storage/{mod,file,zfs}.rs # file 与 OpenZFS 后端
  image.rs                 # Phase 4
  image_input.rs           # Phase 4：私有输入副本及有限格式校验
  instance.rs              # Phase 5
  instance_config.rs       # Phase 5：配置模型与规格校验
  network.rs               # Phase 6
  hypervisor.rs            # Phase 7：argv 与最小 API
  systemd.rs               # Phase 7：systemctl 与共享模板校验
  lifecycle.rs             # Phase 7：有界 CLI/helper 编排
  console.rs               # Phase 8
packaging/systemd/chadm-instance@.service  # Phase 7：唯一共享模板
tests/
docs/architecture.md
```

模块按阶段添加，不放空 stub。Phase 2 新增 metadata.rs/repo.rs，Phase 3 新增 volume.rs、storage/{mod,file}.rs 和共享输出工具 output.rs，Phase 4 新增 image.rs、image_input.rs 和 storage/file_image.rs，扩展 storage trait 的镜像操作；Phase 5 新增 instance.rs、instance_config.rs，扩展卷克隆和资源引用校验；Phase 6 新增 network.rs（TAP/bridge 与 IP 观测）、网络测试和宿主机验证 example；Phase 7 新增 hypervisor.rs/systemd.rs/lifecycle.rs、唯一共享模板及测试；其他后续模块尚未创建。

```text
/etc/chadm/config.toml                 # 可选 host config
/var/lib/chadm/repos/<name>/config.toml
/var/lib/chadm/volumes/<name>/config.toml
/var/lib/chadm/images/<name>/config.toml
/var/lib/chadm/instances/<name>/config.toml
<file-repo>/images/<image>.raw          # 不可写基础镜像
<file-repo>/volumes/<volume>.raw
/run/chadm/<instance>/{api,console}.sock
/run/chadm/locks/                      # 后续全局/资源 flock
```

配置与 payload 分开。读命令不创建目录；写命令在校验权限后仅初始化需要的目录。TOML 加 schema_version，未知字段和不支持版本报错，避免拼写错误或静默丢字段。

## 核心数据结构草案

这些类型按需加入，Phase 1 只实现 HostConfig/Layout。Name 是构造与反序列化时都验证的 newtype；path 从受管 ID 派生，不信任 TOML 中任意 payload 路径。

```rust
struct RepoConfig {
    schema_version: u32,
    id: ResourceId,                  // 创建时生成、持久化的所有权标识
    name: Name,
    storage: RepoStorage,
}
// serde 内部 tagged enum，driver = "file" / "zfs"
enum RepoStorage { File { path: PathBuf }, Zfs { pool: String } }

struct VolumeConfig {
    schema_version: u32,
    id: ResourceId,
    name: Name,
    repo: Name,                     // driver 只属于 repo
    size_bytes: u64,
    source_image: Option<Name>,
}
struct ImageConfig {
    schema_version: u32,
    id: ResourceId,
    name: Name,
    repo: Name,
    virtual_size_bytes: u64,
    format: ImageFormat,            // 导入结果只有 Raw
}
struct InstanceConfig {
    schema_version: u32,
    id: ResourceId,
    name: Name,
    image: Name,
    compute: ComputeConfig,
    boot: BootConfig,
    disk: Vec<DiskConfig>,
    network: Vec<NetworkConfig>,
}
struct ComputeConfig { cpus: u16, memory: MemorySize }
// serde tag = "mode"，对应 [boot] 下的 mode；禁用不匹配字段
enum BootConfig {
    Firmware { firmware: Option<PathBuf> },
    Direct { kernel: PathBuf, initramfs: Option<PathBuf>, cmdline: String },
}
struct DiskConfig { name: Name, volume: Name }
struct NetworkConfig { bridge: BridgeName, tap: TapName, mac: MacAddress, mode: NetworkMode }
enum NetworkMode { Dhcp }
enum InstanceState { Running, Stopped, Starting, Stopping, Failed, Unknown }
```

MemorySize/volume size 接受并校验 `2G` 等人类输入，内部用 checked 算术转换 bytes；定义 G=GiB，防零值/溢出。memory 可以按示例序列化为规范字符串。磁盘顺序持久化；多个实例引用同一个可写卷一律拒绝。不重复保存 attached_to，而是锁下扫描实例引用，避免两份 metadata 不一致。

storage trait 只在 Phase 3 出现，初始最小接口：

```rust
trait StorageDriver {
    fn create_volume(&self, volume: &VolumeConfig) -> Result<VolumeIdentity>;
    fn delete_volume(&self, volume: &VolumeConfig) -> Result<()>;
    fn get_volume_path(&self, volume: &VolumeConfig) -> Result<PathBuf>;
    // Phase 4 已加入 import/check/delete/get_image_path；Phase 5 已加入 clone_image。
}
```

Phase 3 用 FileDriver；Phase 9 已加入 ZfsDriver，通过 repo.driver 的小型工厂返回 Box<dyn StorageDriver>，无插件注册表或 async trait。VolumeIdentity 分别携带 file inode 或 ZFS GUID；volume_exists、marker path、目录同步也由驱动处理，避免向 /dev 写 sidecar。ZFS repo 的 raw image 保存在其挂载文件系统，volume 使用原生 zvol，详见 [Phase 9](phase9.md)。resize 没有 CLI 需求，暂不加。clone 完成之前创建卷不对外可见；缩小一律拒绝。file 用 create_new + set_len 生成 sparse raw，copy/reflink 后检查容量；ZFS 先创建 zvol，再有边界地复制 raw 内容。

## Phase 1 具体计划和验收

1. 创建 Cargo package、clap CLI、库/binary 分工；仅注册 version、doctor。
2. HostConfig 默认 `/etc/chadm/config.toml`、`/var/lib/chadm`、`/run/chadm`；显式配置必须存在，未知字段报错，读命令不创建目录。只允许显式绝对 binary 路径或默认的可信目录查找。
3. 定义带路径与 source 的 error；业务/环境错误 exit 1，clap 用法错误 exit 2，help/version exit 0；不在生产代码 unwrap/expect/panic。
4. doctor 检查 KVM 当前用户可访问性、CH version、systemd manager 可联系性、iproute2、qemu-img、显式 firmware。核心依赖缺失 FAIL；qemu-img 或未配置 firmware WARN（direct boot/raw image 不依赖它们）。未实现的 repo/ZFS/bridge 检查显示 N/A，不能假报 OK。
5. 外部探测不调用 shell，stdin 关闭、单条命令 3 秒超时、stdout 限量；版本探测失败仍继续其余检查。
6. 单元测试验证配置校验/解析、进程失败/超时和报告 exit 语义；CLI 测试验证 help/version、非法命令、显式配置失败和无资源目录副作用。
7. 执行 fmt、test、clippy，手工运行 help/version/doctor。若工具链不可用，记录未验证项，不宣称测试通过。验收后停止，不进入 Phase 2。

Phase 1 无删除、存储写入、unit/TAP 创建或 VM 启动。权限和所有权的完整写入安全边界应在 Phase 2 首次修改宿主资源之前实现。


## Phase 7 实现补充

首次交给 systemd 前，实例目录写入带实例 ID 的 `systemd.toml` 注册记录。它表示生命周期归属，不保存 running/stopped。未注册实例延续离线元数据准备模式并明确 state_source=metadata_only；已注册实例必须查询 systemd，active 时再通过已验证 MainPID 的 UNIX API socket 查询 vm.info，失败显示 unknown。服务配置改变到其他 data/runtime 目录也导致 unknown，不能把原 VM 忘记并删除卷。

模板统一使用 /usr/local/bin/chadm，读取 /etc/chadm/config.toml。不使用每实例 unit/drop-in。内部 helper 同时验证 INVOCATION_ID、MainPID/ControlPID 和模板；runtime owner 记录实例与 invocation 身份。CLI 的实例操作锁与 helper 的全局元数据锁分开，systemctl --no-block 提交 job 后释放全局锁再等待。ExecStopPost 在 VMM 退出后安全清理已验证 TAP 和已知 socket；自动恢复未知/stale runtime 留给 Phase 10。具体安装、限制与测试见 [Phase 7](phase7.md)。


## Cloud-init seed 补充

按用户要求增加保留名称 cloudinit 的 file repo（kind=cloudinit），使用已有 image 导入/归属验证来管理 raw seed。创建实例默认选择 default.raw；选择和 generation 写入实例 TOML，启动追加只读 virtio 磁盘，已有实例配置保持不变。只接受 raw，拒绝 ISO/QCOW2；不生成或输出 seed 内容。具体接口、限制与验收见 [cloud-init.md](cloud-init.md)。


## Phase 8 实现补充

已提供 instance console 和 inspect/JSON。console 校验 systemd、runtime、API 和串口 peer 后进入有界 poll relay；Ctrl+] 本地退出，raw terminal 使用 guard 恢复，同实例 console 锁与启停锁独立。inspect 在只读锁下解析配置和启动 argv，整体遮蔽 kernel cmdline，不读取或输出 seed 内容；不可用存储保留诊断报告。graceful shutdown 沿用 Phase 7 的 power-button 与退出等待。接口、测试及宿主机验收见 [Phase 8](phase8.md)。

## Phase 10：恢复边界

instance start/stop/recover 使用同一受保护恢复路径；systemd Snapshot 增加 Job、ControlGroup，终止状态要求 MainPID、ControlPID 和 Job 均为零。旧 runtime 只有在两次 systemd 查询确认静止且 cgroup 无进程后才能清理；TAP 继续按 alias/generation/index 验证。operation/global/console 锁职责保持独立，无常驻恢复线程。doctor 增加 JSON，资源 list/show/inspect 保持既有 schema。细节、故障注入和未恢复的存储事务边界见 [Phase 10](phase10.md)。

## 冷挂载与固定 firmware 补充

volume attach/detach 只在停止状态更新 instance 的 disk 引用，不修改 volume payload 或接入运行中 VMM。持有 operation/global 锁，核对 generation、唯一引用、systemd/cgroup 和 runtime/TAP，再通过 fsync 临时文件、原子 rename、目录 fsync 发布 config。root 始终第一块且不能 detach。固定 firmware_path 从 data_dir 派生，legacy firmware 字段只作为旧 TOML 输入兼容；网络未指定 mode 默认 DHCP。详细测试和迁移见 [冷挂载与固定 firmware](cold-attach.md)。
