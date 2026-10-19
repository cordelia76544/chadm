# Windows 镜像配置

导入已经安装好的 Windows raw 或独立 qcow2 模板时添加 `--windows`。chadm 将 `windows = true` 写入 image 配置和存储归属 marker；创建实例时将标记复制到实例自己的 config.toml。之后启动按实例配置生成参数，file 和 ZFS 根卷使用相同规则。

```bash
cargo build --offline --locked --release
sudo install -m 0755 target/release/chadm /usr/local/bin/chadm

sudo chadm image create \
  --name windows-template --path ./windows.qcow2 --repo local --windows
sudo chadm image show windows-template

sudo chadm instance create \
  --name win01 --image windows-template --repo fast \
  --boot firmware --cpus 4 --ram 8G \
  --volume root,size=80G --net bridge=virbr0 --no-cloud-init
sudo chadm inspect win01
sudo chadm instance start win01
```

这里 local 是已有 file repo，fast 是已有 ZFS repo；root size 必须至少等于模板的虚拟容量。qcow2 仍只用 qemu-img 转为只读 raw 基础镜像，实例获得独立 root zvol。也可使用 file repo 作为实例卷的目标，不启动 qemu-system-*。

## 实际参数

Windows profile 自动附加以下 CPU 参数，不改变用户给出的 CPU 数量和 RAM：

```text
--cpus boot=4,max=4,kvm_hyperv=on,max_phys_bits=39
--firmware /var/lib/chadm/CLOUDHV.fd
```

`kvm_hyperv=on` 启用 Cloud Hypervisor 的 KVM Hyper-V 支持；39 位物理地址采用保守兼容默认值，并非宣称对所有 Windows 都是最快配置。该 profile 当前针对 x86_64/KVM，需要 firmware boot，direct boot 会在分配实例卷前明确失败。固定 firmware 路径仍为 `<data_dir>/CLOUDHV.fd`。启动前检查已安装 Cloud Hypervisor 的 help 是否声明上述 CPU 选项。

依据 [Cloud Hypervisor v52 Windows 文档](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/docs/windows.md)、[CPU 配置源码](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/vm_config.rs) 和 [x86_64 load_payload 源码](https://github.com/cloud-hypervisor/cloud-hypervisor/blob/v52.0/vmm/src/vm.rs)。Windows 文档的旧示例用 `--kernel CLOUDHV.fd`；v52 的源码也支持 `--firmware` 加载相同 payload，因此保留 chadm 已有 firmware 模式。

标记没有安装或修改 Guest 内容：模板需已配置 UEFI 启动和 virtio-blk/网络驱动。串口 console 依赖 Guest 预先启用 SAC/EMS，图形桌面可使用预先配置的远程访问。该标记不负责 Windows 安装、TPM 配置或 Guest 初始化。cloud-init seed 的选择沿用现有规则；示例通过 `--no-cloud-init` 跳过它，已经配置相应 Windows 初始化工具时可显式选用 seed。

## 配置与兼容

- image list/show 显示 windows/generic profile，JSON 添加布尔 `windows` 字段。
- instance show 显示启用的 Windows CPU 参数，JSON 和 inspect 包含实例自己的 `windows` 标记。
- 旧 image/instance 配置缺少字段时按 false 读取，保持原启动参数。
- 已有实例不会因为导入其他 Windows 镜像而被更改。已有镜像若导入时未标记，可用新名称重新导入并添加 `--windows`；不需要手工修改归属 marker。
- cloudinit repo 的 seed 不允许使用 `--windows`。

主要实现修改在 cli、image、instance/instance_config、hypervisor、lifecycle 和 main；没有修改共享 systemd 模板，也没有新增依赖。

## 验证

```bash
cargo fmt --check
cargo test --offline --locked
cargo clippy --offline --locked --all-targets -- -D warnings
cargo build --offline --locked --release
```

回归测试覆盖 CLI 标记、raw/qcow2 导入与原内容一致、marker、JSON、配置继承/持久化、实际 argv、冷挂载后保留 profile、CPU 参数能力检查、direct boot 提前拒绝、seed 拒绝和旧元数据兼容。本次 156 项测试通过，5 项宿主机/UNIX socket 能力测试默认 ignored。测试使用普通数据文件验证配置流程，未在当前沙箱内实际启动 Windows VM，也未修改宿主机 binary、服务、存储池或 Guest。
