# Cloud-init raw seed 补充

更新：最新版本已移除 chadm `--firmware`，固定使用 `<data_dir>/CLOUDHV.fd`；已支持冷 attach/detach，网络模式默认 DHCP。旧 firmware 配置兼容读取但不生效。迁移与操作见 [冷挂载与固定 firmware](cold-attach.md)。

本阶段补充已有 NoCloud seed 的导入、持久化选择和只读挂载，不推进 console/inspect、ZFS 或 LVM。

## 接口与数据

repo 的 `driver` 描述存储实现，`kind` 描述用途。新增 `kind = "cloudinit"`，只允许 `driver = "file"` 且名称恰好为 `cloudinit`。普通 repo 默认 `kind = "storage"`；旧普通 repo 的 TOML 没有 kind 仍可读取。`cloudinit` 为保留名称，不指定 kind 时也识别为 cloudinit。

seed 复用 image 的安全导入和删除接口，不新增一套 CRUD/存储驱动。导入 `--name default` 后，实际文件是 `<repo.path>/images/default.raw`，元数据和归属验证规则与 image 相同。image 名称仍是全局唯一的，其他 repo 已有同名 image 时明确拒绝，不覆盖。

cloudinit repo 仅接受扩展名 `.raw` 的离线普通文件，不允许 qcow2 转换或 ISO。检查私有导入快照，拒绝 qcow2 内容以及 ISO 9660 的 CD001 标识，因此将这些文件改名成 `.raw` 也不能绕过。不会自动生成 seed，也不验证 seed 内部的 NoCloud 配置；需要用户提供正确的 FAT/ext 等 raw 磁盘，包含 user-data 和 meta-data。原始文件保持不变，受管副本权限为 0400，父目录受保护，不输出 seed 内容。

创建实例时：

- 没有 cloudinit repo 且未指定 seed：不挂载，保持原有创建方式。
- 存在 cloudinit repo：默认选择 image `default`，缺失或不可用时报错，且不留下实例/卷。
- `--cloud-init custom`：选择该 repo 中名为 custom 的 seed，不传路径或 `.raw` 后缀。
- `--no-cloud-init`：明确跳过默认 seed，与 `--cloud-init` 冲突。

选中的 image 名称和 generation ID 持久化：

```toml
[cloud_init]
image = "default"
image_id = "<managed image generation>"
```

启动再次核对 repo、image 归属与 generation，然后在所有可写 root/data 磁盘之后增加只读 virtio seed 磁盘。默认 seed 可以共享，VM 无法通过磁盘写入修改它。实例 show/JSON 显示 seed 名称和 readonly，不显示内容。seed 被实例引用时不能删除，cloudinit repo 非空时也不能删除。实例删除保留 seed，与保留 volume 一致。该 repo 不能用于 volume 创建或 VM 基础镜像。

默认选择仅发生在实例创建时；已有实例不会自动挂载。当前没有为已有实例添加/更换 seed 的命令，也没有 hotplug。修改 default 需要先解除所有实例引用；重新创建同名 seed 的 generation 不会被旧实例静默接受。

同一 default seed 的 meta-data（包括 instance-id/hostname）和凭据会被多个实例复用；chadm 不自动为每个实例改写它们。需要不同配置时，导入不同名称的 seed 并显式选择。cloud-init 的具体执行结果由 Guest 中的 cloud-init 服务决定。

## 修改文件

新增 src/cloud_init.rs；更新 cli、repo、image、volume、instance_config、instance、hypervisor、main、lib、network 的配置构造及 tests/instance.rs。更新 README、架构和 Phase 7 文档。没有新增依赖或改变 systemd 模板。

## 验证

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
```

新增测试验证：默认选择、显式选择、跳过、缺少默认 seed 时无卷分配、引用删除保护、generation 替换拒绝、0400 权限、只读磁盘参数及顺序、外部 repo 拒绝、禁止 seed 作为基础镜像/volume repo、ISO/QCOW2 改名拒绝与导入回滚、kind/名称约束及 CLI 冲突参数。原有真实 socket/TAP/VM 测试仍需宿主机环境。

## 宿主机手工验证

停止运行中的 VM 后更新安装的二进制。共享 systemd 模板安装方法见 [Phase 7](phase7.md)。以下使用默认 host 配置，已有 ubuntu-test 基础镜像、virbr0 和项目内 CLOUDHV.fd；名称存在时不要重复创建。

```bash
cd /home/davyjones/chadm
cargo build --offline --locked --release
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm

sudo /usr/local/bin/chadm repo create --name cloudinit --kind cloudinit \
  --driver file --path /var/lib/chadm/cloudinit-storage
sudo /usr/local/bin/chadm image create --name default --repo cloudinit \
  --path /home/davyjones/chadm/cloudinit.raw
sudo /usr/local/bin/chadm repo show cloudinit --json | jq .
sudo /usr/local/bin/chadm image show default --json | jq .

sudo /usr/local/bin/chadm instance create --name vmseed01 --image ubuntu-test \
  --boot firmware \
  --cpus 2 --ram 2G --net bridge=virbr0,dhcp
sudo /usr/local/bin/chadm instance show vmseed01 --json | jq .cloud_init
sudo /usr/local/bin/chadm instance start vmseed01
sudo /usr/local/bin/chadm instance show vmseed01
sudo journalctl -u chadm-instance@vmseed01.service -n 80 --no-pager
sudo /usr/local/bin/chadm instance stop vmseed01
# Guest 不响应时使用：
# sudo /usr/local/bin/chadm instance stop vmseed01 --force
```

Guest 登录后可以查看 `cloud-init status --long` 验证 seed 是否被识别。当前执行环境未实际启动 VM，不能声称 Guest cloud-init 或 SSH 已验收。

## 本次验证记录

完整测试 102 项通过，3 项真实 socket/TAP/VM 测试明确 ignored；fmt、Clippy（warnings denied）和 release 构建通过。release CLI 在独立临时目录导入项目现有 cloudinit.raw，验证 kind/JSON、默认 seed 选择、0400 权限和引用删除保护，然后清理自己的测试资源。未创建 runtime/TAP、安装 binary/unit 或启动宿主机 VM；源 cloudinit.raw 未修改。Guest cloud-init 实际执行仍待宿主机验收。
