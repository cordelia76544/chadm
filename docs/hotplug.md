# 磁盘热插拔

`volume attach/detach` 根据实例状态选择行为：stopped 时修改 TOML，running 时调用 Cloud Hypervisor API 并持久化最终配置。仅处理已有、归属可验证的 file raw 卷和 ZFS zvol 数据卷；root 卷和只读 cloud-init seed 不允许热拔插。paused、转换状态、待执行 systemd job、所有权或 API 无法确认时拒绝操作。仍使用共享 systemd 模板，没有常驻 chadm daemon。

## 使用方法

先更新 binary。已运行的 VM 可直接使用下面的 attach/detach；systemd 模板本次无需更新。

```bash
cargo build --offline --locked --release
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm
# test01 需要处于 running；尚未启动时执行 instance start test01。

# local 为已有 file repo；也可以 --repo fast 使用已配置 ZFS repo。
sudo /usr/local/bin/chadm volume create --name hotdata --repo local --size 1G
sudo /usr/local/bin/chadm volume attach hotdata --instance test01
sudo /usr/local/bin/chadm instance show test01
```

在 Guest 中执行 `lsblk -o NAME,SIZE,TYPE,MOUNTPOINTS`，查看新磁盘。新卷没有分区或文件系统，chadm 不代为格式化。Guest 需要 virtio-blk 和 PCI/ACPI 热插拔支持；Guest 启动完成后再测试，API 已接受插盘不代表 Guest 已经识别磁盘。新磁盘可能使用不同的 `/dev/vdX` 名称，不应假定固定盘符。

拔盘前在 Guest 内停止使用该卷的进程，卸载该盘的文件系统，关闭其 swap、device mapper 或其他持有者，并完成写入。chadm 不探测 Guest 挂载状态，不提供强制拔盘参数。

```bash
sudo /usr/local/bin/chadm volume detach hotdata --instance test01
# detach 成功后卷仍保留，可以再次 attach；不要先删除它。
sudo /usr/local/bin/chadm volume show hotdata
```

实例下次启动沿用已提交的磁盘配置。已有运行 VM 的磁盘可从 `vm.info.config.disks` 按经过验证的路径解析原生 ID；新启动的可写磁盘显式使用 `ch-disk-<volume generation ID>`，磁盘顺序改变不改变设备 ID。不会猜测 `_diskN`。

## API 和一致性

实现依据 Cloud Hypervisor v52 官方 [hotplug 文档](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/docs/hotplug.md)、[OpenAPI](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/api/openapi/cloud-hypervisor.yaml)、[vm remove_device](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/vm.rs) 和 [device manager eject_device](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/device_manager.rs)。使用 `PUT /api/v1/vm.add-disk`（path、id、readonly=false、image_type="Raw"）和 `PUT /api/v1/vm.remove-device`（id）。HTTP 204 只说明移除请求已处理；实际 Guest eject 完成前，设备仍可能出现在 device_tree 中。

操作持有实例 operation lock 和全局 metadata lock。检查 systemd 模板、MainPID、InvocationID、ControlPID/Job 和 runtime owner，并用 SO_PEERCRED 验证 API socket 属于当前 VMM。API 请求及回复有大小/时间限制；每次查询与修改前以及提交前重新核对 invocation，拒绝不完整、重复键或不匹配的 JSON。不会执行 shell 字符串。

发送修改请求前写入并同步 `instances/<name>/hotplug.toml`，记录实例/卷 generation、device ID、路径、invocation 和修改前后的磁盘列表，不保存 Guest cmdline 或 seed 内容。只有查询确认插盘存在，或拔盘的 config 和 device_tree 均已无该设备，才原子替换 config.toml 并清理 intent。确认等待最长 30 秒，单次 API I/O 有 2 秒超时。没有自动反向拔盘回滚：插入后 Guest 可能已经使用新盘。

API 超时、回复丢失、Guest eject 未完成、CLI crash 或发布失败都保留事务证据；卷引用检查同时保留修改前后两侧引用，阻止删除和其他实例使用该卷。list/show/inspect 的配置仍表示已发布配置，reason 会提示 pending；不会把尚未确认的插盘列成已提交磁盘。

## 未完成操作恢复

VM 仍在同一个 invocation 运行时，重试同一条 attach/detach 命令。已经确认插入的设备不会重复添加，已经进入 eject 的设备不会重复提交 remove 请求。不同卷或相反操作被拒绝。

若 Guest 不支持 eject，或 VMM 已崩溃/宿主机已重启：

```bash
sudo /usr/local/bin/chadm instance stop test01
# 若正常关机无法完成，可显式 instance stop test01 --force。
sudo /usr/local/bin/chadm instance recover test01
sudo /usr/local/bin/chadm instance show test01
```

recover 必须证明 systemd/cgroup 已停止、runtime/TAP 已清理，再解除 hotplug intent。它保留最后发布的 config.toml：未提交的 attach 被取消；未提交的 detach 仍保留原卷引用。确认 show 后，可在 stopped 状态再次执行所需 attach/detach，再 start。intent 存在时 start（包括 systemd 模板 prepare）拒绝运行；stop 和清理允许执行。不要手工删除 intent 或直接用 ch-remote 修改 chadm 管理的磁盘。

## 验证

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
```

自动测试覆盖 HTTP JSON body/字节长度、结构/重复键校验、stable/legacy ID、路径转义、身份/权限不匹配、invocation 变化、API 回复丢失重试、Guest eject 超时、引用保护、提交和 stopped 事务恢复。`tests/lifecycle.rs` 的宿主机测试已改为运行中添加/移除一个从未挂载的空卷；它默认 ignored，需要 root、实际 KVM/TUN/systemd、已安装 binary/template、兼容 firmware、支持 hotplug 的 Guest 和明确的 CHADM_TEST_IMAGE/BRIDGE。当前沙箱未执行真实 VM 或 ZFS 热插拔测试，也没有修改宿主机服务或 pool。

本次自动测试结果：151 项通过，5 项宿主机/UNIX socket 能力测试默认 ignored。设备树 JSON 形状另已核对 v52 [DeviceTree 源码](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/device_tree.rs)，按原生 ID 到 DeviceNode 的 map 解析；不接受无法确认 eject 的未知结构。
