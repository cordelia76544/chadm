# Phase 5：Instance metadata 与离线存储准备

实现 `instance create/delete/list/show`，list/show 支持 `--json`。本阶段不启动 Cloud Hypervisor、不创建 TAP、不安装 systemd unit、不查询 Guest IP，也不实现 volume attach/detach。下一阶段才加入 TAP/Linux bridge 模块。

## 文件清单

- `src/instance_config.rs`：实例/compute/boot/disk/network 模型、参数解析、名称/MAC/TAP 校验。
- `src/instance.rs`：实例元数据、全局引用校验、创建/删除事务、查询视图和 JSON。
- `src/{cli,main,lib}.rs`：实例命令注册和人工输出。
- `src/volume.rs`：锁内创建/删除入口、镜像来源引用、实例引用保护和归属验证后的回滚。
- `src/image.rs`：锁内检查镜像可用性，拒绝删除被克隆卷或实例引用的镜像。
- `src/storage/{mod,file}.rs`：增加实际需要的 clone_image；reflink 或 sparse copy 后扩展 raw 长度。
- `src/doctor.rs`：检查实例元数据及依赖存储的可用性。
- `tests/instance.rs`、模块单元测试和文档：验证及验收。

不新增 Cargo 依赖。LVM/ZFS 尚未接入。

## 创建命令

已经创建 local file repo 和导入 debian13 image 时：

```bash
sudo ./target/release/chadm instance create \
  --name test01 --image debian13 --boot firmware \
  --cpus 2 --ram 2G \
  --volume root,size=20G --volume data,size=30G \
  --net bridge=virbr0,dhcp

sudo ./target/release/chadm instance list
sudo ./target/release/chadm instance show test01
sudo ./target/release/chadm instance show test01 --json | jq .
```

默认 CPU 为 2、RAM 为 2G、boot 为 firmware。`--repo local` 指定所有新卷的目标 repo；未指定则使用 image 所属 repo。省略 `--volume root` 时自动创建 image virtual size 大小的 root 卷；显式大小不能小于镜像。root 始终是第一张磁盘，其余磁盘保持参数顺序。卷名为 `<instance>-<disk>`，例如 test01-root、test01-data；不采用已经存在的卷，冲突直接报错。

file root 卷从受管只读 raw 镜像复制，创建独立文件与独立所有权 ID，mode 为 0600。支持同一 repo 或跨 file repo 克隆；优先 reflink，失败时 sparse copy。随后 set_len 扩展到目标大小，不扩 Guest 分区/文件系统，也不预留全部物理容量。data 卷为空 sparse raw，不自动格式化。

firmware 路径从可选 `--firmware /absolute/path/CLOUDHV.fd` 或 host config 继承；指定时检查非空普通文件可读。未配置 firmware 仍可准备实例元数据，show 明确提示启动前需要配置，doctor 保留 WARN。此阶段不验证固件架构、Guest 引导程序或实际启动兼容性。

direct boot：

```bash
sudo ./target/release/chadm instance create \
  --name direct01 --image debian13 --boot direct \
  --kernel /actual/absolute/path/vmlinuz \
  --initramfs /actual/absolute/path/initrd.img \
  --cmdline 'root=/dev/vda1 console=ttyS0'
```

kernel 和非空 cmdline 必需，initramfs 可选。路径必须是绝对路径，拒绝 `..`、末级 symlink、设备和空文件。cmdline 不接受控制字符，最多 8192 字节；保存在 0600 的 config.toml，show/JSON 只报告已配置，不展开内容。实际可用的内核格式、驱动、CPU 上限等在 Phase 7 根据已安装 VMM 和宿主环境验证；目前没有执行启动测试。

## 配置与网络标识

每实例保存 `<data_dir>/instances/<name>/config.toml`，schema_version = 1，包括受管标记和资源 generation ID。主结构：

```toml
name = "test01"
image = "debian13"
# schema_version / managed_by / id / image_id 由 chadm 生成

[compute]
cpus = 2
memory = "2G"

[boot]
mode = "firmware"

[[disk]]
name = "root"
volume = "test01-root"
# volume_id 由 chadm 生成，用于防止引用被同名新卷替换

[[network]]
bridge = "virbr0"
tap = "ch-test01-0"
mac = "52:54:00:11:22:33"
mode = "dhcp"
```

以上是结构示意，不是完整可手写导入的配置。名称限制、ID、root 顺序、重复磁盘与全局 writable volume 引用在加载时再次验证。root 卷追加可选 source_image/source_image_id，记录不可修改的来源；旧的空卷元数据仍可读取，无需迁移。

`--net bridge=virbr0,mac=52:54:00:12:34:56,dhcp` 可指定 MAC；未指定时使用 OS 随机源生成 `52:54:xx:xx:xx:xx`。检查非零 unicast、统一小写格式，在持锁范围内避免所有受管网卡间的冲突。用户指定的 unicast MAC 可以是 globally administered；自动生成的必须 locally administered。MAC 一次生成并保存，list/show 不改变它。

