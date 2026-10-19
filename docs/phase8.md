# Phase 8：Console、API 与 inspect

本阶段提供 `chadm instance console <name>` 和 `chadm inspect <name> [--json]`。正常 stop 的 power-button API 已在 Phase 7 接入，本阶段继续使用该实现，不把 VMM shutdown/reboot 当作 Guest 优雅操作。没有新增常驻进程、Cargo 依赖、unit 文件或 drop-in，共享模板保持不变。不推进 Phase 9 ZFS。

## 文件

新增 src/console.rs、src/inspect.rs；更新 cli/main/lib、lifecycle 的串口连接鉴权、hypervisor 的 socket 类型/owner/权限/peer 校验、instance 的内部诊断入口，以及 error 的 TOML 错误内容保护。repo 也拒绝 storage path 覆盖 operation/console 锁目录。更新 tests/instance.rs、tests/repo.rs、README、架构文档和本阶段文档。console 模块包含终端恢复及串口 relay 测试。

## Console

```bash
sudo /usr/local/bin/chadm instance console vmseed01
```

接入已有 Cloud Hypervisor serial UNIX socket `/run/chadm/vmseed01/console.sock`；启动参数仍是 `--serial socket=...` 和 `--console off`，不虚构另一种串口 API。

先验证 root 权限、服务配置一致性、实例注册、共享模板、systemd active/MainPID、runtime instance/invocation ID 与 API Running/Paused，再验证串口 socket 的类型、owner、权限和 SO_PEERCRED UID/PID；连接前后核对 systemd invocation。API 和串口连接共享有界连接与 peer 校验，不允许连接任意路径或 PID 文件指定的进程。

每实例 console 锁放在 data_dir/console-sessions/<instance>/.lock，防止多个 chadm 会话连接同一实例。该锁与 metadata/启停操作锁独立；relay 不持有 metadata 或 operation 锁，另一个终端可以 stop/reboot。连接期间不会创建额外 runtime 文件，所以不干扰 ExecStopPost 清理。停止/crash 后 socket EOF 会结束 console；不自动重连另一 invocation。锁目录保留，flock 随 CLI 退出释放。

TTY 输入进入 raw 模式；Ctrl+C、Ctrl+D、Ctrl+Z 作为原始字节发给 Guest。**Ctrl+]**（0x1d）在本地断开，不发给 Guest，也不停止 VM。Ctrl+] 后同一输入块的字节丢弃，之前的待发送输入有 2 秒有界 flush；输入 EOF 同样断开。socket 输入缓冲最多一块 4096 字节，使用非阻塞 socket/poll 处理写入背压，不缓存无界数据。

正常退出、socket/输入/输出错误、SIGHUP/SIGINT/SIGQUIT/SIGTERM 都通过作用域 guard 恢复终端设置。SIGKILL 无法执行进程清理，手工强杀 CLI 后若终端异常可执行 `stty sane`。连接提示在 stderr，stdout 只写 Guest 原始串口字节，支持重定向。console 输出不回放连接前的启动日志；Guest 必须配置串口输出和登录服务，例如 ttyS0/serial-getty，才能看到预期内容。连接成功后屏幕空白可以按 Enter。

Linux terminal ABI 当前按 x86_64/aarch64 实现；PTY 测试在本机验证 raw/恢复行为。没有给 Guest 分配另一 PTY，也不引入复杂 terminal multiplexer。

## Inspect

```bash
sudo /usr/local/bin/chadm inspect vmseed01
sudo /usr/local/bin/chadm inspect vmseed01 --json | jq .
```

只读显示解析后的实例 TOML、计算/boot/disk/network、可写磁盘路径、只读 cloud-init seed 路径、TAP/bridge/MAC、状态、systemd unit、API/console socket、实际 firmware，以及按启动规则解析的 Cloud Hypervisor program/argv。

