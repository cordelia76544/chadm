# Phase 10：稳定性

更新：最新版本已移除 chadm `--firmware`，固定使用 `<data_dir>/CLOUDHV.fd`；已支持冷 attach/detach，网络模式默认 DHCP。旧 firmware 配置兼容读取但不生效。迁移与操作见 [冷挂载与固定 firmware](cold-attach.md)。

本阶段补充受保护的 runtime/TAP 恢复、准备失败回滚、systemd 状态检查、独立进程锁测试和 doctor JSON。继续使用共享 systemd 模板，不新增 daemon、数据库、Cargo 依赖或每实例 unit 文件。file 和 ZFS 保持同一存储接口。

## 文件

修改 src/systemd.rs、src/lifecycle.rs、src/cli.rs、src/main.rs、src/doctor.rs；扩展 src/network.rs 单元测试和 tests/lifecycle.rs 宿主机测试，新增 tests/stability.rs。更新 README、架构和阶段文档。

## Crash 和 stale 资源恢复

```bash
sudo chadm instance recover test01
```

仅清理当前实例配置所证明归属的遗留 runtime 和 TAP。recover 不启动、关机或杀死 VM；实例正在运行或转换时返回错误。instance start 自动执行同一恢复流程；停止后或对已停止的注册实例执行 stop，也会检查并清理遗留资源。recover 本身无需磁盘/firmware 可用，适合先修复 runtime 再处理存储故障。

恢复持有实例 operation 锁和全局 metadata 锁；检查共享模板、无 drop-in、MainPID=0、ControlPID=0、Job 无待执行任务、unit inactive/failed，以及 cgroup 无进程。systemd 仍保留 ControlGroup 时，读取 cgroup v2 的 cgroup.events populated 字段，检查整个子树；无法确认时拒绝。要求 cgroup v2 挂载于 /sys/fs/cgroup。ControlGroup 已被 systemd 释放或对应 cgroup 目录不存在时无需读取事件。

删除前再次查询 systemd，并检查 invocation 和终止状态。直接 systemctl start 的模板 prepare helper 也必须获取 metadata 锁，所以准备和恢复不会同时修改资源。helper 等待 metadata 锁后重新校验 MainPID/ControlPID/invocation，避免使用等待前的过期上下文。

runtime 必须是受保护的真实目录；owner.toml 必须匹配 managed_by、实例名称、实例 generation 和合法 invocation。允许清理旧 invocation 的文件，但必须先证明当前 unit 已静止。未知文件、伪装成 socket 的普通文件、非法 owner、symlink、权限/归属异常都保留并报错。未完成的 owner 写入不能仅凭目录名称被收编。

TAP 校验既有 generation alias、设备类型、持久化标记和 interface index；所有 NIC 预检通过后才开始清理，清理前再检查 index。旧 bridge 消失或 TAP 只完成了一半设置不妨碍清理。名称相同的 foreign TAP、另一代实例的 TAP 或被替换的 interface 不会被删除。没有 runtime 时也能清理已验证的 orphan TAP；不扫描或删除无关接口。

共享模板既有 KillMode=control-group 和 ExecStopPost 继续承担 VMM crash 后的清理；Restart=no，不自动重启故障 VM。/run 在宿主机重启后自然清空，原生 TAP 也会消失；持久化 config/MAC/volume 不变，下次 start 按模板重新准备。

