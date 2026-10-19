# Phase 2：file repo

## 范围与文件

实现 `repo create/delete/list/show`，driver 只支持 file；list/show 提供 JSON 输出。doctor 增加受管 file repo 检查。没有 volume、image import、ZFS 或 VM 启动功能，不需要已安装 Cloud Hypervisor 才能管理 repo。

新增 `src/repo.rs`、`src/metadata.rs`、`tests/repo.rs` 和本文；修改 CLI、main、error、doctor、lib、README 和架构文档。没有新增第三方依赖，没有引入空的 storage trait。

## 使用

```bash
cargo build --locked --release
sudo ./target/release/chadm repo create \
  --name local --driver file --path /var/lib/chadm/storage
sudo ./target/release/chadm repo list
sudo ./target/release/chadm repo show local
sudo ./target/release/chadm repo list --json
sudo ./target/release/chadm repo show local --json
# 仅空 repo 可删除；storage 根目录保留。
sudo ./target/release/chadm repo delete local
```

repo 的 storage 父目录必须已经存在；默认 data_dir 初始化后可直接创建上述 storage 子目录。允许注册已有的空目录，拒绝接管已有数据的目录。存储路径必须绝对、非根目录、UTF-8，并且不能含 `..` 或控制字符。名称限制为 `[a-z][a-z0-9-]{0,62}`。相同路径或父子路径不能注册成多个 repo，也不能覆盖 chadm metadata/runtime。

## 持久化布局

```text
/var/lib/chadm/
  .lock
  repos/local/config.toml
  storage/
    .chadm-repo.toml
    images/
    volumes/
```

```toml
schema_version = 1
managed_by = "chadm"
id = "0123456789abcdef0123456789abcdef"
name = "local"
driver = "file"
path = "/var/lib/chadm/storage"
```

config.toml 与 storage 归属标记必须完全一致。ID 使用 `/dev/urandom` 生成，重新注册保留下来的空根目录会得到新 ID。目录以 0700 创建，元数据/lock 文件以 0600 创建；umask 可以进一步限制权限。

## 写入、锁与删除

所有 repo 读写使用 data_dir 下的 flock；读操作共享、写操作独占。有冲突时立即返回错误，调用者可重试。锁 guard 显式解锁，避免并行测试或库调用中的 fork 继承 fd 意外延长锁生命周期。进程退出后由内核释放锁，不记录 PID。

创建先把完整 config 写入 `.create-<id>` 并 fsync，再准备 storage 和归属标记，最后 rename 到正式 metadata 目录。错误时按清单回滚本次创建的文件和空目录；已有 storage 根目录保留。初次创建失败可能留下已初始化的 data_dir、repos 和 lock，这是可继续使用的元数据基础目录。

删除要求 marker/config 一致、images/volumes 都为空、storage 没有未知条目、metadata 目录只有 config.toml。先把 metadata 移到 `.delete-<id>`，再清理明确属于该 repo 的空子目录和标记。普通清理错误尝试恢复；最后删除 tombstone。始终保留 storage 根目录，生产代码不调用递归删除，不跟随 symlink，不操作 block device。

路径逐级检查 symlink、目录所有者和权限；元数据打开时使用 O_NOFOLLOW，并拒绝非普通文件、其他用户所有、group/other 可写或有 hardlink 的文件。允许 root 所有的 sticky `/tmp` 作为父目录，但不允许把它作为 repo 根目录。user namespace 中以当前文件系统根目录所有者识别映射后的 root。安全边界信任 root 和当前执行用户；这些用户自身可改写的目录不被视为对同一用户进程隔离的机制。group/other 可写的 workspace 目录需要先选择一个安全父目录，不自动 chmod 用户现有目录。

创建/删除被 kill 或主机断电时，可能保留 `.create-*`/`.delete-*` 或孤立 storage 标记；repo list/create/delete 和 doctor 会报告可见的未完成 metadata 事务，拒绝猜测性清理。show 可以查看仍有正式配置的 repo。此阶段不自动 crash recovery：保留事务配置、核对 ID 和实际目录后人工恢复；不要盲目递归删除。commit 后 fsync 失败可能表示操作已经提交，应检查 list/show 再决定是否重试。

## 输出与退出码

list 按名称排序。无初始化 metadata 时返回空列表，不创建目录。show 返回 name、driver、path、ID、availability；JSON 多提供 schema_version、managed_by、reason。输出不可用 repo 有助于排查，list/show 对合法 metadata 返回 0；脚本应检查 `available`。配置/元数据错误、创建删除失败返回 1；未知 driver 或 CLI 用法错误返回 2。doctor 有不可用 repo 或坏 metadata 则 FAIL。

JSON 当前是针对固定 repo schema 的小型编码器，显式转义引号、反斜杠和控制字符。结构化输出扩大时再引入 serde_json，避免本阶段因新依赖阻断开发。

## 自动测试与手工验证

自动测试覆盖 CLI roundtrip、稳定配置/marker ID、空根目录保留、重复/重叠 repo、危险名称/路径、已有数据拒绝、非空和未知文件删除保护、symlink/hardlink、writable ancestor、锁冲突、损坏 schema、未完成事务和普通失败回滚。权限拒绝测试在 root 下跳过相应 DAC 场景。

使用隔离目录手工验收，无需 root：

```bash
trial=$(mktemp -d /tmp/chadm-phase2.XXXXXX)
cat > "$trial/host.toml" <<EOF
data_dir = "$trial/data"
runtime_dir = "$trial/run"
EOF
./target/release/chadm --config "$trial/host.toml" repo create \
  --name local --driver file --path "$trial/storage"
./target/release/chadm --config "$trial/host.toml" repo show local --json | jq .
./target/release/chadm --config "$trial/host.toml" repo list --json | jq .
# 写一个测试文件后删除应失败，且保留此文件：
touch "$trial/storage/volumes/keep.raw"
./target/release/chadm --config "$trial/host.toml" repo delete local
test -f "$trial/storage/volumes/keep.raw"
rm "$trial/storage/volumes/keep.raw"
./target/release/chadm --config "$trial/host.toml" repo delete local
test -d "$trial/storage"
./target/release/chadm --config "$trial/host.toml" repo list --json
```

验收后保留或人工清理该唯一临时目录。默认系统目录下的命令要使用和创建资源时一致的用户身份。

## 验证记录

在 Rust/Cargo 1.85.1 上完成：

- `cargo fmt --check`：通过。
- `cargo test --offline --locked`：7 项单元测试、4 项原 CLI 集成测试、14 项 repo 集成测试，共 25 项全部通过。当前非 root 环境也执行了权限失败回滚测试。
- 修复锁的 fork 继承生命周期问题后，repo 集成测试连续重复运行 20 次通过（该轮包含当时已有的 13 项 repo 测试；最终又增加 1 项 storage 准备失败回滚测试）。
- `cargo clippy --offline --locked --all-targets -- -D warnings`：通过。
- `cargo build --offline --locked --release`：通过。
- release CLI 在唯一临时目录完成 create/list/show/delete、非空删除保护、根目录保留、doctor repo 可用性检查；含引号、反斜杠和中文的路径经过 Python JSON parser 和 jq 验证。

本机已检测到 `/usr/bin/cloud-hypervisor`，version 为 `cloud-hypervisor v52.0.0`。当前沙箱无法访问 `/dev/kvm` 或 systemd manager，doctor 对这些项返回 FAIL；本阶段没有启动 VM，也没有在默认系统目录下创建 repo。

完成本阶段后停止，不进入 Phase 3。
