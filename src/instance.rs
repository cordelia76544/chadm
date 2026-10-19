//! Instance metadata and offline storage preparation. No VMM/network lifecycle yet.
use crate::config::HostConfig;
use crate::error::{Error, Result};
use crate::instance_config::{check_boot_files, memory_string, new_mac, tap_name, validate};
pub use crate::instance_config::{
    parse_mac, parse_network, parse_volume, BootConfig, CloudInitConfig, ComputeConfig,
    CreateOptions, DiskConfig, InstanceConfig, NetworkConfig, NetworkMode, NetworkSpec, VolumeSpec,
};
use crate::metadata::{self, directory, io_error, Lock};
use crate::output::json_string;
use crate::{image, repo, volume};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub struct InstanceInfo {
    pub config: InstanceConfig,
    pub state: &'static str,
    pub state_source: &'static str,
    pub ips: Vec<crate::network::IpObservation>,
    pub systemd_managed: bool,
    pub available: bool,
    pub reason: Option<String>,
    pub disks: Vec<volume::VolumeInfo>,
    pub unit: String,
    pub api_socket: PathBuf,
    pub console_socket: PathBuf,
    pub firmware: Option<PathBuf>,
}
fn serialize(config: &InstanceConfig) -> Result<String> {
    let text = toml::to_string_pretty(config)
        .map_err(|error| Error::Resource(format!("cannot serialize instance: {error}")))?;
    if text.len() > 65536 {
        return Err(Error::Resource(
            "instance configuration exceeds 64 KiB".into(),
        ));
    }
    Ok(text)
}
pub(crate) fn load(host: &HostConfig, name: &str) -> Result<InstanceConfig> {
    metadata::validate_name("instance", name)?;
    let dir = host.layout().instances.join(name);
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Resource(format!("instance {name:?} was not found")))
        }
        Err(error) => return Err(io_error("inspect instance", &dir, error)),
        Ok(_) => (),
    }
    directory(&dir, false)?;
    let path = dir.join("config.toml");
    let config: InstanceConfig =
        toml::from_str(&metadata::read(&path)?).map_err(|source| Error::Toml { path, source })?;
    validate(&config)?;
    if config.name != name {
        return Err(Error::Resource(
            "instance metadata name does not match its directory".into(),
        ));
    }
    Ok(config)
}
/// Caller holds the data-directory lock. Malformed or unfinished references fail closed.
pub(crate) fn configs_unlocked(host: &HostConfig) -> Result<Vec<InstanceConfig>> {
    let dir = host.layout().instances;
    match fs::symlink_metadata(&dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(io_error("inspect instances", &dir, error)),
        Ok(_) => (),
    }
    directory(&dir, false)?;
    let mut configs = Vec::new();
    let mut volume_names = HashSet::new();
    let mut volume_ids = HashSet::new();
    let mut macs = HashSet::new();
    for entry in fs::read_dir(&dir).map_err(|error| io_error("list instances", &dir, error))? {
        let entry = entry.map_err(|error| io_error("list instances", &dir, error))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| Error::Resource("instance name is not UTF-8".into()))?;
        if name.starts_with('.') {
            return Err(Error::Resource(format!(
                "unfinished instance transaction {name:?}; inspect metadata before retrying"
            )));
        }
        let mut config = load(host, &name)?;
        crate::hotplug::reserve(host, &mut config)?;
        for disk in &config.disk {
            if !volume_names.insert(disk.volume.clone())
                || !volume_ids.insert(disk.volume_id.clone())
            {
                return Err(Error::Resource(
                    "a writable volume is referenced by multiple instances".into(),
                ));
            }
        }
        for net in &config.network {
            if !macs.insert(net.mac.clone()) {
                return Err(Error::Resource(format!(
                    "MAC {:?} is assigned to multiple managed interfaces",
                    net.mac
                )));
            }
        }
        configs.push(config);
    }
    configs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(configs)
}
fn absent(path: &Path, what: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("inspect resource path", path, error)),
        Ok(_) => Err(Error::Resource(format!(
            "{what} {path:?} already exists; refusing to overwrite"
        ))),
    }
}
/// Phase 5 creates no managed runtime. Any artifact is evidence requiring review.
pub(crate) fn offline_runtime_state(host: &HostConfig, name: &str) -> Result<&'static str> {
    match fs::symlink_metadata(&host.runtime_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok("stopped"),
        Err(error) => {
            return Err(io_error(
                "inspect runtime directory",
                &host.runtime_dir,
                error,
            ))
        }
        Ok(_) => directory(&host.runtime_dir, false)?,
    }
    match fs::symlink_metadata(host.runtime_dir.join(name)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("stopped"),
        Err(error) => Err(io_error(
            "inspect instance runtime",
            &host.runtime_dir.join(name),
            error,
        )),
        Ok(_) => Ok("unknown"),
    }
}
pub(crate) fn runtime_state(host: &HostConfig, name: &str) -> Result<&'static str> {
    let config = load(host, name)?;
    let state = crate::lifecycle::state(host, &config);
    if let Some(reason) = state.reason {
        return Err(Error::Resource(reason));
    }
    Ok(state.name)
}
pub(crate) fn info(host: &HostConfig, config: InstanceConfig) -> InstanceInfo {
    let mut disks = Vec::new();
    let mut errors = Vec::new();
    let runtime_state = crate::lifecycle::state(host, &config);
    let state = runtime_state.name;
    if let Some(reason) = runtime_state.reason {
        errors.push(reason);
    }
    match crate::hotplug::pending(host, &config) {
        Ok(Some(_)) => errors.push("unfinished disk hotplug; volume remains reserved; retry the same command, or stop and run instance recover".into()),
        Ok(None) => (),
        Err(e) => errors.push(e.to_string()),
    }
    if runtime_state.source == "metadata_only" && state != "stopped" {
        errors.push("unregistered instance has unknown runtime artifacts".into());
    }
    match image::checked_unlocked(host, &config.image) {
        Ok(source) if source.config.id == config.image_id => (),
        Ok(_) => errors.push("instance source image generation changed".into()),
        Err(error) => errors.push(error.to_string()),
    }
    if let Some(seed) = &config.cloud_init {
        if let Err(error) = crate::cloud_init::resolve(host, seed) {
            errors.push(error.to_string());
        }
    }
    if let Err(error) = check_boot_files(&config.boot, &host.firmware_path(), false) {
        errors.push(error.to_string());
    }
    for disk in &config.disk {
        match volume::load(host, &disk.volume) {
            Err(error) => errors.push(error.to_string()),
            Ok(resolved) => {
                if resolved.id != disk.volume_id {
                    errors.push(format!(
                        "volume {:?} generation does not match instance reference",
                        disk.volume
                    ));
                }
                if disk.name == "root"
                    && (resolved.source_image_id.as_deref() != Some(config.image_id.as_str())
                        || resolved.source_image.as_deref() != Some(config.image.as_str()))
                {
                    errors.push("root volume source does not match instance image".into());
                }
                let item = volume::info(host, resolved);
                if !item.available {
                    errors.push(format!(
                        "volume {:?} is unavailable: {}",
                        disk.volume,
                        item.reason.as_deref().unwrap_or("unknown reason")
                    ));
                }
                disks.push(item);
            }
        }
    }
    let reason = if errors.is_empty() {
        None
    } else {
        Some(errors.join("; "))
    };
    let runtime = host.runtime_dir.join(&config.name);
    let ips = config
        .network
        .iter()
        .enumerate()
        .map(|(index, _)| {
            if state == "running" {
                crate::network::observe(&config, index).unwrap_or_else(|e| {
                    crate::network::IpObservation {
                        status: "unavailable",
                        queried_at_unix: None,
                        addresses: Vec::new(),
                        reason: Some(e.to_string()),
                    }
                })
            } else {
                crate::network::IpObservation {
                    queried_at_unix: None,
                    status: if ["stopped", "failed"].contains(&state) {
                        "not_applicable"
                    } else {
                        "unknown"
                    },
                    addresses: Vec::new(),
                    reason: None,
                }
            }
        })
        .collect();
    let systemd_managed = crate::lifecycle::registered(host, &config).unwrap_or(true);
    InstanceInfo {
        unit: format!("chadm-instance@{}.service", config.name),
        api_socket: runtime.join("api.sock"),
        console_socket: runtime.join("console.sock"),
        firmware: matches!(config.boot, BootConfig::Firmware { .. }).then(|| host.firmware_path()),
        config,
        state,
        state_source: runtime_state.source,
        ips,
        systemd_managed,
        available: reason.is_none(),
        reason,
        disks,
    }
}
pub fn list(host: &HostConfig) -> Result<Vec<InstanceInfo>> {
    host.validate()?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Ok(Vec::new());
    };
    configs_unlocked(host)?
        .into_iter()
        .map(|config| load(host, &config.name).map(|actual| info(host, actual)))
        .collect()
}
pub fn show(host: &HostConfig, name: &str) -> Result<InstanceInfo> {
    host.validate()?;
    metadata::validate_name("instance", name)?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("instance {name:?} was not found")));
    };
    // Also reject conflicts with other instances in hand-edited configurations.
    configs_unlocked(host)?;
    Ok(info(host, load(host, name)?))
}

