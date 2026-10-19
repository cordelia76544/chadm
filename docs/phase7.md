# Phase 7：systemd 与 Cloud Hypervisor 启停

更新：最新版本已移除 chadm `--firmware`，固定使用 `<data_dir>/CLOUDHV.fd`；已支持冷 attach/detach，网络模式默认 DHCP。旧 firmware 配置兼容读取但不生效。迁移与操作见 [冷挂载与固定 firmware](cold-attach.md)。

本阶段让 file repo 的独立 raw 卷可以通过 Cloud Hypervisor 启动。只提供一份 `chadm-instance@.service` 模板；实例配置仍来自 TOML。没有 daemon、数据库、QEMU VM、每实例 unit/drop-in 或自动 enable。后续按用户要求补充了 [cloudinit raw seed 挂载](cloud-init.md)。

## 文件与命令

新增 `src/hypervisor.rs`（argv 与最小 UNIX HTTP API）、`src/systemd.rs`（systemctl 与模板校验）、`src/lifecycle.rs`（CLI/短生命周期 helper）、`packaging/systemd/chadm-instance@.service`、`tests/lifecycle.rs`。更新 CLI/main、实例状态及删除保护、network 的启动前验证与 IP 查询时间、doctor、metadata 内部 uid 查询、README 和架构文档。没有新增 Cargo 依赖。

```text
chadm instance start <name>
chadm instance stop <name> [--force]
chadm instance reboot <name> [--force]
```

生命周期命令需要 root。create 仍只创建实例元数据和独立 root/data，不自动启动 VM。list/show 有 JSON；console 与 inspect 后续已在 [Phase 8](phase8.md) 提供。

## 安装一次共享模板

此执行环境只能修改项目和临时目录，没有安装宿主机 unit 或 binary。请在宿主机执行下面的安装方法。更新现有安装前先停止运行 VM，保持 binary/template 版本一致。

```bash
cargo build --offline --locked --release
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm
sudo install -m 0644 packaging/systemd/chadm-instance@.service /etc/systemd/system/chadm-instance@.service
sudo systemd-analyze verify /etc/systemd/system/chadm-instance@.service
sudo systemctl daemon-reload
```

在没有运行 VM 时按实际环境编辑 `/etc/chadm/config.toml`，其中目录应与已有资源一致。运行时也不要手工修改实例 TOML；先停止 VM，再修改配置。例如：

```toml
data_dir = "/var/lib/chadm"
runtime_dir = "/run/chadm"
cloud_hypervisor = "cloud-hypervisor"
```

默认配置文件缺失时仍可使用默认目录；firmware 模式固定从 data_dir/CLOUDHV.fd 启动，请提前以 root-owned 普通文件安装。direct boot 则需要实际兼容的 kernel/initramfs/cmdline。路径存在可读不代表格式/架构/Guest 引导兼容已经验证。

共享模板固定调用 `/usr/local/bin/chadm`，服务只读 `/etc/chadm/config.toml`。root 使用的服务配置、模板、binary 与父目录必须通过权限/类型校验。CLI `--config` 的有效 data/runtime/firmware/binary 设置必须与服务配置相同，否则启动、停止、重启会拒绝；不会把临时配置目录悄悄换成默认目录。

不安装每实例 service 文件，不接受 Phase 7 自定义 drop-in，不 enable 开机启动。实例 test01 的 unit 是 `chadm-instance@test01.service`；宿主机重启后 VM 默认为 stopped。

## 启动

1. 在锁内检查实例、image/volume generation、root 来源、受管 raw 的属性和路径、boot 文件、bridge 和 TAP 冲突。
2. 检查已安装共享模板和 helper，核对本机 Cloud Hypervisor `--help` 支持必需参数。
3. 首次启动时持久化 `instances/<name>/systemd.toml`，记录 chadm、实例名称与实例 ID。它是 systemd 生命周期归属记录，不是 running 标志。
4. 提交 `systemctl start --no-block`，释放全局元数据锁后等待 systemd/API；同实例的 CLI start/stop/reboot/create/delete 用单独操作锁串行化。
5. `ExecStartPre` 校验配置，创建 0700 runtime 目录与 0600 owner.toml，然后准备受管 TAP。
6. `ExecStart` 再检查磁盘与 TAP，构造 argv 并 exec Cloud Hypervisor。VMM 成为 systemd MainPID，chadm 不留常驻进程。
7. `ExecStartPost` 等待 `vm.info` 返回 Running，再结束 readiness helper。启动 timeout 为 unit 60 秒，API readiness 等待 30 秒。