启动参数使用 JSON 数组显示，保留参数边界；这是诊断数据，不是待执行的 shell command。direct kernel cmdline 整体显示为 `<redacted>`，config TOML 和 argv 都先遮蔽再输出；不输出 cloud-init 文件内容或 API 响应正文。TOML 解析错误也不回显原始配置行，以免损坏配置中的敏感值出现在 stderr。JSON 的 resolved_config_toml 是经过遮蔽的 TOML 字符串，instance 对象包含结构化磁盘、网络、状态和 IP；command_line 包含 program、args、error 和 cmdline_redacted。

磁盘丢失、firmware 未配置、CH binary 不可用时，inspect 仍显示可解析配置和诊断原因，command_line 标为 unavailable；不会假装参数已成功解析。inspect 返回 0 表示成功生成诊断报告，不表示 VM 可启动；不存在/不合法名称、配置或元数据无法解析等返回非零。它不会创建 resource/runtime、提交 systemd job 或探测 DHCP。

## Graceful shutdown

`instance stop` 仍通过已校验 peer 的 API 请求 `PUT /api/v1/vm.power-button`，等待 Guest 关机和 VMM/cleanup 完成，最多 60 秒。API 请求失败、Guest 不响应或 invocation 改变时明确报错，不自动强杀。需要强制终止时由用户显式执行 `stop --force`。正常 reboot 是同一实例操作锁内 stop 后 start。此行为和现有测试继续保留。

## 验证与宿主机验收

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
```

新增测试覆盖 Ctrl+] 本地过滤/其他控制键保留、真实 PTY 的 raw 设置及正常/错误退出后的完整恢复、外部 SIGTERM 子进程恢复、inspect/JSON 遮蔽 cmdline 和 seed 内容、路径/TAP/unit/readonly argv 解析、不可用存储诊断、无副作用及无效名称。真实 UNIX socket relay 测试明确 ignored，需要宿主机支持 AF_UNIX：

```bash
cargo test --offline --locked --lib serial_relay_preserves_bytes_and_detaches_without_guest_escape -- --ignored
cargo test --offline --locked --lib unix_http_validates_peer_and_reads_split_headers_without_waiting_for_eof -- --ignored
```

停止运行中的 VM 后更新 `/usr/local/bin/chadm`，模板无需改变。已有 vmseed01 和模板时：

```bash
cd /home/davyjones/chadm
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm
sudo /usr/local/bin/chadm inspect vmseed01
sudo /usr/local/bin/chadm instance start vmseed01
sudo /usr/local/bin/chadm instance console vmseed01
# 按 Ctrl+] 返回宿主机；VM 应继续 running。
sudo /usr/local/bin/chadm instance show vmseed01
sudo /usr/local/bin/chadm inspect vmseed01 --json | jq .
sudo /usr/local/bin/chadm instance stop vmseed01
```

建议在 Guest 登录后检查 `cloud-init status --long`；再连接一次 console，在另一个宿主机终端执行 chadm stop，确认 Guest 电源事件、CLI EOF、终端恢复和 TAP/runtime 清理。若 Guest 不响应，用显式 --force 验证断开和清理。没有配置 ttyS0/serial-getty 的镜像可能无输出或登录提示，chadm 不修改 Guest 配置。

源码网络查询在本次执行环境不可用；本机 Cloud Hypervisor v52 `--help` 已确认 serial socket 选项，沿用 Phase 7 已核对的 API 路径和参数。串口支持依据：[v52 serial manager](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/serial_manager.rs)；API 依据：[v52 OpenAPI](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/api/openapi/cloud-hypervisor.yaml)。本机仍未实际启动宿主机 VM，真实 serial/Guest 关机与 cloud-init 结果不能报告为通过。

## 本次验证记录

111 项测试通过，4 项真实 socket/serial relay/TAP/VM 测试明确 ignored；fmt、Clippy（warnings denied）和 release 构建通过。新增真实 PTY 和外部 SIGTERM 子进程恢复测试通过。release CLI 在独立临时目录验证 inspect 文本/JSON、cmdline 遮蔽、只读 seed 路径、磁盘/unit/启动 argv 和 console 的非运行/非 root 拒绝；没有创建 runtime/TAP 或提交 systemd job，验证后清理自己的测试资源。宿主机 binary/template 未安装或修改，VG ch/zpool tank 未使用。
