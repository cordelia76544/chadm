# Phase 6：Linux bridge 与 TAP

本阶段只实现网络模块与验证入口，不启动 VM，不安装 systemd 模板。用户的 virbr0 已通过 sysfs 确认存在、是 Linux bridge、administratively UP。执行环境禁止 netlink socket，且未暴露 /dev/net/tun，因此此处没有创建真实 TAP，也没有验证 DHCP。

## 文件与接口

- `src/network.rs`：bridge 检查、TAP 创建/接桥/UP/清理、IP 被动观测与失败序列单元测试。
- `src/command.rs`：短超时参数数组执行，网络查询输出上限 1 MiB；超过上限返回错误，不静默截断。
- `src/instance.rs`：网络 helper 所需的锁内加载入口；存在预定 TAP 时拒绝删除实例，保留归属元数据。
- `src/instance_config.rs`：复用严格接口名校验。
- `src/doctor.rs`：检查实例引用的 Linux bridge 是否存在并 UP、TUN/TAP 节点和 bridge 工具。
- `tests/network.rs`：元数据/锁/runtime 集成测试、显式 opt-in 的宿主机 TAP 生命周期测试。
- `examples/network.rs`：本阶段的开发验证入口，不扩展正式 chadm CLI 资源集合。
- README、架构文档及本文。

`network::prepare(host, instance)` / `cleanup(host, instance)` 从受保护 TOML 加载配置并获取数据目录独占锁。任何对应 runtime 痕迹导致 unknown，拒绝离线修改。Phase 7 的生命周期 helper 使用锁内入口，不在持锁期间嵌套调用公共 wrapper。当前 metadata-only 状态无法识别外部手工启动的 VMM，离线验证入口只用于没有运行 VM 的测试实例。

## TAP 生命周期与安全

prepare 先校验完整实例配置，检查全部 bridge 存在、类型正确且 UP，并确认所有预定 TAP 名称不存在。任何同名接口都返回冲突；不自动采用或删除它。

通过 `/dev/net/tun` 的 TUNSETIFF 原生 ioctl 创建 TAP，使用 TAP、NO_PI、VNET_HDR，与 Cloud Hypervisor v52 单 queue-pair 打开方式一致。设置 TUN_EXCL，拒绝误接已有设备。创建阶段保持非 persistent，持有 fd；设置 `ifalias=chadm:<实例 generation ID>:<网卡 index>` 后才开启 persistence。如果设置归属标记失败，关闭 fd 自动移除新建 TAP。当前 ioctl 实现支持 Linux x86_64/aarch64；多 queue-pair 的参数配合在 Phase 7 核对，不承诺本阶段支持。

接桥前将 TAP MTU 设为 bridge 的当前 MTU，避免默认 1500 的 TAP 接入时降低 jumbo bridge 的 MTU；接桥、设置 UP 使用固定系统目录中的 `ip` 可执行文件与参数数组，3 秒超时；不调用 shell、不修改 bridge 的配置、地址、路由、NAT 或 DHCP，不把 Guest MAC 设为宿主机 TAP MAC。Guest MAC 始终来自已有实例配置，后续交给 VMM。

每次修改前检查 TAP flags、完整归属 marker、ifindex。清理前检查全部网卡归属，存在任何陌生接口则停止；已缺失 TAP 视为成功。新建 TAP 的接桥/UP 失败会按逆序回滚，身份变化时保留设备并报告 rollback incomplete。删除后确认接口消失。不会按名称前缀扫描并删除宿主机设备，也不清除未知 marker；自动 crash/stale 恢复仍属 Phase 10。

marker 由内核 ifalias 保存，是本项目对受管设备的约定；其他 root 管理员可以改变它。检查与命令执行间仍存在外部管理员并发修改的窗口，操作时应避免其他工具同时重命名/替换这些 TAP。chadm 命令之间由元数据锁串行化。Phase 7 接入 systemd 后，必须先确认 VMM 已退出再清理。

`instance create` 继续允许离线准备配置，bridge 可稍后建立；prepare 和 doctor 才检查实际设备。`instance delete` 若预定 TAP 仍存在，拒绝删除元数据，先由 network cleanup 校验归属并清理。

## IP 观测