pub fn create(host: &HostConfig, options: &CreateOptions) -> Result<InstanceConfig> {
    host.validate()?;
    metadata::validate_name("instance", &options.name)?;
    metadata::validate_name("image", &options.image)?;
    volume::validate_size(options.ram)?;
    let Some(read_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!(
            "image {:?} was not found",
            options.image
        )));
    };
    drop(read_lock);
    let _lock = Lock::acquire(&host.data_dir, true)?;
    let instances = configs_unlocked(host)?;
    if instances.iter().any(|config| config.name == options.name) {
        return Err(Error::Resource(format!(
            "instance {:?} already exists",
            options.name
        )));
    }
    let _operation = crate::lifecycle::operation_lock(host, &options.name)?;
    if offline_runtime_state(host, &options.name)? != "stopped" {
        return Err(Error::Resource(
            "instance runtime exists; inspect it before creating metadata".into(),
        ));
    }
    let source = image::checked_unlocked(host, &options.image)?;
    if source.config.windows && !matches!(options.boot, BootConfig::Firmware { .. }) {
        return Err(Error::Resource(
            "Windows images require --boot firmware; direct boot is not supported".into(),
        ));
    }
    if source.config.repo == crate::cloud_init::REPO {
        return Err(Error::Resource(
            "cloudinit seed cannot be used as a VM base image".into(),
        ));
    }
    let cloud_init =
        crate::cloud_init::select(host, options.cloud_init.as_deref(), options.no_cloud_init)?;
    let repo_name = options.repo.as_deref().unwrap_or(&source.config.repo);
    repo::validate_name(repo_name)?;
    let target_repo = repo::load(host, repo_name)?;
    repo::verify(&target_repo)?;
    let existing = volume::configs_unlocked(host)?;
    let mut specs = Vec::new();
    let mut aliases = HashSet::new();
    for spec in &options.volumes {
        metadata::validate_name("disk", &spec.name)?;
        volume::validate_size(spec.size_bytes)?;
        if !aliases.insert(spec.name.clone()) {
            return Err(Error::Resource(format!("duplicate disk {:?}", spec.name)));
        }
    }
    let root_size = options
        .volumes
        .iter()
        .find(|spec| spec.name == "root")
        .map(|spec| spec.size_bytes)
        .map(Ok)
        .unwrap_or_else(|| {
            if target_repo.driver == repo::Driver::Zfs {
                crate::storage::zfs::aligned_size(source.config.virtual_size_bytes)
            } else {
                Ok(source.config.virtual_size_bytes)
            }
        })?;
    if root_size < source.config.virtual_size_bytes {
        return Err(Error::Resource(
            "root volume size must not be smaller than image virtual size".into(),
        ));
    }
    specs.push(VolumeSpec {
        name: "root".into(),
        size_bytes: root_size,
    });
    specs.extend(
        options
            .volumes
            .iter()
            .filter(|spec| spec.name != "root")
            .cloned(),
    );
    let mut disk = Vec::new();
    for spec in &specs {
        let name = format!("{}-{}", options.name, spec.name);
        metadata::validate_name("volume", &name)?;
        if existing.iter().any(|volume| volume.name == name) {
            return Err(Error::Resource(format!("volume {name:?} already exists")));
        }
        if target_repo.driver == repo::Driver::Zfs {
            crate::storage::zfs::ensure_volume_absent(&target_repo, &name)?;
        }
        for path in [
            target_repo.path.join("volumes").join(format!("{name}.raw")),
            target_repo
                .path
                .join("volumes")
                .join(format!("{name}.chadm.toml")),
        ] {
            absent(&path, "volume storage")?;
        }
        disk.push(DiskConfig {
            name: spec.name.clone(),
            volume: name,
            volume_id: metadata::random_id()?,
        });
    }
    let mut used: HashSet<String> = instances
        .iter()
        .flat_map(|instance| instance.network.iter().map(|net| net.mac.clone()))
        .collect();
    // Reserve explicit MACs before generating any, independent of NIC order.
    for spec in &options.networks {
        if let Some(mac) = &spec.mac {
            let mac = parse_mac(mac)?;
            if !used.insert(mac.clone()) {
                return Err(Error::Resource(format!(
                    "MAC {mac:?} is already assigned to a managed interface"
                )));
            }
        }
    }
    let mut network = Vec::new();
    for (index, spec) in options.networks.iter().enumerate() {
        let mac = match &spec.mac {
            Some(mac) => parse_mac(mac)?,
            None => new_mac(&used)?,
        };
        used.insert(mac.clone());
        network.push(NetworkConfig {
            bridge: spec.bridge.clone(),
            tap: tap_name(&options.name, index)?,
            mac,
            mode: NetworkMode::Dhcp,
        });
    }
    let config = InstanceConfig {
        schema_version: 1,
        managed_by: "chadm".into(),
        id: metadata::random_id()?,
        name: options.name.clone(),
        image: source.config.name.clone(),
        image_id: source.config.id.clone(),
        windows: source.config.windows,
        cloud_init,
        compute: ComputeConfig {
            cpus: options.cpus,
            memory: memory_string(options.ram),
        },
        boot: options.boot.clone(),
        disk,
        network,
    };
    validate(&config)?;
    check_boot_files(&config.boot, &host.firmware_path(), false)?;
    let text = serialize(&config)?;
    let instances_dir = host.layout().instances;
    directory(&instances_dir, true)?;
    metadata::sync_directory(&host.data_dir)?;
    let stage = instances_dir.join(format!(".create-{}", config.id));
    metadata::create_directory(&stage)?;
    let intent = stage.join("intent.toml");
    let result = (|| {
        metadata::write_new(&intent, &text)?;
        metadata::sync_directory(&stage)?;
        metadata::sync_directory(&instances_dir)?;
        for (disk, spec) in config.disk.iter().zip(&specs) {
            let image = if disk.name == "root" {
                Some(&source.config)
            } else {
                None
            };
            volume::create_unlocked(
                host,
                &disk.volume,
                repo_name,
                spec.size_bytes,
                image,
                Some(&disk.volume_id),
            )?;
        }
        metadata::write_new(&stage.join("config.toml"), &text)?;
        fs::remove_file(&intent)
            .map_err(|error| io_error("remove completed instance intent", &intent, error))?;
        metadata::sync_directory(&stage)?;
        fs::rename(&stage, instances_dir.join(&config.name))
            .map_err(|error| io_error("publish instance metadata", &stage, error))?;
        Ok(())
    })();
    if let Err(error) = result {
        if let Err(rollback) = rollback_volumes(host, &config, &target_repo.path) {
            return Err(Error::Resource(format!("{error}; rollback incomplete: {rollback}; instance transaction retained at {stage:?}")));
        }
        for file in [stage.join("config.toml"), intent] {
            match fs::remove_file(&file) {
                Ok(()) => (),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => {
                    return Err(io_error("remove failed instance metadata", &file, error))
                }
            }
        }
        fs::remove_dir(&stage)
            .map_err(|error| io_error("remove failed instance transaction", &stage, error))?;
        metadata::sync_directory(&instances_dir)?;
        return Err(error);
    }
    metadata::sync_directory(&instances_dir)?;
    Ok(config)
}
fn rollback_volumes(host: &HostConfig, config: &InstanceConfig, repo_path: &Path) -> Result<()> {
    let target = repo::list_unlocked(host)?
        .into_iter()
        .find(|repo| repo.path == repo_path)
        .ok_or_else(|| Error::Resource("rollback repo was not found".into()))?;
    for disk in config.disk.iter().rev() {
        let dir = host.layout().volumes.join(&disk.volume);
        match fs::symlink_metadata(&dir) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if target.driver == repo::Driver::Zfs {
                    crate::storage::zfs::ensure_volume_absent(&target, &disk.volume)?;
                }
                absent(
                    &repo_path
                        .join("volumes")
                        .join(format!("{}.raw", disk.volume)),
                    "incomplete volume payload",
                )?;
                absent(
                    &repo_path
                        .join("volumes")
                        .join(format!("{}.chadm.toml", disk.volume)),
                    "incomplete volume marker",
                )?;
            }
            Err(error) => return Err(io_error("inspect rollback volume", &dir, error)),
            Ok(_) => {
                let volume = volume::load(host, &disk.volume)?;
                if volume.id != disk.volume_id {
                    return Err(Error::Resource(
                        "rollback volume ownership differs from instance intent".into(),
                    ));
                }
                volume::delete_unlocked(host, &disk.volume, Some(&volume))?;
            }
        }
    }
    // Backend failures may have left a volume transaction even without a payload.
    volume::configs_unlocked(host)?;
    Ok(())
}

