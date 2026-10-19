# Phase 4：本地 Image 导入

实现 `image create/delete/list/show`，仅 file repo 和本地 path。raw 使用 Rust 文件操作；QCOW2 v2/v3（含压缩镜像）通过 `qemu-img convert` 转为 raw。不启动 QEMU，不实现 URL、实例创建或镜像克隆。

## 修改与新增文件

- `src/image.rs`：镜像模型、TOML 元数据、事务、归属校验和 CRUD。
- `src/image_input.rs`：输入暂存、reflink/sparse copy 和有限 QCOW2 头部校验。
- `src/storage/file_image.rs`：file backend 的导入、只读设置、验证与删除。
- `src/storage/{mod,file}.rs`：增加需要的 image 操作，复用 fd 扩展属性归属标记。
- `src/{cli,main,lib}.rs`：注册 image 命令与模块。
- `src/repo.rs`：即使镜像文件遗失，也拒绝删除仍被镜像元数据引用的 repo。
- `src/{command,doctor}.rs`：有超时的转换执行及镜像可用性诊断。
- `tests/image.rs`、README、架构文档及本文件：测试与验收说明。

不新增 Cargo 依赖。

## 数据与行为

```text
<data_dir>/images/<name>/config.toml
<repo>/images/<name>.raw
<repo>/images/<name>.chadm.toml
```

镜像元数据包含 schema_version、managed_by、随机 ID、name、repo、repo_id、format、source_format、virtual_size_bytes、inode。最终 format 固定为 raw；source_format 记录 raw/qcow2。路径从受管 repo 与校验过的名称派生，配置不接受任意 payload 路径。

导入不修改原文件，也不把输入文件直接注册为基础镜像。先在 data_dir 的私有 `.create-<id>` 目录复制输入；优先尝试 Linux FICLONE reflink，失败时用固定 1 MiB 缓冲复制、跳过全零块。然后检查这份私有副本，避免检查原路径后又打开一个已经被替换的文件。输入必须是非空普通文件；拒绝末级 symlink、目录、设备与 FIFO。

raw 没有通用格式签名：识别 QCOW2 magic/version，并拒绝部分已知其他容器签名，其余内容作为 raw；不按文件扩展名判断，也不承诺识别所有其他容器。导入成功不代表 Guest 能启动或登录。QCOW2 virtual size 来自标准头部；拒绝 backing file、外部 data file、加密、dirty/corrupt 和未知 incompatible feature。头部检查不是完整的 QCOW2 文件系统校验，转换读取由 qemu-img 完成。

QCOW2 使用受信系统目录中的 qemu-img，参数数组为：

```text
qemu-img convert -q -n -f qcow2 -O raw <private-snapshot> <exclusive-created-managed-target>
```

目标先用 create_new 创建并标记所有权，`-n` 跳过工具的目标创建步骤。转换超时为一小时；stderr 持续读取但仅保留前 8 KiB 诊断，不读取用户 PATH、不调用 shell。raw 导入不依赖 qemu-img。参数依据见 [QEMU 官方工具文档](https://www.qemu.org/docs/master/tools/qemu-img.html)，头部字段依据见 [QCOW2 格式文档](https://www.qemu.org/docs/master/interop/qcow2.html)。

基础镜像完成后 chmod 0400，检查 raw 长度、inode、`user.chadm.image-id` 与侧边 marker，再发布元数据。只读权限防止正常流程写入；有权限的管理员仍可修改权限和文件，当前不做内容哈希校验或 immutable inode 设置。Phase 5 必须创建独立 root volume 才能给 Guest 写入。

导入要求源镜像处于离线状态。检查复制前后的长度及 mtime/ctime；这能发现部分并发修改，不是在线一致性快照协议。暂存和目标可能需要额外磁盘空间，稀疏文件不预留全部容量。转换期间持有全局锁，其余资源操作快速返回锁冲突；后续有实际需要再改为更细的锁。

## 删除与失败处理

所有写操作持有数据目录锁，复用目录权限、无符号链接、普通文件/单硬链接校验。删除前验证元数据、repo generation、marker、xattr、inode、长度及只读权限；不删除源文件、不递归删目录、不接受 block device 作为删除目标。未知实例元数据存在时拒绝删除，Phase 5 再按模型检查实际引用。

导入失败回滚本次创建的文件；不能确认清理完成时保留事务和归属证据，不自动清掉可疑文件。删除先把元数据移动到 `.delete-<id>`：raw unlink 失败则恢复元数据；unlink 之后的清理失败保留 tombstone。未完成事务阻止相关写操作，并显示明确错误，需要检查后处理；自动 crash recovery 留到 Phase 10。

list/show 为只读操作，缺少资源目录时不初始化目录。元数据损坏返回 exit 1；有效配置但 payload 不可用时，list/show 成功返回 `available=false` 和 `reason`。`--json` 提供稳定字段，供脚本读取。

## 构建与测试

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
```

首次运行需要已下载 Cargo 依赖；真实 QCOW2 集成测试要求安装 qemu-img。当前环境使用 Rust/Cargo 1.85.1、qemu-img 10.0.13。

测试涵盖 raw 内容与源文件独立性、只读/稀疏、QCOW2 v2/v3/压缩转换内容、backing/external data 拒绝、转换与权限失败回滚、重复和非法名称、未知事务、锁冲突、symlink/hardlink、marker/xattr/inode/repo generation、未知实例引用、JSON 输出及 doctor。命令执行单元测试覆盖非零退出诊断与超时。

## 手工验收

已有 file repo 名为 local 时：

```bash
sudo ./target/release/chadm image create --name debian13 --path ./debian13.raw --repo local
sudo ./target/release/chadm image list
sudo ./target/release/chadm image show debian13
sudo ./target/release/chadm image show debian13 --json | jq .
sudo ./target/release/chadm doctor
```

也可将 `--path` 指向独立 QCOW2；show 应显示 Format: raw、Source format: qcow2。确认实际 raw 的大小与 virtual size 相同、mode 为 0400；修改源文件后镜像内容不变。重复名称、backing chain 镜像、非法名称均应失败，原文件保持不变。

删除仅针对本次验收创建的镜像：

```bash
sudo ./target/release/chadm image delete debian13
```

确认只移除该镜像的受管 raw、marker 和元数据，原始输入及 repo 根目录仍在。若医生检查显示 KVM/systemd 失败，仍须按其执行上下文判断；本阶段没有启动 VM。

## 验证记录

56 项测试通过（10 单元、4 CLI、13 image、14 repo、15 volume）；fmt、clippy（warnings denied）和 release 构建通过。release CLI 的隔离手工验收覆盖 raw/QCOW2 导入、内容比对、jq 解析 JSON、只读权限、list/show/delete 及 repo 根目录保留。当前停在 Phase 4，不进入 Phase 5。
