# 冷挂载、固定 firmware 和默认 DHCP

本次补齐已有卷的离线 attach/detach，简化 firmware 选择和 network 参数。更新 cli/main、instance/instance_config、volume、lifecycle、config、hypervisor、inspect、doctor、repo 保护路径及相关测试和文档。不引入 hotplug、后台进程或新依赖，共享 systemd 模板不变。

后续更新：运行中的磁盘 attach/detach 现支持热插拔，原先“不支持 hotplug”的描述仅表示本阶段历史行为；当前行为、限制与恢复方式见 [磁盘热插拔](hotplug.md)。固定 firmware 和默认 DHCP 设计不变。

## 先更新并安装固定 firmware

停止运行中的 chadm VM 后更新程序。默认 data_dir 是 /var/lib/chadm；使用自定义 data_dir 时，把以下固件目标路径改为对应目录下的 CLOUDHV.fd。

```bash
cd /home/davyjones/chadm
cargo build --offline --locked --release
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm
sudo install -d -m 0700 /var/lib/chadm
sudo install -m 0644 CLOUDHV.fd /var/lib/chadm/CLOUDHV.fd
sudo /usr/local/bin/chadm doctor
```

`--boot firmware` 固定读取 `<data_dir>/CLOUDHV.fd`，文件名区分大小写。也兼容用户输入的 `--boot fireware` 拼写，持久化仍是 firmware。chadm 的 `--firmware` 已移除，传入会返回用法错误；Cloud Hypervisor 原生 --firmware argv 保留，传入的始终是固定路径。

旧 host config 的 firmware 和旧 instance boot.firmware 字段兼容读取但不再用于路径选择，新写出的 TOML 不再保存这些覆盖字段。已有 test01 无需重建；先安装固定文件，再启动即可。show/JSON/inspect/doctor 都显示实际固定路径。元数据创建允许 firmware 尚未安装；start 必须验证固定文件存在、非空、普通文件、当前用户所有、无 group/other 写权限、无硬链接别名，拒绝 symlink。root 运行时需要 root-owned 文件。

替换固件时先停止使用 firmware boot 的实例，再用同名 root-owned 文件替换，不使用 symlink。direct boot 的 kernel/initramfs/cmdline 不变，不要求 CLOUDHV.fd。doctor 检查安全和可读性，不验证固件架构或 Guest 实际引导能力。

## 冷 attach/detach

以下假定 local repo 和 test01 已存在：

```bash
sudo /usr/local/bin/chadm instance stop test01
sudo /usr/local/bin/chadm volume create --name extra --repo local --size 30G
sudo /usr/local/bin/chadm volume attach extra --instance test01
sudo /usr/local/bin/chadm instance show test01
sudo /usr/local/bin/chadm inspect test01
sudo /usr/local/bin/chadm instance start test01
sudo /usr/local/bin/chadm instance console test01
```

若 test01 从未启动，无需先 stop。volume create 创建空 raw/zvol，attach 不格式化，也不向 Guest 挂载目录。Guest 中先用 lsblk 确认新增数据盘，再按需要分区、建立文件系统并挂载；已有卷保留原数据。

卸载：

```bash
sudo /usr/local/bin/chadm instance stop test01
sudo /usr/local/bin/chadm volume detach extra --instance test01
sudo /usr/local/bin/chadm volume show extra
sudo /usr/local/bin/chadm instance start test01
```

detach 只删除实例引用，卷及内容仍保留，可以挂到另一停止实例。删除卷仍需单独执行 volume delete；root 卷不能 detach。附加卷追加到可写磁盘列表，cloud-init seed 始终在其后只读接入。detach 可能改变后续 Guest 设备编号，Guest 挂载建议使用文件系统 UUID；chadm 不管理 Guest fstab。

file 和 ZFS 卷均支持，volume 只引用 repo，不增加 --driver。默认 disk alias 使用 volume 名，冲突时自动选择未使用的 data0/data1 等名称；show/JSON 中同时保留真实 volume 名与 generation。每实例最多 32 块可写盘，包括 root。

running、paused、启动/停止转换、待执行 systemd job、残留 cgroup 进程时拒绝，不会偷偷停机或 hotplug。运行中返回包含 `hotplug is not supported yet` 的错误。runtime/TAP 遗留或状态不可确认时也拒绝；先通过 instance recover 验证并清理自己的遗留资源。

## 更新安全与默认网络

修改持有实例 operation 锁和全局 metadata 锁；检查 volume generation、repo/backend/marker/payload 可用性、全局唯一可写引用和 root 约束。发布前再次检查离线状态。只更新 instance config，不修改 volume、image、MAC、cloud-init 或 unit。

配置在同目录写入 0600 临时文件并 fsync，经原子 rename 发布后 fsync 目录。发布前失败保留旧 config，发布后目录同步失败明确报已发布。中断留下的临时文件会阻止进一步修改/删除，不猜测清理未知证据。detach 可用于解除存储不可用卷的引用，但卷 generation 必须一致。

```bash
sudo /usr/local/bin/chadm instance create \
  --name test02 --image ubuntu-test --boot firmware \
  --cpus 2 --ram 2G --volume root,size=20G \
  --net bridge=virbr0
```

`bridge=virbr0` 与 `bridge=virbr0,dhcp` 等价，MAC 未指定时仍自动生成并持久化；显式 MAC 仍可使用。TOML network.mode 缺省也按 dhcp 读取。没有 --net 时不自动添加网卡，不选择默认 bridge；DHCP server 仍由 bridge 后面的网络提供。

## 验证

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
```

新增测试覆盖 attach/detach 往返和数据保留、唯一引用、重复挂载、root 拒绝、generation 变化、锁/runtime/未知更新证据、权限导致的发布失败、运行/helper/job 拒绝，以及固定 firmware argv、旧路径忽略、CLI --firmware 拒绝、DHCP 默认和 fireware 别名、缺失或 symlink firmware 拒绝。既有宿主机 VM 测试补充 running attach 拒绝，ZFS 测试补充跨后端冷 attach/detach；这两项仍需宿主机条件，默认 ignored。

本次验证：133 项测试通过，5 项真实 socket/TAP/VM/ZFS 测试默认 ignored；fmt、Clippy（warnings denied）和 release 构建通过。release CLI 在隔离目录验证冷 attach/detach、卷内容不变、固定 firmware argv、默认 DHCP、root/data/只读 seed 顺序、各资源 list/show JSON、doctor JSON 和清理；未创建 runtime/TAP 或启动 VM。宿主机 binary、firmware、模板未安装或修改，真实 VM/ZFS 冷挂载尚未验收。