pub(crate) fn edit_volume(
    host: &HostConfig,
    name: &str,
    volume_name: &str,
    attach: bool,
) -> Result<()> {
    host.validate()?;
    metadata::validate_name("instance", name)?;
    metadata::validate_name("volume", volume_name)?;
    let Some(read_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("instance {name:?} was not found")));
    };
    load(host, name)?;
    drop(read_lock);
    let _operation = crate::lifecycle::operation_lock(host, name)?;
    let _lock = Lock::acquire(&host.data_dir, true)?;
    let instances = configs_unlocked(host)?;
    volume::configs_unlocked(host)?;
    repo::list_unlocked(host)?;
    let mut config = load(host, name)?;
    check_edit_directory(host, &config)?;
    crate::checkpoint::ensure_config_editable(host, &config)?;
    let target = crate::lifecycle::hotplug_target(host, &config)?;
    let before = config.clone();
    let disk_volume = volume::load(host, volume_name)?;
    if let Some(after) = crate::hotplug::resume_config(host, &config, volume_name, attach)? {
        if !config
            .disk
            .iter()
            .chain(after.disk.iter())
            .any(|d| d.volume == volume_name && d.volume_id == disk_volume.id)
        {
            return Err(Error::Resource(
                "pending hotplug volume generation changed".into(),
            ));
        }
        let snapshot = target.as_ref().ok_or_else(|| {
            Error::Resource(
                "unfinished hotplug on stopped VM; run instance recover before editing volumes"
                    .into(),
            )
        })?;
        let info = volume::info(host, disk_volume);
        if !info.available {
            return Err(Error::Resource(
                "pending hotplug volume is unavailable".into(),
            ));
        }
        let path = info
            .path
            .ok_or_else(|| Error::Resource("volume path is unavailable".into()))?;
        return crate::hotplug::execute(
            host,
            &config,
            &after,
            volume_name,
            &path,
            attach,
            snapshot,
        );
    }
    if attach {
        for instance in &instances {
            if instance
                .disk
                .iter()
                .any(|d| d.volume == volume_name || d.volume_id == disk_volume.id)
            {
                return Err(Error::Resource(format!(
                    "volume {volume_name:?} is attached to instance {:?}",
                    instance.name
                )));
            }
        }
        let info = volume::info(host, disk_volume.clone());
        if !info.available {
            return Err(Error::Resource(format!(
                "volume {volume_name:?} is unavailable: {}",
                info.reason.as_deref().unwrap_or("unknown reason")
            )));
        }
        let mut alias = volume_name.to_string();
        if config.disk.iter().any(|d| d.name == alias) {
            alias = (0..32)
                .map(|i| format!("data{i}"))
                .find(|name| !config.disk.iter().any(|d| &d.name == name))
                .ok_or_else(|| Error::Resource("instance disk limit reached".into()))?;
        }
        config.disk.push(DiskConfig {
            name: alias,
            volume: volume_name.into(),
            volume_id: disk_volume.id,
        });
    } else {
        let index = config
            .disk
            .iter()
            .position(|d| d.volume == volume_name)
            .ok_or_else(|| {
                Error::Resource(format!(
                    "volume {volume_name:?} is not attached to instance {name:?}"
                ))
            })?;
        if config.disk[index].volume_id != disk_volume.id {
            return Err(Error::Resource(
                "attached volume generation changed; refusing detach".into(),
            ));
        }
        if index == 0 || config.disk[index].name == "root" {
            return Err(Error::Resource("root volume cannot be detached".into()));
        }
        config.disk.remove(index);
    }
    validate(&config)?;
    if let Some(snapshot) = target {
        let info = volume::info(host, volume::load(host, volume_name)?);
        if !info.available {
            return Err(Error::Resource("hotplug volume is unavailable".into()));
        }
        let path = info
            .path
            .ok_or_else(|| Error::Resource("volume path is unavailable".into()))?;
        return crate::hotplug::execute(
            host,
            &before,
            &config,
            volume_name,
            &path,
            attach,
            &snapshot,
        );
    }
    crate::lifecycle::check_cold_edit(host, &config)?;
    replace_config(host, &config)
}

