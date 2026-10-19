use chadm::cli::{
    BootMode, Cli, Command, ImageAction, InstanceAction, RepoAction, RepoDriver, SnapshotAction,
    VolumeAction,
};
use chadm::config::HostConfig;
use chadm::error::{Error, Result};
use clap::Parser;
use std::io::{self, Write};
use std::process::ExitCode;

fn run(cli: Cli) -> Result<bool> {
    let mut out = io::stdout().lock();
    let write_result = match cli.command {
        Command::Service { action } => {
            if cli.config.is_some() {
                return Err(Error::Resource(
                    "service helpers only read /etc/chadm/config.toml".into(),
                ));
            }
            use chadm::cli::ServiceAction;
            let (action, name) = match action {
                ServiceAction::Prepare { name } => ("prepare", name),
                ServiceAction::Run { name } => ("run", name),
                ServiceAction::Ready { name } => ("ready", name),
                ServiceAction::Cleanup { name } => ("cleanup", name),
            };
            chadm::lifecycle::service(action, &name)?;
            Ok(true)
        }
        Command::Version => writeln!(out, "chadm {}", env!("CARGO_PKG_VERSION")).map(|_| true),
        Command::Doctor { json } => {
            let config = HostConfig::load(cli.config.as_deref())?;
            let report = chadm::doctor::run(&config);
            if json {
                writeln!(out, "{}", report.json()).map_err(output_error)?;
            } else {
                for check in &report.checks {
                    writeln!(
                        out,
                        "{:<20} {:<5} {}",
                        check.name,
                        check.status.label(),
                        check.detail
                    )
                    .map_err(output_error)?;
                }
            }
            Ok(report.healthy())
        }
        Command::Inspect { name, json } => {
            let host = HostConfig::load(cli.config.as_deref())?;
            let report = chadm::inspect::inspect(&host, &name)?;
            writeln!(out, "{}", if json { report.json() } else { report.text() }).map(|_| true)
        }
        Command::Instance { action } => {
            let config = HostConfig::load(cli.config.as_deref())?;
            run_instance(&config, action, &mut out)?;
            Ok(true)
        }
        Command::Repo { action } => {
            let config = HostConfig::load(cli.config.as_deref())?;
            run_repo(&config, action, &mut out)?;
            Ok(true)
        }
        Command::Image { action } => {
            let config = HostConfig::load(cli.config.as_deref())?;
            run_image(&config, action, &mut out)?;
            Ok(true)
        }
        Command::Volume { action } => {
            let config = HostConfig::load(cli.config.as_deref())?;
            run_volume(&config, action, &mut out)?;
            Ok(true)
        }
        Command::Snapshot { action } => {
            let config = HostConfig::load(cli.config.as_deref())?;
            run_snapshot(&config, action, &mut out)?;
            Ok(true)
        }
    };
    let healthy = write_result.map_err(output_error)?;
    out.flush().map_err(output_error)?;
    Ok(healthy)
}

