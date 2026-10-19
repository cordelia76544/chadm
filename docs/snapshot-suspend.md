# 磁盘快照与落盘挂起

本次增加 `snapshot create/restore/delete/list/show` 和 `instance suspend/resume`。继续使用本地 TOML、共享 systemd 模板和 Cloud Hypervisor API，没有新增 daemon、数据库或 Cargo 依赖。

| 操作 | 保存什么 | 条件 | 恢复后 |
| --- | --- | --- | --- |
| snapshot create | 实例全部可写磁盘 | VM 已停止，runtime/TAP 已清理 | restore 后仍停止，下次正常启动 |
| instance suspend | RAM、CPU/设备状态和全部可写磁盘检查点 | 已登记、稳定 running 的受管 VM | 原 VMM 退出，释放内存，显示 suspended |
| instance resume | 恢复对应磁盘和执行状态 | VM 已停止、保存状态完整、环境匹配 | 新 VMM 接续原 Guest 执行 |

磁盘快照不保存 RAM；落盘挂起不是仅把进程留在 paused 状态，也不调用 Guest 内部 S4/hibernate。只读基础 image 和 cloud-init seed 不重复复制，但继续由现有实例及 checkpoint 引用保护。

## 先更新 binary 和共享模板

本次模板有修改：为 RAM restore 提供较长启动窗口。仍只安装一个模板，不生成每实例 unit 或 drop-in。

```bash
cargo build --offline --locked --release
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm
sudo install -m 0644 packaging/systemd/chadm-instance@.service /etc/systemd/system/chadm-instance@.service
sudo systemd-analyze verify /etc/systemd/system/chadm-instance@.service
sudo systemctl daemon-reload
sudo chadm doctor
```

整套升级前先安排正常停止相关 VM。已有挂起状态时，先恢复并正常关机，或明确放弃挂起状态后再升级。不要在保存状态期间升级/重新安装 Cloud Hypervisor、升级宿主机内核或替换 firmware/kernel/initramfs。

## 磁盘快照

例如已有 test01：

```bash
sudo chadm instance stop test01
sudo chadm snapshot create --name before-upgrade --instance test01
sudo chadm snapshot list
sudo chadm snapshot show before-upgrade --json
sudo chadm instance start test01
```

从未启动且已确认 stopped 的实例不必先执行 stop。create 捕获所有 root/data 卷，不能只选择部分盘；当前不接受运行中磁盘快照，不声称提供 Guest 文件系统冻结或应用一致性接口。

需要回到快照磁盘内容时：

```bash
sudo chadm instance stop test01
sudo chadm snapshot restore before-upgrade
sudo chadm instance start test01
```

restore 会覆盖现有卷在快照之后的写入，保持卷身份和实例配置，VM 不会自动启动。所有磁盘先进行可用性/身份检查，再持久化 restoring 状态并依次恢复。多卷、跨 repo 的恢复不承诺原子完成；途中失败时禁止启动，保留源快照并重试同一个 restore，直到全部磁盘完成。

快照存在期间不允许通过 attach/detach 改变磁盘配置，也不允许删除实例、被引用的卷、image 或 repo。CPU、RAM、boot、MAC、seed 和卷配置也应保持不变；手工修改配置会导致恢复/启动校验失败。需要改配置时先显式删除快照。

不再需要回滚点时：

```bash
sudo chadm snapshot delete before-upgrade
```

删除磁盘快照保留当前活动卷，不回滚数据。无未完成恢复事务时，磁盘快照可在 VM 正常运行期间删除；仍会校验后端归属。

### 后端方式

| 后端 | 创建 | 恢复 | 删除 |
| --- | --- | --- | --- |
| file | 同 repo 文件系统上的 FICLONE/reflink；不支持时 sparse copy | 优先 reflink，否则保留原 inode 和容量逐段覆盖 | 校验 inode、容量、只读权限和 snapshot xattr 后删除独立文件 |
| zfs | 每个 zvol 的原生 `zfs snapshot` | 原生 `zfs rollback`，不带 `-r/-R` | 按记录 GUID、本地归属属性和 volume ID 校验，非递归 destroy 单个快照 |

file 快照 payload 位于 `<repo.path>/volumes/.chadm-snapshot-<snapshot-id>-<volume-id>.raw`，只读保存。reflink 可以节省初始复制空间，实际行为取决于文件系统；后续 CoW 写入和回退复制仍需要空间。