helper 验证当前 INVOCATION_ID 和自身 MainPID/ControlPID。helper 获取全局锁时有短暂有界重试；CLI 等 job 时不持有该锁，避免 prepare/run/readiness/cleanup 死锁。只读 show/list 在遇到锁冲突时仍清晰报 retry 错误。

Cloud Hypervisor argv 使用 `--firmware` 或 `--kernel/--initramfs/--cmdline`，磁盘显式 `image_type=raw,readonly=off`，网络传持久化 TAP/MAC 并固定 `num_queues=2`（一个 queue pair，与本阶段 TAP 兼容），API 使用 `--api-socket path=...`，串口使用 `--serial socket=...`、virtio console off。不添加 `--no-shutdown`，所以 Guest 正常关机后 VMM 会退出。

所有参数通过数组传递，不用 shell。逗号分隔 CH 选项中的路径若含逗号则明确拒绝，避免 option injection；普通空格保持同一参数。socket 路径限制为 Linux 的 107 字节。启动返回 Running 表示 VMM/VM 已进入运行状态，不保证 Guest 已完成启动或可以 SSH 登录。

## 停止与重启

正常 stop 请求 `PUT /api/v1/vm.power-button`，最多等待 60 秒。Guest 响应 ACPI 电源事件并自行关机后，VMM 退出，systemd 自动执行 ExecStopPost。API 请求失败或 Guest 不响应时，返回错误，保留 VM，提示 `--force`。不把 vm.shutdown、vm.reboot 当作 Guest 优雅操作。

`stop --force` 提交 systemd stop，模板以 SIGKILL 结束 unit cgroup，不读取或信任 PID 文件。等待 VMM 退出和 post-stop 结束，再确认 runtime/TAP 都消失；清理不完整返回错误并保留归属证据。强制停止后如果 unit 因信号被标为 failed，但清理已确认完成，会 reset-failed。

reboot 在同一个实例操作锁下先 stop 再 start，强制版本先 force stop；MAC 与磁盘不重建。对 stopped 的 reboot 返回 not running。对已经停止且清理完整的已注册实例，重复 stop 成功。

模板不提供 ExecStop 的 Guest 协议处理，因此直接 `systemctl stop/restart` 或宿主机 systemd 停机走强制路径。需要优雅停止时请使用 chadm stop/reboot，并在关宿主机前停止 VM。

## 状态、IP 与清理

未注册 systemd 的旧实例继续显示 `state_source=metadata_only`，runtime 痕迹导致 unknown；它表示尚未交给 chadm 的 systemd 生命周期，不能用来判断外部手工启动的 VMM。

已注册实例按 systemd 查询 starting/stopping/stopped/failed；active 时调用 vm.info，得到 running/paused/starting/shutdown。API UNIX socket 的 SO_PEERCRED PID 必须匹配 systemd MainPID，查询前后 invocation/PID 要一致。systemd/API/归属或服务配置查询不确定时显示 unknown，不误报 stopped；已停止但 runtime 遗留会显示清理原因并阻止重启/删除。没有 stale PID 文件判断。

running 实例 show 按网卡查询邻居表与 FDB：IPv4/IPv6、来源、邻居状态、查询时间；未发现 unknown，查询失败 unavailable 并带 reason。JSON 的 `ip_queried_at_unix` 只是观测时间，不是 DHCP lease/最后在线时间。IP 查询失败不改变 VM 运行状态。停止实例 N/A。

ExecStopPost 覆盖 Guest 自行关机、VMM crash、启动失败。先确认 MainPID=0、runtime 的实例 ID/invocation，再检查全部已知目录条目与 TAP 归属；只删除 owner.toml、已验证的 api.sock/console.sock 和受管 TAP，不递归删除 runtime。陌生文件、类型/身份变化或不完整 marker 都保留证据并报错。

实例 delete 必须确认 stopped/failed 且 runtime/TAP 已消失，再移除实例 config/注册记录；保留所有磁盘、镜像和共享模板。全局的同名实例操作锁目录保留用于后续安全复用。自动 stale/crash 事务恢复仍在 Phase 10，本阶段不猜测删除遗留资源。