fn run_snapshot(host: &HostConfig, action: SnapshotAction, out: &mut impl Write) -> Result<()> {
    use chadm::checkpoint;
    match action {
        SnapshotAction::Create { name, instance } => {
            checkpoint::create(host, &name, &instance)?;
            writeln!(
                out,
                "disk snapshot {name:?} created for instance {instance:?}"
            )
            .map_err(output_error)?;
        }
        SnapshotAction::Restore { name } => {
            checkpoint::restore(host, &name)?;
            writeln!(out, "snapshot {name:?} disks restored; VM remains stopped")
                .map_err(output_error)?;
        }
        SnapshotAction::Delete { name, discard } => {
            checkpoint::delete(host, &name, discard)?;
            writeln!(out, "snapshot {name:?} deleted; live volumes retained")
                .map_err(output_error)?;
        }
        SnapshotAction::List { json } => {
            let snapshots = checkpoint::list(host)?;
            if json {
                writeln!(
                    out,
                    "[{}]",
                    snapshots
                        .iter()
                        .map(|s| s.json())
                        .collect::<Vec<_>>()
                        .join(",")
                )
                .map_err(output_error)?;
            } else {
                writeln!(
                    out,
                    "{:<24} {:<16} {:<8} {:<12} AVAILABLE",
                    "NAME", "INSTANCE", "KIND", "PHASE"
                )
                .map_err(output_error)?;
                for s in snapshots {
                    writeln!(
                        out,
                        "{:<24} {:<16} {:<8} {:<12} {}",
                        s.name,
                        s.instance,
                        if s.kind == checkpoint::Kind::Disk {
                            "disk"
                        } else {
                            "suspend"
                        },
                        s.phase.label(),
                        s.available
                    )
                    .map_err(output_error)?;
                }
            }
        }
        SnapshotAction::Show { name, json } => {
            let s = checkpoint::show(host, &name)?;
            if json {
                writeln!(out, "{}", s.json()).map_err(output_error)?;
            } else {
                writeln!(
                    out,
                    "Name: {}\nInstance: {}\nKind: {:?}\nPhase: {}\nAvailable: {}",
                    s.name,
                    s.instance,
                    s.kind,
                    s.phase.label(),
                    s.available
                )
                .map_err(output_error)?;
                for (volume, repo, backend) in s.volumes {
                    writeln!(out, "  Volume: {volume} (repo {repo}, {backend})")
                        .map_err(output_error)?;
                }
                if let Some(reason) = s.reason {
                    writeln!(out, "Reason: {reason}").map_err(output_error)?;
                }
            }
        }
    }
    Ok(())
}

