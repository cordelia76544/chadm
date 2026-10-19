# Phase 3：file raw sparse volumes

## 范围与文件

新增 `src/volume.rs`、`src/storage/{mod,file}.rs`、共享 JSON escaping 的 `src/output.rs`、`tests/volume.rs` 和本文。修改 CLI、main、metadata、repo、doctor、lib、README 与架构文档。

实现 `volume create/delete/list/show`，以及最小 StorageDriver trait 和 FileDriver。driver 属于 repo，volume 只引用 repo。没有增加第三方依赖、后台进程、外部 truncate 命令或 qemu-system 调用。attach/detach、resize、image clone 与 ZFS 均不在本阶段。

## 命令

```bash
sudo ./target/release/chadm volume create --name data --repo local --size 30G
sudo ./target/release/chadm volume list
sudo ./target/release/chadm volume list --json
sudo ./target/release/chadm volume show data
sudo ./target/release/chadm volume show data --json
sudo ./target/release/chadm volume delete data
```

容量接受正整数 bytes，或 B、K/KiB、M/MiB、G/GiB、T/TiB 后缀；K/M/G/T 均使用 1024 的幂。禁止零值、负数、小数、不支持的单位或溢出，最大逻辑容量为 i64::MAX bytes，实际仍受文件系统限制。名称沿用 `[a-z][a-z0-9-]{0,62}`。卷名称全局唯一，不同 repo 也不能重复。

raw 使用 `OpenOptions::create_new` 与 `File::set_len` 创建稀疏文件，绝不覆盖已有文件。读取 hole 得到零字节；没有分区或文件系统。逻辑容量不代表已预留可用空间，后续 Guest 写入仍消耗宿主机容量。show/JSON 的 allocated_bytes 来自 Linux stat 的 blocks×512，可能包含文件扩展属性等开销。

## 持久化与归属

```text
<data_dir>/volumes/data/config.toml
<repo_path>/volumes/data.raw
<repo_path>/volumes/data.chadm.toml
```

```toml
schema_version = 1
managed_by = "chadm"
id = "0123456789abcdef0123456789abcdef"
name = "data"
repo = "local"
repo_id = "abcdef0123456789abcdef0123456789"
size_bytes = 32212254720
inode = 123456
```

metadata 与 sidecar 内容必须完全相同。repo_id 绑定 repo 创建代次，删除再创建相同名字的 repo 不会静默接管旧卷。磁盘路径从受管 repo 路径和卷名推导，volume TOML 不接受任意 disk path。