## 测试与宿主机手工验收

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release --bins --examples
```

测试包含 argv/direct/firmware/raw/TAP/MAC、socket/path 边界、HTTP 分片/长度/错误响应与 state 字段、systemd 属性解析、模板/name/PID/invocation、正常 stop 不调用强杀、timeout 保留 VM、force 不依赖 API、清理确认、登记 generation、并发锁及未知 runtime 文件保护。网络模块仍覆盖 TAP 失败回滚和归属检查。

当前环境禁止 AF_UNIX/netlink socket，也未暴露 KVM/TUN；HTTP 字节协议与生命周期通过注入测试验证。三个需要真实环境的测试显式 ignored：UNIX socket/peer、TAP 生命周期、真实 file-backed VM。没有把它们报告为通过。

正常用户在未限制 AF_UNIX 的宿主机可以先跑真实 socket/peer 测试：

```bash
cargo test --offline --locked --lib unix_http_validates_peer_and_reads_split_headers_without_waiting_for_eof -- --ignored
```

安装好上面的共享模板，确认 firmware、已有 file repo/bootable image/instance 与 virbr0，再执行：

```bash
sudo /usr/local/bin/chadm doctor
sudo /usr/local/bin/chadm instance start test01
sudo /usr/local/bin/chadm instance show test01
sudo /usr/local/bin/chadm instance show test01 --json | jq .
systemctl show chadm-instance@test01.service -p ActiveState -p SubState -p MainPID
journalctl -u chadm-instance@test01.service -n 50
```

应显示 running 和 systemd+api，磁盘是独立 root/data raw，MAC 与配置一致。Guest DHCP/IP 观测取决于镜像配置和 bridge 后面的 DHCP server，不保证一定能发现地址。

Guest 启动完成并能处理电源按钮后，再验证 stop/reboot 与 TAP 清理：

```bash
sudo /usr/local/bin/chadm instance reboot test01
sudo /usr/local/bin/chadm instance stop test01
# 不响应电源按钮时显式强制：
sudo /usr/local/bin/chadm instance stop test01 --force
```

可选的隔离 host integration test：`cargo test --offline --locked --test lifecycle --no-run`，取输出中测试可执行文件路径，运行：

```bash
sudo env CHADM_TEST_IMAGE=/绝对路径/bootable-cloud-image.raw \
  CHADM_TEST_FIRMWARE=/绝对路径/CLOUDHV.fd CHADM_TEST_BRIDGE=virbr0 \
  /上述测试可执行文件路径 --ignored --exact host_file_vm_start_reboot_stop --nocapture
```

该测试在默认服务配置中创建随机命名的 file repo/image/instance（1 CPU、512 MiB），执行 start、重复 start 拒绝、运行时 delete 拒绝、force reboot、force stop、MAC 不变与 runtime/TAP 清理，最后显式删除其测试资源。停止或清理失败会保留资源并报告路径；不把未知状态当作可删除。它不修改源 cloud image、VG ch 或 zpool tank。

实际能力依据：[Cloud Hypervisor v52 CLI](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/cloud-hypervisor/src/main.rs)、[OpenAPI](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/api/openapi/cloud-hypervisor.yaml)、[GuestExit/VMM 生命周期](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/lib.rs)、[serial socket 管理](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/serial_manager.rs)、[systemd v257 service](https://github.com/systemd/systemd/blob/v257/man/systemd.service.xml)。

本阶段完成后停止，console/inspect 属于 Phase 8，ZFS 属于 Phase 9，LVM 仍在未来阶段。


## 本次验证记录

97 项测试通过，3 项真实环境测试明确 ignored；cargo fmt --check、Clippy（warnings denied）、release binary 和 examples 构建通过。未新增依赖。release CLI 在独立临时目录验证 file repo/image、root/data、JSON 的生命周期/IP/template 字段、非 root 启动拒绝、资源引用与清理，未创建 runtime/TAP 或提交 systemd job。

systemd-analyze verify 在本执行环境因 SO_PASSCRED: Operation not permitted 被阻止，未声称模板已通过宿主机静态验证。上面的安装步骤包含宿主机 verify；真实 API peer、KVM/TUN、VM 启停与 DHCP 验收也仍需宿主机执行。binary/template 没有安装到 /usr/local/bin 或 /etc/systemd/system，VG ch/zpool tank 未修改。
