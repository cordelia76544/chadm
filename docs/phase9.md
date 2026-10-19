# Phase 9：ZFS repo 与 zvol

更新：最新版本已移除 chadm `--firmware`，固定使用 `<data_dir>/CLOUDHV.fd`；已支持冷 attach/detach，网络模式默认 DHCP。旧 firmware 配置兼容读取但不生效。迁移与操作见 [冷挂载与固定 firmware](cold-attach.md)。

本阶段为已有 file 存储接口加入 OpenZFS 后端，支持 ZFS repo 的 create/delete/list/show、zvol 的 create/delete/list/show、raw/QCOW2 image 导入，以及独立 root 卷复制和实例启停使用 zvol。没有新增依赖或常驻进程；共享 systemd 模板不变。停止在 Phase 9，不推进 Phase 10 或 LVM。

## 文件和数据结构

新增 `src/storage/zfs.rs` 和 `tests/zfs.rs`；更新 repo、volume、image、instance、storage/file/mod、cli/main、doctor、README 和架构文档。

RepoConfig 增加可选 `pool`、`zfs_guid`、`volumes_guid`；`pool` 指 repo 的新 dataset 路径，必须位于已有 filesystem 下。VolumeConfig 增加可选 `zfs_guid`：file 卷仍使用 inode；zvol 的 inode 为 0，持久化原生 GUID。原有 file TOML 无需迁移。JSON 中 GUID 输出十进制字符串，避免脚本对 64 位整数的精度损失。

StorageDriver 工厂按 repo.driver 选择 file 或 ZFS，create/clone 返回实际 VolumeIdentity。marker 位置、存在性和存储目录同步由驱动处理，没有引入插件注册或 async trait。resize、snapshot、hotplug 和 LVM 不在本阶段范围。

## 存储布局

例如 `--name fast --pool tank/chadm`：

```text
ZFS: tank/chadm                         # filesystem，repo root
ZFS: tank/chadm/volumes                 # filesystem，canmount=off，mountpoint=none
ZFS: tank/chadm/volumes/test01-root     # zvol
/dev/zvol/tank/chadm/volumes/test01-root

/var/lib/chadm/zfs-repos/fast/          # chadm 管理的 repo mountpoint
  .chadm-repo.toml
  images/base.raw                      # 独立只读 0400 raw，复用镜像导入
  images/base.chadm.toml
  volumes/test01-root.chadm.toml        # sidecar，不写入 /dev
/var/lib/chadm/repos/fast/config.toml
/var/lib/chadm/volumes/test01-root/config.toml
```

挂载路径由 data_dir 派生，ZFS repo 不接受 `--path`。原生 volumes dataset 不挂载，挂载 filesystem 内同名普通目录只保存 sidecar。基础镜像保持 raw 文件，root 卷通过完整字节复制写入新 zvol；支持 file image → ZFS root、ZFS image → file root。暂不使用 ZFS snapshot/clone。额外容量保持新 zvol 的零内容，chadm 不扩展 Guest 分区或文件系统。

zvol block size 为 16 KiB，显式容量必须是其整数倍；默认 root 容量向上对齐。采用 OpenZFS 默认容量 reservation，不使用 `-s`，所以创建卷需要足够可预留空间。allocated_bytes 使用 referenced，不代表 reservation。镜像在文件系统上继续要求 user xattr 支持。

cloudinit repo 仍仅接受 file driver，名称必须 cloudinit，raw seed 作为最后一块只读磁盘挂载，不改为 zvol，也不接受 ISO。

## 创建、删除和失败边界

仅接受新子 dataset；拒绝 pool root、已有 dataset、路径穿越、snapshot/bookmark 名和命令选项字符串。已有 `tank/chadm` 不会自动收编，需换用尚不存在的子路径。父 filesystem 必须存在；不调用 `zfs create -p`，不创建或删除 zpool，不处理用户的 LVM VG。

原生 create 同时写入本地属性 `chadm:managed-by`、`chadm:repo-id`、`chadm:kind`、`chadm:name`；zvol 还写入 volume-id。后续检查 GUID、属性值和属性来源 local，继承或收到的同名属性不足以证明归属。repo 校验同时检查 root/container GUID、mountpoint 和 /proc/self/mountinfo 的真实来源与 rw 状态。

删除仅针对完全验证过的单个 dataset，拒绝 snapshot、bookmark、未知子对象和文件；从不使用 destroy -r/-f，不 unlink 或擦除 block device。repo 删除要求没有 volume/image 引用，销毁自己创建的空 container/root，保留 pool、父 dataset 和空本地 mountpoint。instance delete 仍保留 volume，需单独删除。

zvol 读写前校验 /dev/zvol 的 root-owned symlink、/dev/zdN 整盘目标、打开后的 block device owner，以及同一 fd 上的 BLKZNAME 和 BLKGETSIZE64；不能只凭路径写任意设备。创建后最多等待设备节点 10 秒；不自动 modprobe、mknod 或调用 udev 操作。Linux OpenZFS 不支持该身份 ioctl 时明确拒绝访问。

所有外部命令使用 argv，无 shell；检查可信 root-owned zfs executable。查询有 3 秒和输出上限，原生变更有 30 秒期限。只有拿到可信 GUID 后才能回滚对应原生对象。超时、原生成功但身份查询失败、部分删除等不确定结果保留 `.create-*`/`.delete-*` 证据并报错；不会猜测删除或自动收编。未完成事务会阻止后续修改，自动恢复留待 Phase 10。

## 宿主机手工验证