`network::observe(config, index)` 用 Guest MAC 匹配 `ip neigh show dev <bridge>`，并通过 `bridge fdb show br <bridge>` 确认 MAC 在对应 TAP 上，同时验证 TAP marker 和 bridge master。返回多个 IPv4/IPv6，以及邻居状态和 source=`bridge_neighbor_fdb`。FDB 指向其他端口、未知/重复端口、只有本地 permanent/self 记录或未学习到 MAC 时不给出地址。暂不处理带 VLAN 的 FDB 记录。

没有观测结果为 unknown；工具、权限、bridge 或归属查询失败为 unavailable 并带 reason。STALE 保留状态，仅表示缓存记录，不表示当前可达，更不是 DHCP 租约。此模块不发包扫描、不运行 DHCP、不读假定存在的租约文件、不从 MAC 推算 IP。未启动实例的 show 仍显示 N/A；Phase 7 再接入运行状态和观测查询。

实现依据：[Linux TUN/TAP 文档](https://github.com/torvalds/linux/blob/master/Documentation/networking/tuntap.rst)、[Linux UAPI flags/ioctl](https://github.com/torvalds/linux/blob/master/include/uapi/linux/if_tun.h)、[Cloud Hypervisor v52 TAP 源码](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/net_util/src/tap.rs)、[ip-link](https://github.com/iproute2/iproute2/blob/main/man/man8/ip-link.8.in)、[bridge](https://github.com/iproute2/iproute2/blob/main/man/man8/bridge.8)、[ip-neighbour](https://github.com/iproute2/iproute2/blob/main/man/man8/ip-neighbour.8)。

## 自动验证

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
cargo build --offline --locked --release --examples
./target/release/examples/network check virbr0
```

覆盖 bridge 不存在/错误类型/down、所有网卡预检、已有 TAP 冲突、归属 marker/generation/type 不匹配、逆序清理与幂等性、第二网卡失败回滚、回滚时 ifindex 被替换、sysfs 解析、参数注入/路径穿越、IPv4/IPv6、多 MAC/端口与 STALE、查询 unavailable、锁冲突、runtime unknown、无副作用的缺失实例错误、doctor bridge 状态。

## 宿主机手工验证

对自己创建、尚未启动过 VMM 的测试实例执行，先通过 show 确认 bridge=virbr0、预定 TAP 和持久化 MAC。例子中的 test01 必须已存在；可使用 `--config /绝对路径/host.toml` 指定同一配置。

```bash
./target/release/examples/network check virbr0
sudo ./target/release/examples/network prepare test01
ip -details link show ch-test01-0
bridge link show dev ch-test01-0
# 应显示 UP、master virbr0、chadm:<实例ID>:0 alias；bridge 本身不应被修改。
# 再次 prepare 应返回 already exists；instance delete 应拒绝遗留 TAP。
sudo ./target/release/examples/network cleanup test01
# 再次 cleanup 应成功；TAP 已消失，实例 TOML/MAC 未改变。
sudo ./target/release/examples/network cleanup test01
```

也提供隔离临时资源的 ignored integration test：先执行 `cargo test --offline --locked --test network --no-run`，从其输出取测试可执行文件路径，然后在宿主机运行：

```bash
sudo env CHADM_TEST_BRIDGE=virbr0 /上述测试可执行文件路径 --ignored --exact host_tap_lifecycle --nocapture
```

该测试只创建一个随机测试实例及其 TAP，验证 marker/flags/master、重复 prepare 拒绝、实例删除保护、清理幂等性、再次准备、MAC 不变和 bridge 不变，最后清理测试资源。没有实际 VM，所以不能据此验收 DHCP 和 Guest IP；它们需要 Phase 7/8 的启动验证。

本阶段完成后停止，不自行进入 Phase 7。VG ch 和 zpool tank 保留未修改。

## 本次验证记录

84 项测试通过，1 项需要宿主机权限的 TAP 生命周期测试明确 ignored；fmt、Clippy（warnings denied）、release binary 与 examples 构建均通过。release helper 只读验证 virbr0：ifindex=3、MTU=1500、Linux bridge、administratively UP；lo 被正确拒绝为非 bridge，../virbr0 被参数校验拒绝。当前环境没有 /dev/net/tun 且 netlink 被限制，未运行真实 TAP 创建、VMM、DHCP 或 Guest IP 验收；这些限制不代表宿主机 KVM/firmware 不可用。测试失败时若 TAP 无法安全清理，宿主机测试会保留临时元数据供检查。