ZFS 原生名称为 `<repo.pool>/volumes/<volume>@chadm-<snapshot-id>`，记录其完整 u64 GUID。zvol 自身 GUID 不变。可以同时保存多个快照，但 ZFS 恢复要求目标是该卷最新快照且无 bookmark；需要回到更旧快照时，先明确处理冲突对象。不会为了回滚自动删除新快照、bookmark 或 clone，原生 holds/clones 等错误也会保留并返回。

暂停或停止期间没有 Guest 写入，因此按顺序捕获多个后端；不宣称跨 pool 的原子 ZFS 快照，也不把 file repo 视为 Btrfs subvolume。

## 落盘挂起

```bash
sudo chadm instance suspend test01
sudo chadm instance show test01
sudo chadm snapshot list
```

成功后 systemd unit 无 VMM 进程，TAP 和 `/run/chadm/test01` 已清理。RAM/CPU 状态在持久化存储中，show/list 的实例状态为 suspended。挂起检查点自动命名为 `suspend-<instance-generation-id>`，可从 snapshot list 找到。

流程：

1. 验证 systemd 模板、PID/invocation、API peer、实例配置及实际磁盘/网卡拓扑；拒绝 hotplug pending 和未管理设备。
2. 在 `/var/lib/chadm/snapshots/<name>/config.toml` 写入 creating 事务，保护卷引用。
3. 通过 `vm.pause` 暂停并确认 Paused；同步磁盘，再创建各后端检查点。
4. 通过 `vm.snapshot` 保存 VMM 执行状态和内存，核对文件、fsync 并保护为只读。
5. 记录 saved，再通过 systemd 结束已暂停的 VMM，等待 cgroup/runtime/TAP 清理，提交 suspended。

API 内存保存最多等待 30 分钟。磁盘复制和暂停期间会有停顿；这不是 live migration。没有 Guest agent 或应用冻结服务；VM 被暂停时的 RAM 与磁盘共同构成恢复点。

磁盘复制期间持有全局元数据锁，大卷使用复制回退时可能让其他管理命令遇到锁冲突。RAM 保存阶段会释放全局锁，但保留实例操作锁和持久化引用保护。

### 空间和保存位置

默认执行状态位于：

```text
/var/lib/chadm/snapshots/suspend-<instance-id>/
  config.toml
  state/
    config.json
    state.json
    memory-ranges
```

以实际 Cloud Hypervisor 输出及清单为准，程序核对保存文件的 inode、大小、修改时间和权限，不接受 symlink/未知文件。内存内容可能接近分配的 RAM 容量；data_dir 所在文件系统需要有空间。磁盘检查点则位于每个 repo 的后端，不把 zvol 数据转存成 raw 副本。

保存状态包含 Guest 内存和配置，应按敏感数据保护。目录为受保护的本地目录，文件只读保存；没有额外的静态加密功能。snapshot show 和 inspect 不输出 RAM、seed 内容或完整 kernel cmdline。

当前 file URL 支持要求 data_dir 使用 ASCII 字母、数字、`/._-`；有空格、逗号或非 ASCII 路径时会在暂停前拒绝挂起。

## 恢复挂起实例

```bash
sudo chadm instance resume test01
sudo chadm instance show test01
sudo chadm instance console test01
```

resume 校验实例配置、全部卷/repo generation、保存文件和宿主机环境，先恢复磁盘，再通过原共享模板重新创建 TAP，启动：

```text
cloud-hypervisor --api-socket path=/run/chadm/test01/api.sock \
  --restore source_url=file:///var/lib/chadm/snapshots/<name>/state,resume=true,memory_restore_mode=copy
```

其他硬件配置和 serial/TAP 路径来自 Cloud Hypervisor 自己保存的 config.json。不会把 `--restore` 与一组冷启动 `--disk/--cpus` 参数混用。

copy 模式把保存内存读入新的 Guest RAM，Running 确认后才自动清理挂起检查点。之前的普通磁盘快照保留。恢复允许宿主机重启后进行，但需要相同宿主机 CPU/虚拟化环境，以及相同 Cloud Hypervisor 文件身份、宿主机 kernel release 和启动文件身份；程序检查 version、inode/设备、大小和修改时间。即使版本号相同，重新安装后换了 binary inode，也会拒绝恢复。不能承诺跨版本、跨宿主机迁移兼容性。