raw 文件另有 `user.chadm.volume-id` 扩展属性，与 config.id 一致。用 fd 调用 fsetxattr/fgetxattr，无 shell 命令；文件系统必须支持 user xattrs，失败时明确报错并回滚。该属性绑定实际 inode，弥补只检查 inode 编号时遇到 inode 复用的不足。接口按 [Linux setxattr man-pages](https://man7.org/linux/man-pages/man2/fsetxattr.2.html) 与 [getxattr man-pages](https://man7.org/linux/man-pages/man2/fgetxattr.2.html) 核对。

检查与删除必须同时满足：repo 归属有效、repo_id 一致、volume metadata/sidecar 一致、raw 是当前用户拥有且没有可写别名的普通文件、单一 hardlink、xattr ID 相同、inode 和逻辑容量相同。raw 为 0600；拒绝 symlink、其他类型、替换文件和手工 resize。修改文件内容本身是正常卷写入，不禁止。

复制/恢复 payload 可能改变 inode 或丢失 xattr；此版本会报告 unavailable，需要人工核对和恢复 metadata。不能只保留 raw 路径再假定可由 chadm 删除。权限边界沿用 Phase 2，信任 root/当前用户，不声称隔离拥有同等目录控制权的其他进程。

## StorageDriver

当前 trait 只有实际使用的 create_volume/delete_volume/get_volume_path/check_volume，orchestrator 负责 metadata 与全局锁，FileDriver 负责 payload。create 返回文件 inode，check 返回实际 allocated_bytes；这是当前 file backend 的身份记录。未来 Phase 9 加 ZFS 时应增加驱动对应的身份类型，不能把 `/dev/zvol` 节点 inode 当作 zvol 的长期身份。本阶段不预先实现多驱动枚举或插件注册机制。

## 事务与删除限制

创建持有全局独占锁，先检查 repo 和全局卷名，再持久化 `.create-<id>/intent.toml`；随后创建 raw、设置文件 ID、同步 payload、写 sidecar 与最终 config，最后原子 rename 到正式 metadata 目录。intent 的 inode=0 表示还没有完成 payload 创建，不是可供运行的 volume config。

普通失败按本次创建清单回滚，raw 只能在身份校验后删除。如果 raw 删除失败，保留事务和归属证据，不继续清空 metadata。首次操作失败可能留下空的 volumes metadata 基础目录，这不代表卷已创建。

删除先检查 ownership 和 metadata 目录中的未知条目，再把 metadata 移到 `.delete-<id>`，删除 raw 和 sidecar，最后移除 tombstone。如果 raw unlink 失败，恢复原配置；raw 已删除后的失败不能恢复卷数据，此时保留未完成删除记录并返回错误。中断、断电或 fsync 失败可能留下事务目录；后续 volume list/create/delete、repo delete 和 doctor 拒绝猜测性清理，需人工检查。完成 commit 后报同步错误，也应先检查实际状态再重试。

repo delete 会扫描 volume metadata 引用，即使 raw/sidecar 已被外部删除，也不能删除仍被引用的 repo。实例尚未实现；若发现非空 instances metadata 目录，volume delete 拒绝操作，等 Phase 5 增加实际引用解析。此阶段没有 --force 绕过安全检查。

## JSON 和诊断

list 按名称排序。JSON 包含 schema_version、managed_by、id、name、repo、repo_id、size_bytes、inode、path、available、allocated_bytes、reason；不可解析磁盘路径时 path=null。合法 metadata 的查询可以返回 unavailable，exit 0，脚本检查 available；坏 metadata 和操作失败 exit 1，用法/容量参数错误 exit 2。

doctor 增加 volumes 行，验证归属、文件身份、大小与可用性，无卷时显示 N/A。这是存储诊断，不需要 VM 启动或 firmware。宿主机 KVM 已由用户验证可用，执行沙箱的设备限制不代表宿主机不支持虚拟化；后续使用 CLOUDHV.fd 时指定实际绝对路径，本阶段不读取或设置推测的 firmware 路径。

## 手工验收

先构建 release，使用唯一临时目录：

```bash
cargo build --locked --release
trial=$(mktemp -d /tmp/chadm-phase3.XXXXXX)
cat > "$trial/host.toml" <<EOF
data_dir = "$trial/data"
runtime_dir = "$trial/run"
EOF
./target/release/chadm --config "$trial/host.toml" repo create \
  --name local --driver file --path "$trial/storage"
./target/release/chadm --config "$trial/host.toml" volume create \
  --name data --repo local --size 30G
./target/release/chadm --config "$trial/host.toml" volume list
./target/release/chadm --config "$trial/host.toml" volume show data --json | jq .
# stat 的 size 是逻辑容量，blocks 是以 512 bytes 为单位的已分配块数。
stat -c 'size=%s blocks=%b' "$trial/storage/volumes/data.raw"
# repo 删除应报仍被 volume 引用：
./target/release/chadm --config "$trial/host.toml" repo delete local
./target/release/chadm --config "$trial/host.toml" volume delete data
./target/release/chadm --config "$trial/host.toml" repo delete local
test -d "$trial/storage"
```

验收后保留或人工清理这个唯一临时目录。不要用 truncate 或编辑 TOML 来绕过归属/容量检查。

## 验证记录

在 Rust/Cargo 1.85.1 上完成：

- `cargo fmt --check`：通过。
- `cargo test --offline --locked`：8 项单元测试、4 项 CLI 集成测试、14 项 repo 集成测试、15 项 volume 集成测试，共 41 项通过。
- `cargo clippy --offline --locked --all-targets -- -D warnings`：通过。
- `cargo build --offline --locked --release`：通过。
- release CLI 在唯一临时目录完成 repo/volume 全流程，创建 30 GiB raw sparse 卷，stat 的 raw 已分配量为 0 bytes；确认逻辑大小、持久 xattr ID、中文/引号/反斜杠路径 JSON，经 Python parser 和 jq 验证。
- 测试移除 raw ownership ID 后删除返回 1，文件保留；恢复测试中删除的原 ID 后成功删除；volume 引用期间 repo 删除返回 1，卷删除后 repo 可删除且根目录保留。

自动测试还覆盖普通权限失败回滚、替换文件、容量异常、symlink/hardlink、marker/代次不一致、坏 metadata、未完成事务、锁冲突、未知 instance metadata 和 doctor 可用性。非 root 环境执行了权限拒绝场景；root 会跳过对应 DAC 场景。

没有在默认系统路径创建资源，没有启动 VM，也没有修改已有镜像或 firmware。完成后停止在 Phase 3，不进入 Phase 4。