先停止正在运行的 chadm VM，更新 binary；共享模板无需修改。下面假定 `tank` 已存在、`tank/chadm` 尚不存在，已有普通 file repo 的 image `ubuntu-test` 和可用 virbr0。镜像名称换成 `chadm image list` 中实际名称。

```bash
cd /home/davyjones/chadm
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm
sudo chadm repo create --name fast --driver zfs --pool tank/chadm
sudo chadm repo show fast --json | jq .
sudo chadm volume create --name zfs-test --repo fast --size 1G
sudo chadm volume show zfs-test --json | jq .
sudo chadm volume delete zfs-test

sudo chadm instance create \
  --name zfstest01 --image ubuntu-test --repo fast \
  --boot firmware \
  --cpus 2 --ram 2G --volume root,size=20G \
  --net bridge=virbr0,dhcp
sudo chadm inspect zfstest01 --json | jq .
sudo chadm doctor
sudo chadm instance start zfstest01
sudo chadm instance show zfstest01
sudo chadm instance console zfstest01
# Ctrl+] 退出 console；在 Guest 中验证 cloud-init 和磁盘。
sudo chadm instance stop zfstest01
```

配置了 cloudinit repo 时沿用默认 default seed；未配置时不挂载，也可显式 `--no-cloud-init`。show 的 IP 仍通过 bridge 上的被动邻居/FDB 观测，chadm 不提供 DHCP。

只在这个测试实例和卷确定不再需要时清理：

```bash
sudo chadm instance delete zfstest01
sudo chadm volume delete zfstest01-root
sudo chadm repo delete fast
# tank 保留；ubuntu-test 基础镜像保留。
```

## 自动测试与限制

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
```

普通测试覆盖 dataset 参数、属性响应、GUID/本地标签、volume ID、snapshot/bookmark/未知子对象删除保护、mount source、容量对齐、legacy file 元数据和身份篡改。真实宿主机测试默认 ignored，不创建 VM/TAP/unit，只在明确指定的已有父 filesystem 下创建唯一测试 dataset，验证空卷、只读 image、独立 root 写入、双向跨后端复制、引用保护和清理。

运行该测试需要 root 和实际 OpenZFS；先编译并从 Cargo 输出获得测试可执行文件路径：

```bash
cargo test --offline --locked --test zfs --no-run
# 把下一行的占位路径替换为上一步输出的实际路径：
sudo env CHADM_TEST_ZFS_PARENT=tank \
  ./target/debug/deps/zfs-实际hash \
  host_zfs_repo_volume_image_clone_cleanup --exact --ignored --nocapture
```

测试会打印临时元数据目录和唯一 dataset；失败时保留它们供排查，不执行递归原生清理。当前执行沙箱没有 /dev/zfs、root 和真实设备访问，因此不能报告宿主机 zvol/VM 实测通过，也未修改 tank 或 VG ch。

实现依据：[OpenZFS zfs create](https://openzfs.github.io/openzfs-docs/man/master/8/zfs-create.8.html)、[原生 GUID 和 user properties](https://openzfs.github.io/openzfs-docs/man/master/7/zfsprops.7.html)。设备 ioctl 同时核对本机 OpenZFS 2.4.4 的 sys/fs/zfs.h 和 zvol_os.c。

本次验证：119 项测试通过，5 项宿主机能力测试默认 ignored；fmt、Clippy（warnings denied）和 release 构建通过。release CLI 在隔离目录通过 file/cloud-init 创建和清理、inspect JSON/argv、cmdline 遮蔽、只读 seed 路径和 console 拒绝检查；没有创建 runtime/TAP 或 systemd job。宿主机 binary/template 未安装或修改，真实 ZFS 集成测试未执行。

## 包管理器 zfs symlink 修复

部分宿主机的 /usr/bin/zfs 是指向 ../sbin/zfs 的 symlink，/bin 也可能指向 usr/bin。旧检查把合法包管理器 symlink 当成非普通文件拒绝，报 `zfs executable must be a trusted root-owned regular file`。现改为逐组件解析并检查目录、每一条 symlink 的 root 归属，以及最终普通可执行文件的 root 归属和权限，执行经过检查的最终路径。不会统一放行 symlink，也不会忽略不可信目录或可写目标；循环和 dangling link 返回错误。

更新 binary 后重试原 repo create 即可。原报错发生于创建前的检查，没有提交 native dataset 创建。无需替换系统 zfs 文件或修改其权限。回归测试模拟包管理器和 merged-usr 链，并验证可写路径/无执行权限/循环/非法目标拒绝；本机已安装 zfs 的路径检查只读执行，不调用 zfs 或访问 zpool。

## 完整 64 位 GUID 序列化修复

ZFS GUID 为无符号 64 位数，TOML 整数只能表达有符号 64 位数。repo 的 `zfs_guid`、`volumes_guid` 和 volume 的 `zfs_guid` 现保存为十进制字符串，避免原先的 `cannot serialize repo: out-of-range value for u64 type`（volume 也受影响）。仍兼容读取已有非负整数格式；内存中的 GUID 类型和所有权比较仍为 `u64`，没有截断或绕过校验。file 元数据省略 GUID，JSON 输出格式保持不变。

回归测试覆盖完整 u64 边界、repo 两个 GUID 分别越界、zvol GUID、旧整数格式和非法/溢出字符串拒绝。更新 binary 后可重试创建；若原失败同时报告 rollback incomplete 或 transaction retained，应保留事务证据，核对 metadata 和原生 dataset 状态，不能直接删除或接管已有 dataset。