共享模板的 TimeoutStartSec 为 1900 秒；恢复的 ready helper 最多等待 1800 秒，CLI 最多约 1865 秒。普通冷启动仍使用约 30 秒 ready/65 秒 CLI 窗口。大内存恢复会更慢，API/磁盘错误仍可能提前失败。

## 失败、取消与显式放弃

查看阶段：

```bash
sudo chadm snapshot list --json
sudo chadm snapshot show suspend-实际实例generation --json
sudo journalctl -u chadm-instance@test01.service -n 100 --no-pager
```

第二条命令的名称用 list 输出的实际值替换。阶段含义：

`available` 表示已记录的 payload/身份可用，不替代 resume 时对宿主机、binary 和启动文件的兼容性检查。

| phase | 说明与后续操作 |
| --- | --- |
| creating | 捕获中或中断；不作为完整恢复点使用 |
| ready | 完整普通磁盘快照 |
| saved | 状态已完整保存，尚未提交 VMM 停止；可重试 suspend 完成停止 |
| suspended | 可尝试 resume |
| restoring | 磁盘恢复中/中断；禁止启动，完成同一 restore/resume 后再运行 |
| resuming | 磁盘恢复完成，尚待 VMM 启动 |
| consumed | 已在 exec 前标记恢复尝试，不能再次回放旧内存 |
| deleting | 清理中/中断，重试对应清理流程 |

如果原 VMM 仍是同一个 invocation 且 Paused，显式 `instance resume` 可以取消未完成挂起、原地继续运行，再清理已知检查点。若内存保存失败产生未登记的部分文件，原 VM 可以已恢复运行，但清理会报错并保留证据；需检查后处理，不会递归清空目录。

如果新 VMM 已经恢复为 Running，但 CLI 中断或清理失败，再执行 `instance resume` 会核对恢复 invocation，完成清理，不重复恢复。若恢复启动已被尝试后失败，不能自动回放：Guest 可能已经写盘，旧 RAM 与新磁盘不再匹配。consumed 状态会保留并拒绝重放。

明确不再保留执行状态、准备冷启动时，先证明 VM 已停止，再显式删除该挂起记录：

```bash
sudo chadm instance stop test01 --force
sudo chadm snapshot delete suspend-实际实例generation --discard
sudo chadm instance start test01
```

`--discard` 删除保存的 RAM 和后端检查点，保留当前活动卷；它不会先把磁盘回滚到检查点，也不会保留 Guest 尚未落盘的内存内容。不要把 discard 当作无损 resume。未完成 restoring 阶段不能直接删除源快照，需完成恢复。

快照存在期间的普通 start/reboot、修改卷引用或删除实例都受保护；普通磁盘 ready 快照允许正常 start。`instance stop` 和 `instance recover` 不隐式丢弃保存状态。原有 recover 仍负责 runtime/TAP，不是任意快照事务修复器。

损坏/中断的 manifest、未知文件、创建后尚未记录身份的原生对象等都保留，需审查；不要手工移除归属属性、`.next.toml` 或整个 snapshots 目录来绕过检查。

## 验证范围

实现核对 [Cloud Hypervisor snapshot/restore](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/docs/snapshot_restore.md)、[OpenAPI](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/api/openapi/cloud-hypervisor.yaml)、[OpenZFS snapshot](https://openzfs.github.io/openzfs-docs/man/master/8/zfs-snapshot.8.html) 和 [rollback](https://openzfs.github.io/openzfs-docs/man/master/8/zfs-rollback.8.html)。

自动测试覆盖 file 多盘回滚/身份保持、篡改提前拒绝、引用保护、配置漂移、恢复重试、状态文件校验、consume 防重放、部分删除重试、ZFS GUID/属性/最新快照检查。已有默认 ignored 的宿主机测试补充真实 ZFS snapshot/rollback 和 VM suspend/resume；当前受限执行环境未实际运行这些测试，不能声称真实 Guest 内存恢复或 Windows 恢复已经验收。

宿主机验收应使用可丢弃的实例，先验证 Guest 中一个长寿命进程和内存状态，suspend 后确认旧 MainPID 消失，再 resume 并确认 Guest 进程/状态延续。验证磁盘快照时写入测试数据，停机创建快照，再写入不同数据，停机 restore 后确认原内容。不要在业务 VM 上直接测试回滚。