fn run_instance(host: &HostConfig, action: InstanceAction, out: &mut impl Write) -> Result<()> {
    use chadm::instance::{self, BootConfig, CreateOptions};
    match action {
        InstanceAction::Suspend { name } => {
            chadm::lifecycle::suspend(host, &name)?;
            writeln!(out, "instance {name:?} suspended to disk; VMM stopped")
                .map_err(output_error)?;
        }
        InstanceAction::Resume { name } => {
            chadm::lifecycle::resume(host, &name)?;
            writeln!(out, "instance {name:?} resumed").map_err(output_error)?;
        }
        InstanceAction::Recover { name } => {
            chadm::lifecycle::recover(host, &name)?;
            writeln!(out, "instance {name:?} runtime and TAP cleanup confirmed")
                .map_err(output_error)?;
        }
        InstanceAction::Console { name } => chadm::console::connect(host, &name)?,
        InstanceAction::Start { name } => {
            chadm::lifecycle::start(host, &name)?;
            writeln!(out, "instance {name:?} started").map_err(output_error)?;
        }
        InstanceAction::Stop { name, force } => {
            chadm::lifecycle::stop(host, &name, force)?;
            writeln!(out, "instance {name:?} stopped").map_err(output_error)?;
        }
        InstanceAction::Reboot { name, force } => {
            chadm::lifecycle::reboot(host, &name, force)?;
            writeln!(out, "instance {name:?} rebooted").map_err(output_error)?;
        }
        InstanceAction::Create(args) => {
            let boot = match args.boot {
                BootMode::Firmware => {
                    if args.kernel.is_some() || args.initramfs.is_some() || args.cmdline.is_some() {
                        return Err(Error::Resource(
                            "kernel/initramfs/cmdline require --boot direct".into(),
                        ));
                    }
                    BootConfig::Firmware { firmware: None }
                }
                BootMode::Direct => BootConfig::Direct {
                    kernel: args
                        .kernel
                        .ok_or_else(|| Error::Resource("direct boot requires --kernel".into()))?,
                    initramfs: args.initramfs,
                    cmdline: args
                        .cmdline
                        .ok_or_else(|| Error::Resource("direct boot requires --cmdline".into()))?,
                },
            };
            let created = instance::create(
                host,
                &CreateOptions {
                    name: args.name,
                    image: args.image,
                    repo: args.repo,
                    cloud_init: args.cloud_init,
                    no_cloud_init: args.no_cloud_init,
                    cpus: args.cpus,
                    ram: args.ram,
                    boot,
                    volumes: args.volumes,
                    networks: args.networks,
                },
            )?;
            writeln!(
                out,
                "instance {:?} metadata created; {} independent volumes prepared; VM not started",
                created.name,
                created.disk.len()
            )
            .map_err(output_error)?;
        }
        InstanceAction::Delete { name } => {
            instance::delete(host, &name)?;
            writeln!(out, "instance {name:?} metadata deleted; volumes retained")
                .map_err(output_error)?;
        }
        InstanceAction::List { json } => {
            let instances = instance::list(host)?;
            if json {
                writeln!(
                    out,
                    "[{}]",
                    instances
                        .iter()
                        .map(|instance| instance.json())
                        .collect::<Vec<_>>()
                        .join(",")
                )
                .map_err(output_error)?;
            } else {
                writeln!(
                    out,
                    "{:<16} {:<10} {:<5} {:<10} {:<16} STORAGE",
                    "NAME", "STATE", "CPU", "RAM", "IMAGE"
                )
                .map_err(output_error)?;
                for instance in instances {
                    writeln!(
                        out,
                        "{:<16} {:<10} {:<5} {:<10} {:<16} {}",
                        instance.config.name,
                        instance.state,
                        instance.config.compute.cpus,
                        instance.config.compute.memory,
                        instance.config.image,
                        if instance.available {
                            "available"
                        } else {
                            "unavailable"
                        }
                    )
                    .map_err(output_error)?;
                }
                writeln!(
                    out,
                    "Unregistered instances use metadata-only state; registered instances query systemd/API"
                )
                .map_err(output_error)?;
            }
        }
        InstanceAction::Show { name, json } => {
            let info = instance::show(host, &name)?;
            if json {
                writeln!(out, "{}", info.json()).map_err(output_error)?;
            } else {
                writeln!(out,"Name: {}\nState: {} ({})\nImage: {}\nAvailability: {}\n\nCompute:\n  CPUs: {}\n  RAM: {}\n\nBoot:\n  Mode: {}",info.config.name,info.state,info.state_source,info.config.image,if info.available { "available" } else { "unavailable" },info.config.compute.cpus,chadm::volume::display_size(chadm::volume::parse_size(&info.config.compute.memory)?),info.config.boot.mode()).map_err(output_error)?;
                match &info.config.boot {
                    BootConfig::Firmware { .. } => {
                        writeln!(out, "  Firmware: {}", host.firmware_path().display())
                            .map_err(output_error)?;
                    }
                    BootConfig::Direct {
                        kernel, initramfs, ..
                    } => {
                        writeln!(
                            out,
                            "  Kernel: {}\n  Initramfs: {}\n  Cmdline: (configured)",
                            kernel.display(),
                            initramfs
                                .as_ref()
                                .map(|path| path.display().to_string())
                                .unwrap_or_else(|| "(none)".into())
                        )
                        .map_err(output_error)?;
                    }
                }
                if info.config.windows {
                    writeln!(
                        out,
                        "\nWindows CPU profile:\n  kvm_hyperv: on\n  max_phys_bits: 39"
                    )
                    .map_err(output_error)?;
                }
                writeln!(out, "\nStorage:").map_err(output_error)?;
                for disk in &info.config.disk {
                    let resolved = info
                        .disks
                        .iter()
                        .find(|volume| volume.config.id == disk.volume_id);
                    writeln!(
                        out,
                        "  {}: {}\n    Path: {}",
                        disk.name,
                        disk.volume,
                        resolved
                            .and_then(|volume| volume.path.as_ref())
                            .map(|path| path.display().to_string())
                            .unwrap_or_else(|| "(unresolved)".into())
                    )
                    .map_err(output_error)?;
                }
                if let Some(seed) = &info.config.cloud_init {
                    writeln!(
                        out,
                        "\nCloud-init:\n  Repo: cloudinit\n  Seed: {}.raw\n  Readonly: true",
                        seed.image
                    )
                    .map_err(output_error)?;
                }
                writeln!(out, "\nNetwork:").map_err(output_error)?;
                if info.config.network.is_empty() {
                    writeln!(out, "  (none)").map_err(output_error)?;
                }
                for (index, net) in info.config.network.iter().enumerate() {
                    let ip = &info.ips[index];
                    let text = if ip.status == "not_applicable" {
                        "N/A (stopped)".into()
                    } else if ip.addresses.is_empty() {
                        ip.status.into()
                    } else {
                        ip.addresses
                            .iter()
                            .map(|address| {
                                format!(
                                    "{} (observed, neighbor: {})",
                                    address.address, address.neighbor_state
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    writeln!(out,"  net{index}\n    Bridge: {}\n    TAP: {}\n    MAC: {}\n    Mode: dhcp\n    IP: {}",net.bridge,net.tap,net.mac,text).map_err(output_error)?;
                    if let Some(reason) = &ip.reason {
                        writeln!(out, "    IP reason: {reason}").map_err(output_error)?;
                    }
                }
                writeln!(
                    out,
                    "\nRuntime:\n  Systemd unit: {}\n  API socket: {}\n  Console socket: {}",
                    info.unit,
                    info.api_socket.display(),
                    info.console_socket.display()
                )
                .map_err(output_error)?;
                if let Some(reason) = info.reason {
                    writeln!(out, "Reason: {reason}").map_err(output_error)?;
                }
            }
        }
    }
    Ok(())
}

fn run_volume(host: &HostConfig, action: VolumeAction, out: &mut impl Write) -> Result<()> {
    use chadm::volume;
    match action {
        VolumeAction::Attach { name, instance } => {
            volume::attach(host, &name, &instance)?;
            writeln!(
                out,
                "volume {name:?} attached to instance {instance:?}; configuration persisted"
            )
            .map_err(output_error)?;
        }
        VolumeAction::Detach { name, instance } => {
            volume::detach(host, &name, &instance)?;
            writeln!(
                out,
                "volume {name:?} detached from instance {instance:?}; volume retained"
            )
            .map_err(output_error)?;
        }

        VolumeAction::Create { name, repo, size } => {
            let created = volume::create(host, &name, &repo, size)?;
            writeln!(
                out,
                "volume {:?} created in repo {:?} ({})",
                created.name,
                created.repo,
                volume::display_size(created.size_bytes)
            )
            .map_err(output_error)?;
        }
        VolumeAction::Delete { name } => {
            volume::delete(host, &name)?;
            writeln!(out, "volume {name:?} deleted").map_err(output_error)?;
        }
        VolumeAction::List { json } => {
            let volumes = volume::list(host)?;
            if json {
                writeln!(
                    out,
                    "[{}]",
                    volumes
                        .iter()
                        .map(|volume| volume.json())
                        .collect::<Vec<_>>()
                        .join(",")
                )
                .map_err(output_error)?;
            } else {
                writeln!(out, "{:<16} {:<16} {:<16} STATE", "NAME", "REPO", "SIZE")
                    .map_err(output_error)?;
                for volume in volumes {
                    writeln!(
                        out,
                        "{:<16} {:<16} {:<16} {}",
                        volume.config.name,
                        volume.config.repo,
                        volume::display_size(volume.config.size_bytes),
                        if volume.available {
                            "available"
                        } else {
                            "unavailable"
                        }
                    )
                    .map_err(output_error)?;
                }
            }
        }
        VolumeAction::Show { name, json } => {
            let volume = volume::show(host, &name)?;
            if json {
                writeln!(out, "{}", volume.json()).map_err(output_error)?;
            } else {
                writeln!(
                    out,
                    "Name: {}\nRepo: {}\nSize: {} ({} bytes)\nPath: {}\nID: {}\nState: {}",
                    volume.config.name,
                    volume.config.repo,
                    volume::display_size(volume.config.size_bytes),
                    volume.config.size_bytes,
                    volume
                        .path
                        .as_ref()
                        .map(|path| path.display().to_string())
                        .unwrap_or_else(|| "(unresolved)".into()),
                    volume.config.id,
                    if volume.available {
                        "available"
                    } else {
                        "unavailable"
                    }
                )
                .map_err(output_error)?;
                if let Some(allocated) = volume.allocated_bytes {
                    writeln!(out, "Allocated: {} bytes", allocated).map_err(output_error)?;
                }
                if let Some(guid) = volume.config.zfs_guid {
                    writeln!(out, "ZFS GUID: {guid}").map_err(output_error)?;
                }
                if let Some(reason) = volume.reason {
                    writeln!(out, "Reason: {reason}").map_err(output_error)?;
                }
            }
        }
    }
    Ok(())
}

fn run_image(host: &HostConfig, action: ImageAction, out: &mut impl Write) -> Result<()> {
    use chadm::{image, volume};
    match action {
        ImageAction::Create {
            name,
            repo,
            path,
            windows,
        } => {
            let created = image::create_with_windows(host, &name, &repo, &path, windows)?;
            writeln!(
                out,
                "image {:?} imported in repo {:?} (raw, {})",
                created.name,
                created.repo,
                volume::display_size(created.virtual_size_bytes)
            )
            .map_err(output_error)?;
            if created.windows {
                writeln!(
                    out,
                    "Windows profile: kvm_hyperv=on, max_phys_bits=39; firmware boot required"
                )
                .map_err(output_error)?;
            }
        }
        ImageAction::Delete { name } => {
            image::delete(host, &name)?;
            writeln!(out, "image {name:?} deleted").map_err(output_error)?;
        }
        ImageAction::List { json } => {
            let images = image::list(host)?;
            if json {
                writeln!(
                    out,
                    "[{}]",
                    images
                        .iter()
                        .map(|image| image.json())
                        .collect::<Vec<_>>()
                        .join(",")
                )
                .map_err(output_error)?;
            } else {
                writeln!(
                    out,
                    "{:<16} {:<16} {:<16} {:<8} {:<8} STATE",
                    "NAME", "REPO", "SIZE", "FORMAT", "PROFILE"
                )
                .map_err(output_error)?;
                for image in images {
                    writeln!(
                        out,
                        "{:<16} {:<16} {:<16} {:<8} {:<8} {}",
                        image.config.name,
                        image.config.repo,
                        volume::display_size(image.config.virtual_size_bytes),
                        "raw",
                        if image.config.windows {
                            "windows"
                        } else {
                            "generic"
                        },
                        if image.available {
                            "available"
                        } else {
                            "unavailable"
                        }
                    )
                    .map_err(output_error)?;
                }
            }
        }
        ImageAction::Show { name, json } => {
            let image = image::show(host, &name)?;
            if json {
                writeln!(out, "{}", image.json()).map_err(output_error)?;
            } else {
                writeln!(
                    out,
                    "Profile: {}",
                    if image.config.windows {
                        "windows"
                    } else {
                        "generic"
                    }
                )
                .map_err(output_error)?;
                writeln!(out, "Name: {}\nRepo: {}\nFormat: raw\nSource format: {}\nVirtual size: {} ({} bytes)\nPath: {}\nID: {}\nState: {}",
                    image.config.name, image.config.repo, image.config.source_format.label(),
                    volume::display_size(image.config.virtual_size_bytes), image.config.virtual_size_bytes,
                    image.path.as_ref().map(|path| path.display().to_string()).unwrap_or_else(|| "(unresolved)".into()),
                    image.config.id, if image.available { "available" } else { "unavailable" }).map_err(output_error)?;
                if let Some(allocated) = image.allocated_bytes {
                    writeln!(out, "Allocated: {allocated} bytes").map_err(output_error)?;
                }
                if let Some(reason) = image.reason {
                    writeln!(out, "Reason: {reason}").map_err(output_error)?;
                }
            }
        }
    }
    Ok(())
}

fn run_repo(config: &HostConfig, action: RepoAction, out: &mut impl Write) -> Result<()> {
    match action {
        RepoAction::Create {
            name,
            driver,
            kind,
            path,
            pool,
        } => {
            if let Some(kind) = kind {
                if (kind == chadm::repo::RepoKind::Cloudinit) != (name == "cloudinit") {
                    return Err(Error::Resource(
                        "cloud-init repo must be named cloudinit; that name is reserved".into(),
                    ));
                }
            }
            let repo = match driver {
                RepoDriver::File => chadm::repo::create(
                    config,
                    &name,
                    &path.ok_or_else(|| Error::Resource("file repo requires --path".into()))?,
                )?,
                RepoDriver::Zfs => chadm::repo::create_zfs(
                    config,
                    &name,
                    &pool.ok_or_else(|| Error::Resource("ZFS repo requires --pool".into()))?,
                )?,
            };
            writeln!(out, "repo {:?} created at {:?}", repo.name, repo.path)
                .map_err(output_error)?;
        }
        RepoAction::Delete { name } => {
            let driver = chadm::repo::show(config, &name)?.config.driver;
            chadm::repo::delete(config, &name)?;
            writeln!(
                out,
                "repo {name:?} deleted; {}",
                if driver == chadm::repo::Driver::File {
                    "storage root retained"
                } else {
                    "managed datasets removed; zpool retained"
                }
            )
            .map_err(output_error)?;
        }
        RepoAction::List { json } => {
            let repos = chadm::repo::list(config)?;
            if json {
                writeln!(
                    out,
                    "[{}]",
                    repos
                        .iter()
                        .map(|repo| repo.json())
                        .collect::<Vec<_>>()
                        .join(",")
                )
                .map_err(output_error)?;
            } else {
                writeln!(out, "{:<16} {:<8} {:<12} PATH", "NAME", "DRIVER", "STATE")
                    .map_err(output_error)?;
                for repo in repos {
                    writeln!(
                        out,
                        "{:<16} {:<8} {:<12} {}",
                        repo.config.name,
                        repo.config.driver.label(),
                        if repo.available {
                            "available"
                        } else {
                            "unavailable"
                        },
                        repo.config.path.display()
                    )
                    .map_err(output_error)?;
                }
            }
        }
        RepoAction::Show { name, json } => {
            let repo = chadm::repo::show(config, &name)?;
            if json {
                writeln!(out, "{}", repo.json()).map_err(output_error)?;
            } else {
                writeln!(
                    out,
                    "Name: {}\nDriver: {}\nKind: {}\nPath: {}\nID: {}\nState: {}",
                    repo.config.name,
                    repo.config.driver.label(),
                    repo.config.kind.label(),
                    repo.config.path.display(),
                    repo.config.id,
                    if repo.available {
                        "available"
                    } else {
                        "unavailable"
                    }
                )
                .map_err(output_error)?;
                if let Some(pool) = &repo.config.pool {
                    writeln!(out, "Dataset: {pool}").map_err(output_error)?;
                }
                if let Some(reason) = repo.reason {
                    writeln!(out, "Reason: {reason}").map_err(output_error)?;
                }
            }
        }
    }
    Ok(())
}

fn output_error(source: io::Error) -> Error {
    Error::Io {
        action: "write",
        path: "stdout".into(),
        source,
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "chadm: {error}");
            ExitCode::FAILURE
        }
    }
}