原生依据：[systemctl Job 输出实现](https://github.com/systemd/systemd/blob/main/src/systemctl/systemctl-show.c)和 [Linux cgroup v2 populated 定义](https://docs.kernel.org/admin-guide/cgroup-v2.html#un-populated-notification)。Job 无任务时实际可能输出空值，不能把它当作解析错误。

## 回滚和资源锁

准备阶段新建 runtime 后，owner 写入失败仅尝试 remove_dir 删除仍为空的目录，不递归删除未知内容；TAP 准备失败先执行既有逆序回滚，再清理匹配 invocation 的 runtime。回滚失败保留归属证据，并在错误中显示检查位置。ExecStopPost 再次执行清理可重复调用。

repo/image/volume/instance 的已知归属回滚、事务目录和 generation 检查继续保留。ZFS 原生操作超时、GUID 未取得、存储事务损坏等不确定结果仍不自动删除或恢复； `.create-*` / `.delete-*` 继续阻止修改，需检查错误、归属和实际存储结果。recover 针对 runtime/TAP，不是任意存储事务修复器，不提供跳过归属检查的 force 选项。

全局 metadata 使用 flock 保证资源创建/引用/删除一致性；每实例 operation 锁覆盖 start/stop/reboot/recover 和删除，console 使用独立锁。等待 systemd/API 时释放全局锁，helper 可继续工作。锁采用非阻塞获取，明确报冲突；不能删除 .lock 绕过锁。新增独立子进程 SIGKILL 测试验证锁由内核释放，之后复用同一个 .lock inode。

## 脚本输出

repo/volume/image/instance list/show 和 inspect 已支持 --json，本阶段加入：

```bash
sudo chadm doctor --json | jq .
```

输出包含 healthy 和 checks 数组，每项包括 name、status、detail。有 FAIL 时仍返回 exit 1；JSON 本身有效不代表宿主机健康。WARN 不导致失败。诊断和 list/show 不创建资源目录，不修复资源，也不主动探测 DHCP。runtime/registration 的 TOML 错误不回显原始配置内容。

## 测试和宿主机验收

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
```

测试覆盖旧 invocation 恢复、幂等性、运行进程/helper/job 拒绝、两次 snapshot 间发生变化、未知 runtime 文件、错误 generation、systemd 查询失败、cgroup events 格式/人口检查、半准备 TAP 与 bridge 消失、进程被杀后的锁释放、doctor JSON 和查询无副作用。此前文件权限失败、镜像转换失败和多 NIC 失败的回滚测试继续运行。

先正常停止运行的 VM，再更新 binary。模板未变，无需生成 unit：

```bash
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm
sudo chadm instance recover test01
sudo chadm instance recover test01  # 重复执行应成功
sudo chadm instance start test01
sudo chadm instance recover test01 # 运行中应拒绝，VM 保持 running
sudo chadm instance show test01
sudo chadm instance stop test01
sudo chadm doctor --json | jq .
```

需要明确进行 crash 注入时，优先运行隔离的宿主机集成测试，避免杀死自己的业务 VM：

```bash
cargo test --offline --locked --test lifecycle --no-run
# 用 Cargo 输出的实际测试可执行文件路径替换占位值。
sudo env CHADM_TEST_IMAGE=/home/davyjones/chadm/ubuntu.img \
  CHADM_TEST_BRIDGE=virbr0 \
  ./target/debug/deps/lifecycle-实际hash \
  host_file_vm_start_reboot_stop --exact --ignored --nocapture
```

测试使用唯一实例/repo 名称，关闭默认 cloud-init；覆盖启动、重复启动拒绝、force reboot/stop、orphan TAP 恢复、通过自己的 systemd unit 对 VMM 注入 SIGKILL、ExecStopPost 清理和重新启动。退出时仅通过 chadm 删除测试资源，清理失败保留证据。它依赖真实 root、KVM/TUN、bridge、systemd 及已安装的匹配 binary/template，在当前沙箱仍默认 ignored。ZFS 宿主机测试见 [Phase 9](phase9.md)。

当前仍未实现 LVM、snapshot/clone CLI、hotplug、registry 和 Guest 网络配置；不把完成本阶段等同于这些功能已具备。

本次验证记录：127 项测试通过，5 项真实 socket/TAP/VM/ZFS 测试默认 ignored；fmt、Clippy（warnings denied）和 release 构建通过。release CLI 在独立临时目录验证 repo/volume/image/instance list/show JSON、doctor JSON 与失败退出码、inspect argv/敏感 cmdline 遮蔽、只读 cloud-init seed 和 console 拒绝。没有创建 runtime/TAP 或提交 systemd job，验证后清理自己的资源。没有安装宿主机 binary/template，未修改 zpool tank 或 VG ch；真实 VM crash 与 ZFS 集成测试尚未运行。

## 首次启动 reset-failed 修复

宿主机首次启动曾因无条件调用 reset-failed 而报 `Unit chadm-instance@test01.service not loaded`，阻止提交 start。现在 daemon-reload 后重新查询 unit 并验证静止状态，仅在 ActiveState=failed 时执行 reset-failed；inactive 的首次启动直接提交 start。reset-failed 或 start 的真实错误仍正常返回，不统一忽略。

回归测试模拟 inactive 实例的 reset-failed 拒绝、failed 重启的 reset/start 顺序，以及提交前 MainPID/ControlPID/Job 或查询异常时不执行动作。更新 /usr/local/bin/chadm 后重试已有实例即可，不需要重建实例、卷或模板。