pub(crate) fn check_edit_directory(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    let dir = host.layout().instances.join(&config.name);
    directory(&dir, false)?;
    let pending = crate::hotplug::pending(host, config)?.is_some();
    for entry in fs::read_dir(&dir).map_err(|e| io_error("inspect instance metadata", &dir, e))? {
        let entry = entry.map_err(|e| io_error("inspect instance metadata", &dir, e))?;
        let name = entry.file_name();
        if name != "config.toml" && name != "systemd.toml" && !(pending && name == "hotplug.toml") {
            return Err(Error::Resource("instance metadata contains unexpected entries or an interrupted update; inspect before retrying".into()));
        }
    }
    Ok(())
}
pub(crate) fn replace_config(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    check_edit_directory(host, config)?;
    let dir = host.layout().instances.join(&config.name);
    let path = dir.join("config.toml");
    metadata::open_regular(&path, false)?;
    let temporary = dir.join(format!(".config-{}.toml", metadata::random_id()?));
    metadata::write_new(&temporary, &serialize(config)?)?;
    let publish = (|| {
        metadata::sync_directory(&dir)?;
        fs::rename(&temporary, &path)
            .map_err(|e| io_error("publish instance disk configuration", &path, e))
    })();
    if let Err(original) = publish {
        return match fs::remove_file(&temporary) {
            Ok(()) => Err(original),
            Err(cleanup) => Err(Error::Resource(format!(
                "{original}; configuration rollback incomplete: {cleanup}; inspect {temporary:?}"
            ))),
        };
    }
    metadata::sync_directory(&dir).map_err(|e| Error::Resource(format!("instance configuration was published but directory sync failed: {e}; inspect before retrying")))
}