本阶段只校验 bridge 名称语法，bridge 是否存在/是否为 Linux bridge 留到 Phase 6。TAP 名称为 `ch-<instance>-<index>`，最长 15 字节，超长报错、不截断；例如一个网卡时实例名最多 10 字节，索引两位时相应缩短。普通资源名仍最多 63 字节，但必须同时满足派生卷名长度。MVP 最多 32 张磁盘和 16 张网卡，用于限制单份配置规模。

## 状态与输出边界

本阶段没有 VMM 生命周期，正常输出 `stopped (metadata only)`；JSON 包含 `state_source="metadata_only"`、`lifecycle_supported=false`。这表示尚未通过 chadm 启动实例，不是已查询 systemd/API 的运行状态。发现对应 runtime 目录、文件或查询权限问题时返回 unknown，并拒绝删除；不把未知 runtime 自动清除。Phase 7 必须改为 systemd/API 派生状态，不能沿用此阶段的推断来管理正在运行的 VM。

show 显示每网卡 MAC、bridge、预定 TAP 和 `IP: N/A (stopped)`；unknown 状态下显示 IP unknown。JSON 使用空 ip_addresses 数组并携带 ip_status，不填猜测地址。将来按 MAC 查询 IPv4/IPv6，见架构文档。

runtime 输出属于计划：`chadm-instance@test01.service`、`<runtime_dir>/test01/api.sock` 和 console.sock；不会为每实例写独立 unit，Phase 7 提供统一 `chadm-instance@.service` 模板。本阶段连 runtime 目录都不创建。

有效配置但镜像/卷/boot 文件不可用时，list/show 返回成功，`available=false`、reason 给出原因，仍尽量显示可解析的磁盘路径。损坏元数据、冲突引用或未完成事务返回 exit 1；clap 用法错误返回 2。

## 删除、引用与回滚

`instance delete test01` 只删除实例元数据；即使依赖存储已经遗失也可删除合法的 metadata-only 实例。保留所有卷、基础镜像和共享模板。实例目录中有未知文件、symlink/hardlink 或 runtime 状态 unknown 时拒绝删除。

引用中的卷不能删除；镜像被实例或 root 卷来源引用时不能删除。清理顺序示例，仅针对自己创建的资源：

```bash
sudo ./target/release/chadm instance delete test01
sudo ./target/release/chadm volume delete test01-root
sudo ./target/release/chadm volume delete test01-data
# 没有其他引用时才能删除基础镜像：
sudo ./target/release/chadm image delete debian13
```

创建前检查所有参数、目标名称/路径、镜像可用性及 MAC 冲突。全程持有一个数据目录锁，通过内部锁内入口避免嵌套 flock。私有 `.create-<id>/intent.toml` 先记录所有计划磁盘及 ID；全部卷成功后才发布实例配置。失败时验证计划 ID 与实际 metadata，再回滚本次创建的卷；出现身份变化或无法确认清理结果时保留事务和证据，拒绝继续相关写操作，不删除陌生资源。掉电后的自动恢复仍属于 Phase 10。

## 测试与手工验证

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
```

新增实例集成测试覆盖 CLI firmware/direct、独立 root 内容与写入、扩容零填充、空 data、默认/显式 repo、MAC 持久化与冲突、非法规格、TAP 长度、引用/资源 generation、元数据链接及未知条目、锁冲突、runtime unknown、存储权限失败回滚和 doctor。单元测试模拟所有磁盘创建后中止，并验证多卷回滚及归属变化时保留证据。

手工验收：运行上述创建/list/show 命令，使用 jq 检查 `state_source`、MAC、disk path 和 IP 状态；确认 raw 的开头与 image 内容一致，root 比 image 大时末尾为零。写 root 不应改变基础镜像。实例存在时删除 root 必须报 attached 错误；删除实例后 raw 仍存在。检查没有创建运行目录、TAP 或 systemd 文件。

## 后续存储环境

用户已报告宿主机有 LVM VG `ch` 和 zpool `tank`。这不是 chadm repo 的注册或所有权证据。本阶段没有探测、创建或修改这些资源；Phase 9 接入 ZFS 时使用专属受管 dataset（如 tank/chadm），避免把整个 tank 作为删除边界。LVM 仍按原计划属于未来 backend，不提前加入 Phase 5。

## 验证记录

71 项测试通过（13 单元、4 CLI、13 image、12 instance、14 repo、15 volume）；cargo fmt --check、clippy（warnings denied）和 release 构建均通过。

release CLI 在独立临时目录完成 firmware/direct 元数据创建、双网卡 MAC 持久化、JSON/jq、root 内容比对及扩容零填充、root 写入不影响 image、引用删除拒绝、实例删除保留卷和显式依赖清理的手工验收。没有创建 runtime 目录，没有使用 VG ch/zpool tank，没有启动 VM。当前实现停在 Phase 5，不自行进入 Phase 6。