pub fn delete(host: &HostConfig, name: &str) -> Result<()> {
    host.validate()?;
    metadata::validate_name("instance", name)?;
    let Some(read_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(Error::Resource(format!("instance {name:?} was not found")));
    };
    drop(read_lock);
    let _lock = Lock::acquire(&host.data_dir, true)?;
    configs_unlocked(host)?;
    let _operation = crate::lifecycle::operation_lock(host, name)?;
    let config = load(host, name)?;
    crate::checkpoint::ensure_config_editable(host, &config)?;
    if !["stopped", "failed"].contains(&runtime_state(host, name)?)
        || offline_runtime_state(host, name)? != "stopped"
    {
        return Err(Error::Resource(format!(
            "instance {name:?} state is unknown; inspect runtime before deletion"
        )));
    }
    crate::network::ensure_absent(&config)?;
    let instances = host.layout().instances;
    let dir = instances.join(name);
    let entries: Vec<_> = fs::read_dir(&dir)
        .map_err(|error| io_error("inspect instance metadata", &dir, error))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .map_err(|error| io_error("inspect instance metadata", &dir, error))
        })
        .collect::<Result<_>>()?;
    let registered = crate::lifecycle::registered(host, &config)?;
    if entries.len() != if registered { 2 } else { 1 }
        || entries
            .iter()
            .any(|name| name != "config.toml" && !(registered && name == "systemd.toml"))
    {
        return Err(Error::Resource(
            "instance metadata contains unexpected entries; refusing deletion".into(),
        ));
    }
    let stage = instances.join(format!(".delete-{}", config.id));
    fs::rename(&dir, &stage).map_err(|error| io_error("stage instance deletion", &dir, error))?;
    metadata::sync_directory(&instances)?;
    let path = stage.join("config.toml");
    if let Err(error) = fs::remove_file(&path) {
        fs::rename(&stage, &dir).map_err(|restore| {
            Error::Resource(format!(
                "{error}; cannot restore instance metadata: {restore}; inspect {stage:?}"
            ))
        })?;
        metadata::sync_directory(&instances)?;
        return Err(io_error("remove instance metadata", &path, error));
    }
    if registered {
        fs::remove_file(stage.join("systemd.toml"))
            .map_err(|error| io_error("remove systemd registration", &stage, error))?;
    }
    fs::remove_dir(&stage)
        .map_err(|error| io_error("remove instance metadata directory", &stage, error))?;
    metadata::sync_directory(&instances)
}

impl InstanceInfo {
    pub fn json(&self) -> String {
        let disks = self
            .config
            .disk
            .iter()
            .map(|disk| {
                let resolved = self
                    .disks
                    .iter()
                    .find(|info| info.config.id == disk.volume_id);
                let path = resolved
                    .and_then(|info| info.path.as_ref())
                    .map(|path| json_string(&path.to_string_lossy()))
                    .unwrap_or_else(|| "null".into());
                format!(
                    "{{\"name\":{},\"volume\":{},\"volume_id\":{},\"path\":{path}}}",
                    json_string(&disk.name),
                    json_string(&disk.volume),
                    json_string(&disk.volume_id)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let networks = self.config.network.iter().enumerate().map(|(index, net)| {
            let observation = &self.ips[index];
            let addresses = observation.addresses.iter().map(|address| format!("{{\"address\":{},\"source\":{},\"neighbor_state\":{}}}", json_string(&address.address.to_string()), json_string(address.source), json_string(&address.neighbor_state))).collect::<Vec<_>>().join(",");
            format!("{{\"bridge\":{},\"tap\":{},\"mac\":{},\"mode\":\"dhcp\",\"ip_addresses\":[{}],\"ip_status\":{},\"ip_reason\":{},\"ip_queried_at_unix\":{}}}",json_string(&net.bridge),json_string(&net.tap),json_string(&net.mac),addresses,json_string(observation.status),observation.reason.as_ref().map(|reason|json_string(reason)).unwrap_or_else(||"null".into()),observation.queried_at_unix.map(|time|time.to_string()).unwrap_or_else(||"null".into()))
        }).collect::<Vec<_>>().join(",");
        let boot = match &self.config.boot {
            BootConfig::Firmware { .. } => format!("{{\"mode\":\"firmware\",\"firmware\":{}}}",self.firmware.as_ref().map(|path|json_string(&path.to_string_lossy())).unwrap_or_else(||"null".into())),
            BootConfig::Direct { kernel, initramfs, .. } => format!("{{\"mode\":\"direct\",\"kernel\":{},\"initramfs\":{},\"cmdline_configured\":true}}",json_string(&kernel.to_string_lossy()),initramfs.as_ref().map(|path|json_string(&path.to_string_lossy())).unwrap_or_else(||"null".into())),
        };
        let cloud_init = self
            .config
            .cloud_init
            .as_ref()
            .map(|seed| {
                format!(
                    "{{\"repo\":\"cloudinit\",\"image\":{},\"image_id\":{},\"readonly\":true}}",
                    json_string(&seed.image),
                    json_string(&seed.image_id)
                )
            })
            .unwrap_or_else(|| "null".into());
        format!("{{\"schema_version\":1,\"managed_by\":\"chadm\",\"id\":{},\"name\":{},\"image\":{},\"image_id\":{},\"windows\":{},\"compute\":{{\"cpus\":{},\"memory\":{},\"memory_bytes\":{}}},\"boot\":{boot},\"cloud_init\":{cloud_init},\"disk\":[{disks}],\"network\":[{networks}],\"state\":{},\"state_source\":{},\"lifecycle_supported\":true,\"available\":{},\"reason\":{},\"runtime\":{{\"planned\":{},\"systemd_unit\":{},\"api_socket\":{},\"console_socket\":{}}}}}",json_string(&self.config.id),json_string(&self.config.name),json_string(&self.config.image),json_string(&self.config.image_id),self.config.windows,self.config.compute.cpus,json_string(&self.config.compute.memory),volume::parse_size(&self.config.compute.memory).unwrap_or(0),json_string(self.state),json_string(self.state_source),self.available,self.reason.as_ref().map(|reason|json_string(reason)).unwrap_or_else(||"null".into()),!self.systemd_managed,json_string(&self.unit),json_string(&self.api_socket.to_string_lossy()),json_string(&self.console_socket.to_string_lossy()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    struct Fixture {
        root: PathBuf,
        host: HostConfig,
        storage: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "chadm-instance-rollback-{}",
                metadata::random_id().unwrap()
            ));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let host = HostConfig {
                data_dir: root.join("data"),
                runtime_dir: root.join("run"),
                ..Default::default()
            };
            let storage = root.join("storage");
            repo::create(&host, "local", &storage).unwrap();
            let input = root.join("input.raw");
            fs::write(&input, b"base content").unwrap();
            image::create(&host, "base", "local", &input).unwrap();
            Self {
                root,
                host,
                storage,
            }
        }
        fn stage(&self) -> InstanceConfig {
            let mut options = CreateOptions::new("test01", "base");
            options.volumes.push(parse_volume("data,size=1M").unwrap());
            let config = create(&self.host, &options).unwrap();
            fs::rename(
                self.host.layout().instances.join("test01"),
                self.host
                    .layout()
                    .instances
                    .join(format!(".create-{}", config.id)),
            )
            .unwrap();
            config
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn abort_after_disk_creation_rolls_back_only_owned_volumes() {
        let f = Fixture::new();
        let config = f.stage();
        let _lock = Lock::acquire(&f.host.data_dir, true).unwrap();
        rollback_volumes(&f.host, &config, &f.storage).unwrap();
        assert!(volume::configs_unlocked(&f.host).unwrap().is_empty());
        assert!(f.storage.join("images/base.raw").exists());
        assert!(f
            .host
            .layout()
            .instances
            .join(format!(".create-{}", config.id))
            .exists());
    }
    #[test]
    fn rollback_keeps_intent_and_payloads_when_ownership_changes() {
        let f = Fixture::new();
        let mut config = f.stage();
        config.disk[1].volume_id = "0".repeat(32);
        let _lock = Lock::acquire(&f.host.data_dir, true).unwrap();
        assert!(rollback_volumes(&f.host, &config, &f.storage)
            .unwrap_err()
            .to_string()
            .contains("ownership"));
        for disk in &config.disk {
            assert!(f
                .storage
                .join("volumes")
                .join(format!("{}.raw", disk.volume))
                .exists());
        }
    }
}
