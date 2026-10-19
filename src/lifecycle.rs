//! Bounded CLI/helper orchestration. No daemon; the service execs the VMM.
use crate::{
    config::{HostConfig, DEFAULT_CONFIG},
    error::{Error, Result},
    hypervisor::{self, Api, Http},
    instance::{self, InstanceConfig},
    metadata::{self, Lock},
    network,
    systemd::{self, Manager, Snapshot, System},
};
use serde::{Deserialize, Serialize};
use std::os::unix::process::CommandExt;
use std::{
    fs,
    process::Command,
    time::{Duration, Instant},
};

fn error(s: impl Into<String>) -> Error {
    Error::Resource(s.into())
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registration {
    managed_by: String,
    name: String,
    instance_id: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Runtime {
    managed_by: String,
    name: String,
    instance_id: String,
    invocation_id: String,
}
fn registration_path(host: &HostConfig, name: &str) -> std::path::PathBuf {
    host.layout().instances.join(name).join("systemd.toml")
}
pub(crate) fn registered(host: &HostConfig, config: &InstanceConfig) -> Result<bool> {
    let path = registration_path(host, &config.name);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(metadata::io_error("inspect systemd registration", &path, e)),
        Ok(_) => (),
    }
    let r: Registration = toml::from_str(&metadata::read(&path)?)
        .map_err(|_| error("invalid systemd registration TOML"))?;
    if r.managed_by != "chadm" || r.name != config.name || r.instance_id != config.id {
        return Err(error("instance systemd registration ownership differs"));
    }
    Ok(true)
}
fn register(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    if !registered(host, config)? {
        let text = toml::to_string(&Registration {
            managed_by: "chadm".into(),
            name: config.name.clone(),
            instance_id: config.id.clone(),
        })
        .map_err(|e| error(e.to_string()))?;
        let path = registration_path(host, &config.name);
        metadata::write_new(&path, &text)?;
        metadata::sync_directory(
            path.parent()
                .ok_or_else(|| error("invalid registration path"))?,
        )?;
    }
    Ok(())
}
/// CLI operations share this lock with metadata create/delete, but helpers use only
/// the global metadata lock. Never hold the global lock while waiting for a systemd job.
pub(crate) fn operation_lock(host: &HostConfig, name: &str) -> Result<Option<Lock>> {
    metadata::validate_name("instance", name)?;
    Lock::acquire(&host.data_dir.join("operations").join(name), true)
}
fn global(host: &HostConfig) -> Result<Option<Lock>> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match Lock::acquire(&host.data_dir, true) {
            Err(Error::Resource(message))
                if message.contains("another chadm command") && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(50))
            }
            result => return result,
        }
    }
}
fn service_config() -> Result<HostConfig> {
    let path = std::path::Path::new(DEFAULT_CONFIG);
    match fs::symlink_metadata(path) {
        Ok(_) => {
            metadata::directory(
                path.parent()
                    .ok_or_else(|| error("invalid service config path"))?,
                false,
            )?;
            metadata::open_regular(path, false)?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(metadata::io_error("inspect service configuration", path, e)),
    }
    HostConfig::load(None)
}
fn configured(host: &HostConfig) -> Result<()> {
    let service = service_config()?;
    let same = host.data_dir == service.data_dir
        && host.runtime_dir == service.runtime_dir
        && host.cloud_hypervisor == service.cloud_hypervisor;
    if !same {
        return Err(error(format!("CLI configuration differs from service configuration; shared template reads {DEFAULT_CONFIG}; configure that file before starting instances")));
    }
    Ok(())
}
fn require_root() -> Result<()> {
    unsafe extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: Linux geteuid has no arguments or preconditions.
    if unsafe { geteuid() } != 0 {
        return Err(error("instance lifecycle operations require root"));
    }
    Ok(())
}

pub(crate) fn check_cold_edit(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    if registered(host, config)? {
        configured(host)?;
    }
    check_cold_edit_with(&System, host, config)
}
fn check_cold_edit_with(
    manager: &impl Manager,
    host: &HostConfig,
    config: &InstanceConfig,
) -> Result<()> {
    if registered(host, config)? {
        let snapshot = manager.snapshot(&config.name)?;
        manager.verify(&snapshot)?;
        if !systemd::terminal(&snapshot) {
            return Err(error(format!("instance {:?} is running or transitioning; this operation requires a stopped instance", config.name)));
        }
        manager.quiescent(&snapshot)?;
    }
    if instance::offline_runtime_state(host, &config.name)? != "stopped" {
        return Err(error("instance runtime is unknown; recover owned runtime before attaching or detaching volumes"));
    }
    network::ensure_absent(config)
}
fn runtime(host: &HostConfig, config: &InstanceConfig) -> Result<Runtime> {
    let dir = host.runtime_dir.join(&config.name);
    metadata::directory(&dir, false)?;
    let path = dir.join("owner.toml");
    let r: Runtime = toml::from_str(&metadata::read(&path)?)
        .map_err(|_| error("invalid runtime ownership TOML"))?;
    if r.managed_by != "chadm"
        || r.name != config.name
        || r.instance_id != config.id
        || !metadata::valid_id(&r.invocation_id)
    {
        return Err(error("runtime ownership does not match instance"));
    }
    Ok(r)
}
/// Caller holds the operation and metadata locks. None means a verified cold edit.
pub(crate) fn hotplug_target(
    host: &HostConfig,
    config: &InstanceConfig,
) -> Result<Option<Snapshot>> {
    if !registered(host, config)? {
        check_cold_edit(host, config)?;
        return Ok(None);
    }
    configured(host)?;
    let snapshot = System.snapshot(&config.name)?;
    System.verify(&snapshot)?;
    if systemd::terminal(&snapshot) {
        check_cold_edit(host, config)?;
        return Ok(None);
    }
    require_root()?;
    check_live_snapshot(&snapshot)?;
    if runtime(host, config)?.invocation_id != snapshot.invocation {
        return Err(error("hotplug runtime invocation differs from systemd"));
    }
    if hypervisor::vm_state(
        &host.runtime_dir.join(&config.name).join("api.sock"),
        snapshot.pid,
    )? != "running"
    {
        return Err(error("disk hotplug requires a running VM"));
    }
    same_invocation(&snapshot, &System.snapshot(&config.name)?)?;
    Ok(Some(snapshot))
}
fn check_live_snapshot(snapshot: &Snapshot) -> Result<()> {
    if snapshot.active != "active"
        || snapshot.sub != "running"
        || snapshot.pid == 0
        || snapshot.control_pid != 0
        || snapshot.job != 0
    {
        return Err(error(
            "instance is transitioning or has a pending systemd job; disk hotplug is unavailable",
        ));
    }
    Ok(())
}
pub(crate) fn recheck_hotplug(
    host: &HostConfig,
    config: &InstanceConfig,
    before: &Snapshot,
) -> Result<()> {
    let after = System.snapshot(&config.name)?;
    System.verify(&after)?;
    check_live_snapshot(&after)?;
    same_invocation(before, &after)?;
    if runtime(host, config)?.invocation_id != after.invocation {
        return Err(error("hotplug runtime ownership changed"));
    }
    Ok(())
}
fn same_invocation(a: &Snapshot, b: &Snapshot) -> Result<()> {
    if a.pid != b.pid || a.invocation != b.invocation {
        return Err(error(
            "systemd invocation changed during operation; retry after checking instance state",
        ));
    }
    Ok(())
}
pub(crate) struct State {
    pub name: &'static str,
    pub source: &'static str,
    pub reason: Option<String>,
}
fn state_with(
    manager: &impl Manager,
    api: &impl Api,
    host: &HostConfig,
    config: &InstanceConfig,
) -> Result<State> {
    if !registered(host, config)? {
        // Legacy/offline metadata has never been handed to systemd. Runtime artifacts
        // still fail closed. Do not call this a queried systemd state.
        return Ok(State {
            name: instance::offline_runtime_state(host, &config.name)?,
            source: "metadata_only",
            reason: None,
        });
    }
    let s = manager.snapshot(&config.name)?;
    let name = match s.active.as_str() {
        "inactive" if s.pid == 0 => "stopped",
        "failed" if s.pid == 0 => "failed",
        "activating" => "starting",
        "deactivating" => "stopping",
        "active" if s.pid > 0 => {
            let r = runtime(host, config)?;
            if r.invocation_id != s.invocation {
                return Err(error("runtime invocation differs from systemd"));
            }
            let state = api.state(&host.runtime_dir.join(&config.name).join("api.sock"), s.pid)?;
            same_invocation(&s, &manager.snapshot(&config.name)?)?;
            state
        }
        _ => {
            return Err(error(format!(
                "systemd state is inconclusive ({}/{}, MainPID={})",
                s.active, s.sub, s.pid
            )))
        }
    };
    Ok(State {
        name,
        source: if s.active == "active" {
            "systemd+api"
        } else {
            "systemd"
        },
        reason: if systemd::terminal(&s)
            && instance::offline_runtime_state(host, &config.name)? != "stopped"
        {
            Some("VMM is stopped but runtime artifacts remain; run instance recover to validate and clean owned resources".into())
        } else {
            None
        },
    })
}
pub(crate) fn state(host: &HostConfig, config: &InstanceConfig) -> State {
    let result: Result<State> = (|| {
        if registered(host, config)? {
            configured(host)?;
        }
        let mut result = state_with(&System, &Http, host, config)?;
        if ["stopped", "failed"].contains(&result.name) {
            if let Some(state) = crate::checkpoint::status(host, config)? {
                result.name = state;
                result.source = "checkpoint+systemd";
                if state != "suspended" {
                    result.reason = Some(
                        "unfinished checkpoint transaction; inspect snapshot list before starting"
                            .into(),
                    );
                }
            }
        }
        Ok(result)
    })();
    result.unwrap_or_else(|e| State {
        name: "unknown",
        source: "systemd",
        reason: Some(e.to_string()),
    })
}
fn loaded(host: &HostConfig, name: &str) -> Result<InstanceConfig> {
    host.validate()?;
    metadata::validate_name("instance", name)?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(error(format!("instance {name:?} was not found")));
    };
    instance::configs_unlocked(host)?;
    instance::load(host, name)
}
/// Authenticate a live serial connection; release metadata locks before relaying.
pub(crate) fn console_stream(
    host: &HostConfig,
    name: &str,
) -> Result<(std::os::unix::net::UnixStream, Option<Lock>)> {
    require_root()?;
    configured(host)?;
    let Some(_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(error(format!("instance {name:?} was not found")));
    };
    let config = instance::load(host, name)?;
    if !registered(host, &config)? {
        return Err(error(format!("instance {name:?} is not running")));
    }
    let before = System.snapshot(name)?;
    System.verify(&before)?;
    if before.active != "active" || before.pid == 0 {
        return Err(error(format!("instance {name:?} is not running")));
    }
    let owner = runtime(host, &config)?;
    if owner.invocation_id != before.invocation {
        return Err(error("console runtime invocation differs from systemd"));
    }
    let dir = host.runtime_dir.join(name);
    if !["running", "paused"].contains(&hypervisor::vm_state(&dir.join("api.sock"), before.pid)?) {
        return Err(error("VM is not ready for a console connection"));
    }
    let session =
        Lock::acquire(&host.data_dir.join("console-sessions").join(name), true).map_err(|e| {
            error(format!(
                "cannot acquire console session; another console may be connected: {e}"
            ))
        })?;
    let stream = hypervisor::connect(&dir.join("console.sock"), before.pid)?;
    same_invocation(&before, &System.snapshot(name)?)?;
    Ok((stream, session))
}
pub fn start(host: &HostConfig, name: &str) -> Result<()> {
    loaded(host, name)?;
    require_root()?;
    configured(host)?;
    let _operation = operation_lock(host, name)?;
    start_locked(host, name)
}
fn checkpoint_context(host: &HostConfig, config: &InstanceConfig) -> Result<Snapshot> {
    require_root()?;
    configured(host)?;
    if !registered(host, config)? {
        return Err(error("instance has no registered running VMM"));
    }
    let snapshot = System.snapshot(&config.name)?;
    System.verify(&snapshot)?;
    check_live_snapshot(&snapshot)?;
    if runtime(host, config)?.invocation_id != snapshot.invocation {
        return Err(error("checkpoint runtime invocation differs from systemd"));
    }
    Ok(snapshot)
}
fn checkpoint_version(host: &HostConfig, config: &InstanceConfig) -> Result<String> {
    let program = crate::command::executable(&host.cloud_hypervisor)
        .ok_or_else(|| error("cloud-hypervisor was not found"))?;
    let help = crate::command::network_output(&program, &["--help"]).map_err(error)?;
    if !help.contains("--restore") || !help.contains("memory_restore_mode=") {
        return Err(error(
            "installed Cloud Hypervisor does not advertise required restore/copy support",
        ));
    }
    use std::os::unix::fs::MetadataExt;
    let file = metadata::open_regular(&program, false)?;
    let stat = file.metadata().map_err(|e| error(e.to_string()))?;
    let version = crate::command::probe(&program, &["--version"]).map_err(error)?;
    let mut signature = format!(
        "{};binary={}:{}:{}:{}:{}",
        version.trim(),
        stat.dev(),
        stat.ino(),
        stat.len(),
        stat.mtime(),
        stat.mtime_nsec()
    );
    let artifacts = match &config.boot {
        instance::BootConfig::Firmware { .. } => vec![host.firmware_path()],
        instance::BootConfig::Direct {
            kernel, initramfs, ..
        } => std::iter::once(kernel.clone())
            .chain(initramfs.clone())
            .collect(),
    };
    for path in artifacts {
        let file = metadata::open_regular(&path, false)?;
        let m = file.metadata().map_err(|e| error(e.to_string()))?;
        signature.push_str(&format!(
            ";boot={}:{}:{}:{}:{}",
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec()
        ));
    }
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease")
        .map_err(|e| error(format!("cannot identify host kernel: {e}")))?;
    signature.push_str(&format!(";kernel={}", kernel.trim()));
    Ok(signature)
}
fn ensure_api_state(
    host: &HostConfig,
    config: &InstanceConfig,
    snapshot: &Snapshot,
    expected: &str,
) -> Result<()> {
    recheck_hotplug(host, config, snapshot)?;
    if hypervisor::vm_state(
        &host.runtime_dir.join(&config.name).join("api.sock"),
        snapshot.pid,
    )? != expected
    {
        return Err(error(format!("checkpoint requires VM state {expected}")));
    }
    recheck_hotplug(host, config, snapshot)
}
pub fn suspend(host: &HostConfig, name: &str) -> Result<()> {
    use crate::checkpoint::{self, Kind, Phase};
    let config = loaded(host, name)?;
    require_root()?;
    configured(host)?;
    let _operation = operation_lock(host, name)?;
    let lock = global(host)?;
    let config_now = instance::load(host, name)?;
    if config_now.id != config.id {
        return Err(error("instance generation changed"));
    }
    crate::hotplug::ensure_no_pending(host, &config_now)?;
    let mut record = if let Some(record) = checkpoint::suspend_record(host, &config_now)? {
        if record.phase != Phase::Saved {
            return Err(error("suspend checkpoint already exists or is incomplete; inspect snapshot list; use instance resume or explicit snapshot delete --discard"));
        }
        let snapshot = System.snapshot(name)?;
        System.verify(&snapshot)?;
        if !systemd::terminal(&snapshot) {
            if snapshot.invocation != record.invocation {
                return Err(error("suspend invocation changed"));
            }
            ensure_api_state(host, &config_now, &snapshot, "paused")?;
        }
        checkpoint::check_state_files(host, &record)?;
        record
    } else {
        checkpoint::ensure_can_capture(host, &config_now)?;
        let context = checkpoint_context(host, &config_now)?;
        ensure_api_state(host, &config_now, &context, "running")?;
        hypervisor::check_live_configuration(host, &config_now, context.pid)?;
        let version = checkpoint_version(host, &config_now)?;
        let snapshot_name = format!("suspend-{}", config_now.id);
        hypervisor::file_url(
            &host
                .data_dir
                .join("snapshots")
                .join(&snapshot_name)
                .join("state"),
        )?;
        let mut record = checkpoint::begin(
            host,
            &config_now,
            &snapshot_name,
            Kind::Suspend,
            &context.invocation,
            &version,
        )?;
        let state_path = checkpoint::state_path(host, &record);
        hypervisor::file_url(&state_path)?;
        metadata::create_directory(&state_path)?;
        hypervisor::request(
            &host.runtime_dir.join(name).join("api.sock"),
            context.pid,
            "PUT",
            "vm.pause",
        )?;
        ensure_api_state(host, &config_now, &context, "paused")?;
        checkpoint::capture_disks(host, &mut record)?;
        ensure_api_state(host, &config_now, &context, "paused")?;
        // RAM can take minutes to save. Helpers must be able to clean up if an
        // administrator independently stops the service during this request.
        drop(lock);
        hypervisor::snapshot_request(
            &host.runtime_dir.join(name).join("api.sock"),
            context.pid,
            &state_path,
        )?;
        let lock = global(host)?;
        ensure_api_state(host, &config_now, &context, "paused")?;
        checkpoint::capture_state_files(host, &mut record)?;
        record.phase = Phase::Saved;
        checkpoint::save(host, &record, false)?;
        drop(lock);
        stop_locked(&System, &Http, host, name, true, Duration::from_secs(60))?;
        let _lock = global(host)?;
        check_cold_edit(host, &config_now)?;
        record.phase = Phase::Suspended;
        return checkpoint::save(host, &record, false);
    };
    drop(lock);
    stop_locked(&System, &Http, host, name, true, Duration::from_secs(60))?;
    let _lock = global(host)?;
    check_cold_edit(host, &config_now)?;
    record.phase = Phase::Suspended;
    checkpoint::save(host, &record, false)
}
pub fn resume(host: &HostConfig, name: &str) -> Result<()> {
    use crate::checkpoint::{self, Phase};
    loaded(host, name)?;
    require_root()?;
    configured(host)?;
    let _operation = operation_lock(host, name)?;
    let lock = global(host)?;
    let config = instance::load(host, name)?;
    let mut record = checkpoint::suspend_record(host, &config)?
        .ok_or_else(|| error("instance has no suspend checkpoint"))?;
    crate::hotplug::ensure_no_pending(host, &config)?;
    let snapshot = System.snapshot(name)?;
    System.verify(&snapshot)?;
    if !systemd::terminal(&snapshot) {
        let live = checkpoint_context(host, &config)?;
        let path = host.runtime_dir.join(name).join("api.sock");
        let restored_here = record.restore_invocation == live.invocation;
        let cancelled_here = record.phase == Phase::Deleting
            && record.restore_invocation.is_empty()
            && record.invocation == live.invocation;
        if [Phase::Consumed, Phase::Deleting].contains(&record.phase)
            && (restored_here || cancelled_here)
        {
            ensure_api_state(host, &config, &live, "running")?;
            return checkpoint::delete_locked(host, &mut record, true);
        }
        // Explicitly cancelling an interrupted suspend can resume the original
        // paused VMM. It never replays a memory image over disks that have run.
        if [Phase::Creating, Phase::Saved].contains(&record.phase)
            && record.invocation == live.invocation
        {
            let state = hypervisor::vm_state(&path, live.pid)?;
            if state == "paused" {
                hypervisor::request(&path, live.pid, "PUT", "vm.resume")?;
            } else if state != "running" {
                return Err(error(
                    "original VMM is not in a cancellable paused/running state",
                ));
            }
            ensure_api_state(host, &config, &live, "running")?;
            return checkpoint::delete_locked(host, &mut record, true).map_err(|e| {
                error(format!(
                    "original VM resumed, but checkpoint cleanup needs inspection: {e}"
                ))
            });
        }
        return Err(error(
            "instance is running/transitioning or restore invocation differs; refusing replay",
        ));
    }
    recover_locked(&System, host, &config)?;
    if ![
        Phase::Saved,
        Phase::Suspended,
        Phase::Restoring,
        Phase::Resuming,
    ]
    .contains(&record.phase)
    {
        return Err(error("saved execution state is incomplete or a restore was already attempted; inspect it; use snapshot delete --discard for an explicit cold-boot decision"));
    }
    if checkpoint_version(host, &config)? != record.vmm_version {
        return Err(error(
            "Cloud Hypervisor binary, host kernel or boot files differ from saved execution state; refusing resume",
        ));
    }
    checkpoint::check_state_files(host, &record)?;
    checkpoint::restore_disks(host, &mut record, Phase::Resuming)?;
    drop(lock);
    start_with_checkpoint(host, name, true)?;
    let _lock = global(host)?;
    let live = checkpoint_context(host, &config)?;
    ensure_api_state(host, &config, &live, "running")?;
    let mut record = checkpoint::load(host, &record.name)?;
    if record.phase != Phase::Consumed || record.restore_invocation != live.invocation {
        return Err(error(
            "restore completion invocation does not match its checkpoint",
        ));
    }
    checkpoint::delete_locked(host, &mut record, true)
}
fn start_locked(host: &HostConfig, name: &str) -> Result<()> {
    start_with_checkpoint(host, name, false)
}
fn start_with_checkpoint(host: &HostConfig, name: &str, restore: bool) -> Result<()> {
    let _lock = global(host)?;
    instance::configs_unlocked(host)?;
    let config = instance::load(host, name)?;
    crate::hotplug::ensure_no_pending(host, &config)?;
    if !restore {
        crate::checkpoint::ensure_plain_start(host, &config)?;
    }
    let snapshot = System.snapshot(name)?;
    System.verify(&snapshot)?;
    if !systemd::terminal(&snapshot) {
        return Err(error(format!(
            "instance {name:?} is already active or transitioning; inspect it before starting"
        )));
    }
    let (program, _) = hypervisor::command_line(host, &config)?;
    hypervisor::check_capabilities(&program, config.windows)?;
    for net in &config.network {
        network::bridge_status(&net.bridge)?;
    }
    recover_locked(&System, host, &config)?;
    register(host, &config)?;
    // Nonblocking submission while the global lock protects the start/delete gap.
    // Helpers retry the metadata lock briefly; CLI releases it before readiness waits.
    System::reload()?;
    let snapshot = submit_start(&System, name)?;
    drop(_lock);
    let deadline = Instant::now() + Duration::from_secs(if restore { 1865 } else { 65 });
    loop {
        let s = System.snapshot(name)?;
        if s.active == "active" && s.sub == "running" {
            let r = runtime(host, &config)?;
            if r.invocation_id != s.invocation {
                return Err(error("runtime invocation differs after startup"));
            }
            if hypervisor::vm_state(&host.runtime_dir.join(name).join("api.sock"), s.pid)?
                == "running"
            {
                return Ok(());
            }
        }
        if s.active == "inactive"
            && s.invocation != snapshot.invocation
            && metadata::valid_id(&s.invocation)
        {
            return Err(error(
                "VMM exited before startup readiness; inspect its journal",
            ));
        }
        if s.active == "failed" {
            return Err(error(format!(
                "instance {name:?} startup failed; inspect journalctl -u {}",
                systemd::unit(name)?
            )));
        }
        if Instant::now() >= deadline {
            return Err(error(format!(
                "instance {name:?} startup timed out; inspect its unit (it may still be starting)"
            )));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
fn submit_start(manager: &impl Manager, name: &str) -> Result<Snapshot> {
    // Re-query after daemon-reload. An inactive template instance can be unloaded
    // between queries; reset-failed must not be a prerequisite for its first start.
    let snapshot = manager.snapshot(name)?;
    manager.verify(&snapshot)?;
    manager.quiescent(&snapshot)?;
    if snapshot.active == "failed" {
        manager.action("reset-failed", name)?;
    }
    manager.action("start", name)?;
    Ok(snapshot)
}
pub fn stop(host: &HostConfig, name: &str, force: bool) -> Result<()> {
    loaded(host, name)?;
    require_root()?;
    configured(host)?;
    let _operation = operation_lock(host, name)?;
    stop_locked(&System, &Http, host, name, force, Duration::from_secs(60))
}
pub fn reboot(host: &HostConfig, name: &str, force: bool) -> Result<()> {
    loaded(host, name)?;
    require_root()?;
    configured(host)?;
    let _operation = operation_lock(host, name)?;
    let s = System.snapshot(name)?;
    if systemd::terminal(&s) {
        return Err(error(format!("instance {name:?} is not running")));
    }
    stop_locked(&System, &Http, host, name, force, Duration::from_secs(60))?;
    start_locked(host, name)
}
fn stop_locked(
    manager: &impl Manager,
    api: &impl Api,
    host: &HostConfig,
    name: &str,
    force: bool,
    grace: Duration,
) -> Result<()> {
    let _lock = global(host)?;
    instance::configs_unlocked(host)?;
    let config = instance::load(host, name)?;
    if !registered(host, &config)? {
        return Err(error(
            "instance has not been registered with systemd; no managed VM to stop",
        ));
    }
    let first = manager.snapshot(name)?;
    manager.verify(&first)?;
    if systemd::terminal(&first) {
        return recover_locked(manager, host, &config);
    }
    if force {
        manager.action("stop", name)?;
        drop(_lock);
        wait_stopped(manager, name, &first.invocation, Duration::from_secs(20))?;
        finish_stop(manager, host, &config)?;
        if manager.snapshot(name)?.active == "failed" {
            manager.action("reset-failed", name)?;
        }
        return Ok(());
    }
    if first.active != "active" || first.pid == 0 {
        return Err(error("instance is transitioning or state is unknown; graceful stop is unavailable; use --force if required"));
    }
    let r = runtime(host, &config)?;
    if r.invocation_id != first.invocation {
        return Err(error("runtime invocation differs from systemd"));
    }
    api.power_button(&host.runtime_dir.join(name).join("api.sock"), first.pid)
        .map_err(|e| {
            error(format!(
                "graceful shutdown request failed: {e}; VM left running; use --force if required"
            ))
        })?;
    drop(_lock);
    let deadline = Instant::now() + grace;
    loop {
        let current = manager.snapshot(name)?;
        if systemd::terminal(&current) {
            if current.active == "failed" {
                return Err(error(
                    "VMM exited abnormally during graceful shutdown; inspect its journal",
                ));
            }
            return finish_stop(manager, host, &config);
        }
        if current.invocation != first.invocation {
            return Err(error(
                "instance restarted while waiting for shutdown; refusing to stop the new VM",
            ));
        }
        if Instant::now() >= deadline {
            return Err(error(format!("instance {name:?} did not shut down gracefully; VM left running; use --force to stop it")));
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}
fn finish_stop(manager: &impl Manager, host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    let _lock = global(host)?;
    instance::configs_unlocked(host)?;
    if instance::load(host, &config.name)?.id != config.id {
        return Err(error("instance generation changed during stop"));
    }
    recover_locked(manager, host, config)
}
fn confirm_cleanup(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    if instance::offline_runtime_state(host, &config.name)? != "stopped" {
        return Err(error("VMM is stopped but runtime cleanup is incomplete; inspect its journal and ownership before restarting"));
    }
    network::ensure_absent(config)
}

/// Explicit repair and startup use the same guarded path. It never signals a PID.
pub fn recover(host: &HostConfig, name: &str) -> Result<()> {
    loaded(host, name)?;
    require_root()?;
    configured(host)?;
    let _operation = operation_lock(host, name)?;
    let _lock = global(host)?;
    instance::configs_unlocked(host)?;
    let config = instance::load(host, name)?;
    recover_locked(&System, host, &config)?;
    crate::hotplug::recover_stopped(host, &config)
}

fn recover_locked(
    manager: &impl Manager,
    host: &HostConfig,
    config: &InstanceConfig,
) -> Result<()> {
    // Check ownership/contents before deciding whether anything may be removed.
    registered(host, config)?;
    let dir = host.runtime_dir.join(&config.name);
    let owner = match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(metadata::io_error("inspect stale runtime", &dir, e)),
        Ok(_) => Some(runtime(host, config)?),
    };
    let first = manager.snapshot(&config.name)?;
    manager.verify(&first)?;
    manager.quiescent(&first)?;
    let current = manager.snapshot(&config.name)?;
    same_invocation(&first, &current)?;
    manager.verify(&current)?;
    manager.quiescent(&current)?;
    if let Some(owner) = owner {
        // A previous invocation after reboot is safe only when systemd proves quiescence.
        cleanup_runtime(host, config, &owner.invocation_id)?;
    } else {
        // /run may have vanished on reboot while a manually prepared TAP survives.
        network::cleanup_unlocked(config)?;
    }
    confirm_cleanup(host, config)
}
fn wait_stopped(
    manager: &impl Manager,
    name: &str,
    invocation: &str,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        let s = manager.snapshot(name)?;
        if systemd::terminal(&s) {
            return Ok(());
        }
        if s.invocation != invocation {
            return Err(error(
                "instance invocation changed during stop; inspect the new VM",
            ));
        }
        if Instant::now() >= deadline {
            return Err(error(
                "systemd stop timed out; TAP/runtime cleanup has not been confirmed",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn helper_context(snapshot: &Snapshot, action: &str, invocation: &str, pid: u32) -> Result<()> {
    if !metadata::valid_id(invocation) || snapshot.invocation != invocation {
        return Err(error("service helper invocation does not match systemd"));
    }
    let expected_pid = if action == "run" {
        snapshot.pid
    } else {
        snapshot.control_pid
    };
    if expected_pid != pid || pid == 0 {
        return Err(error("service helper is not the unit's MainPID/ControlPID"));
    }
    Ok(())
}
/// Hidden, short-lived systemd entry points. Validate invocation context on every call.
pub fn service(action: &str, name: &str) -> Result<()> {
    require_root()?;
    metadata::validate_name("instance", name)?;
    if !["prepare", "run", "ready", "cleanup"].contains(&action) {
        return Err(error("unsupported service helper"));
    }
    let host = service_config()?;
    let invocation = std::env::var("INVOCATION_ID")
        .map_err(|_| error("service helpers require systemd INVOCATION_ID"))?;
    if !metadata::valid_id(&invocation) {
        return Err(error("invalid systemd invocation ID"));
    }
    let s = System.snapshot(name)?;
    helper_context(&s, action, &invocation, std::process::id())?;
    System.verify(&s)?;
    if action == "ready" {
        return ready(&host, name, &invocation);
    }
    let _lock = global(&host)?;
    // A helper may have waited for the metadata lock while systemd changed state.
    let s = System.snapshot(name)?;
    helper_context(&s, action, &invocation, std::process::id())?;
    System.verify(&s)?;
    instance::configs_unlocked(&host)?;
    let config = instance::load(&host, name)?;
    if ["prepare", "run"].contains(&action) {
        if let Some(record) = crate::checkpoint::suspend_record(&host, &config)? {
            if record.phase == crate::checkpoint::Phase::Resuming
                && checkpoint_version(&host, &config)? != record.vmm_version
            {
                return Err(error(
                    "saved-state host/binary/boot-file identity changed before service restore",
                ));
            }
        }
    }
    match action {
        "prepare" => {
            crate::hotplug::ensure_no_pending(&host, &config)?;
            register(&host, &config)?;
            if instance::offline_runtime_state(&host, name)? != "stopped" {
                return Err(error(
                    "runtime already exists; refusing to overwrite sockets or ownership",
                ));
            }
            let (program, _) = hypervisor::command_line(&host, &config)?;
            metadata::directory(
                program
                    .parent()
                    .ok_or_else(|| error("invalid hypervisor executable path"))?,
                false,
            )?;
            metadata::open_regular(&program, false)?;
            hypervisor::check_capabilities(&program, config.windows)?;
            metadata::directory(&host.runtime_dir, true)?;
            let dir = host.runtime_dir.join(name);
            metadata::create_directory(&dir)?;
            let record = Runtime {
                managed_by: "chadm".into(),
                name: name.into(),
                instance_id: config.id.clone(),
                invocation_id: invocation,
            };
            let text = toml::to_string(&record).map_err(|e| error(e.to_string()))?;
            if let Err(original) = metadata::write_new(&dir.join("owner.toml"), &text) {
                // This process created the directory. Only remove it if still empty;
                // never recursively delete an incomplete or ambiguous owner record.
                if let Err(cleanup) = fs::remove_dir(&dir) {
                    return Err(error(format!("{original}; runtime preparation rollback incomplete: {cleanup}; inspect {dir:?}")));
                }
                metadata::sync_directory(&host.runtime_dir)?;
                return Err(original);
            }
            metadata::sync_directory(&dir)?;
            metadata::sync_directory(&host.runtime_dir)?;
            match network::prepare_unlocked(&config) {
                Ok(()) => Ok(()),
                Err(original) => match cleanup_runtime(&host, &config, &record.invocation_id) {
                    Ok(()) => Err(original),
                    Err(cleanup) => Err(error(format!(
                        "{original}; runtime/TAP rollback incomplete: {cleanup}; inspect {dir:?}"
                    ))),
                },
            }
        }
        "run" => {
            let r = runtime(&host, &config)?;
            if r.invocation_id != invocation {
                return Err(error("runtime invocation differs"));
            }
            network::verify_prepared(&config)?;
            let (program, args) = hypervisor::command_line(&host, &config)?;
            crate::checkpoint::consume_restore(&host, &config, &invocation)?;
            drop(_lock);
            Err(error(format!(
                "cannot exec Cloud Hypervisor: {}",
                Command::new(program).args(args).env("LC_ALL", "C").exec()
            )))
        }
        "cleanup" => {
            if s.pid != 0 {
                return Err(error("VMM MainPID is still present; refusing TAP cleanup"));
            }
            cleanup_runtime(&host, &config, &invocation)
        }
        _ => Err(error("unsupported service helper")),
    }
}
fn ready(host: &HostConfig, name: &str, invocation: &str) -> Result<()> {
    let _lock = global(host)?;
    instance::configs_unlocked(host)?;
    let config = instance::load(host, name)?;
    let r = runtime(host, &config)?;
    if r.invocation_id != invocation {
        return Err(error("readiness invocation differs"));
    }
    let restoring = crate::checkpoint::suspend_record(host, &config)?.is_some_and(|r| {
        r.phase == crate::checkpoint::Phase::Consumed && r.restore_invocation == invocation
    });
    drop(_lock);
    let deadline = Instant::now() + Duration::from_secs(if restoring { 1800 } else { 30 });
    loop {
        let s = System.snapshot(name)?;
        if s.invocation != invocation || s.pid == 0 {
            return Err(error("VMM exited or invocation changed before readiness"));
        }
        if hypervisor::vm_state(&host.runtime_dir.join(name).join("api.sock"), s.pid)
            .is_ok_and(|state| state == "running")
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(error(
                "VM API did not report Running within startup/restore deadline",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
fn cleanup_runtime(host: &HostConfig, config: &InstanceConfig, invocation: &str) -> Result<()> {
    let dir = host.runtime_dir.join(&config.name);
    match fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(metadata::io_error("inspect runtime", &dir, e)),
        Ok(_) => (),
    }
    let r = runtime(host, config)?;
    if r.invocation_id != invocation {
        return Err(error(
            "cleanup belongs to another invocation; preserving runtime and TAPs",
        ));
    }
    // Validate every directory entry first; never recursively remove runtime.
    for entry in fs::read_dir(&dir).map_err(|e| metadata::io_error("inspect runtime", &dir, e))? {
        let entry = entry.map_err(|e| metadata::io_error("inspect runtime", &dir, e))?;
        let name = entry.file_name();
        if name == "owner.toml" {
            continue;
        }
        if name != "api.sock" && name != "console.sock" {
            return Err(error("runtime contains unknown files; refusing cleanup"));
        }
        use std::os::unix::fs::{FileTypeExt, MetadataExt};
        let m = fs::symlink_metadata(entry.path())
            .map_err(|e| metadata::io_error("inspect runtime socket", &entry.path(), e))?;
        if !m.file_type().is_socket() || m.uid() != metadata::uid() || m.nlink() != 1 {
            return Err(error("runtime socket has unexpected type or ownership"));
        }
    }
    network::cleanup_unlocked(config)?;
    for name in ["api.sock", "console.sock"] {
        let path = dir.join(name);
        match fs::remove_file(&path) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(metadata::io_error("remove runtime socket", &path, e)),
        }
    }
    fs::remove_file(dir.join("owner.toml"))
        .map_err(|e| metadata::io_error("remove runtime owner", &dir, e))?;
    fs::remove_dir(&dir).map_err(|e| metadata::io_error("remove runtime directory", &dir, e))?;
    metadata::sync_directory(&host.runtime_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{image, repo};
    use std::os::unix::fs::DirBuilderExt;
    use std::sync::{Arc, Mutex};
    struct Fixture {
        root: std::path::PathBuf,
        host: HostConfig,
        config: InstanceConfig,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "chadm-lifecycle-{}",
                metadata::random_id().unwrap()
            ));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let firmware = root.join("CLOUDHV.fd");
            fs::write(&firmware, b"test firmware").unwrap();
            let host = HostConfig {
                data_dir: root.join("data"),
                runtime_dir: root.join("run"),
                firmware: Some(firmware),
                default_bridge: None,
                cloud_hypervisor: "/bin/true".into(),
            };
            repo::create(&host, "local", &root.join("storage")).unwrap();
            let input = root.join("base.raw");
            fs::write(&input, b"test base").unwrap();
            image::create(&host, "base", "local", &input).unwrap();
            metadata::write_new(&host.firmware_path(), "test firmware").unwrap();
            let config =
                instance::create(&host, &instance::CreateOptions::new("test01", "base")).unwrap();
            Self { root, host, config }
        }
        fn runtime(&self) {
            register(&self.host, &self.config).unwrap();
            metadata::directory(&self.host.runtime_dir, true).unwrap();
            let dir = self.host.runtime_dir.join("test01");
            metadata::create_directory(&dir).unwrap();
            metadata::write_new(
                &dir.join("owner.toml"),
                &toml::to_string(&Runtime {
                    managed_by: "chadm".into(),
                    name: "test01".into(),
                    instance_id: self.config.id.clone(),
                    invocation_id: "a".repeat(32),
                })
                .unwrap(),
            )
            .unwrap();
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
    struct Fake {
        snapshot: Arc<Mutex<Snapshot>>,
        actions: Mutex<Vec<String>>,
        offline: bool,
    }
    impl Fake {
        fn new(active: &str, pid: u32) -> Self {
            Self {
                snapshot: Arc::new(Mutex::new(Snapshot {
                    load: "loaded".into(),
                    active: active.into(),
                    sub: if active == "active" {
                        "running"
                    } else {
                        "dead"
                    }
                    .into(),
                    pid,
                    control_pid: 0,
                    job: 0,
                    cgroup: String::new(),
                    invocation: "a".repeat(32),
                    fragment: "/etc/systemd/system/chadm-instance@.service".into(),
                    drop_ins: String::new(),
                })),
                actions: Mutex::new(Vec::new()),
                offline: false,
            }
        }
    }
    impl Manager for Fake {
        fn verify(&self, _: &Snapshot) -> Result<()> {
            Ok(())
        }
        fn snapshot(&self, _: &str) -> Result<Snapshot> {
            if self.offline {
                Err(error("systemd unavailable"))
            } else {
                Ok(self.snapshot.lock().unwrap().clone())
            }
        }
        fn action(&self, action: &str, _: &str) -> Result<()> {
            self.actions.lock().unwrap().push(action.into());
            let mut s = self.snapshot.lock().unwrap();
            if action == "reset-failed" && s.active != "failed" {
                return Err(error("Unit chadm-instance@test01.service not loaded"));
            }
            s.active = "inactive".into();
            s.pid = 0;
            Ok(())
        }
    }
    #[test]
    fn first_start_skips_reset_failed_but_failed_restart_resets_before_start() {
        let fresh = Fake::new("inactive", 0);
        // This manager rejects reset-failed for a fresh/unloaded template instance.
        submit_start(&fresh, "test01").unwrap();
        assert_eq!(*fresh.actions.lock().unwrap(), vec!["start"]);
        let failed = Fake::new("failed", 0);
        submit_start(&failed, "test01").unwrap();
        assert_eq!(
            *failed.actions.lock().unwrap(),
            vec!["reset-failed", "start"]
        );
    }

    #[test]
    fn cold_disk_edits_reject_active_transitioning_helpers_and_pending_jobs() {
        let f = Fixture::new();
        register(&f.host, &f.config).unwrap();
        let manager = Fake::new("active", 123);
        assert!(check_cold_edit_with(&manager, &f.host, &f.config)
            .unwrap_err()
            .to_string()
            .contains("requires a stopped instance"));
        let mut s = manager.snapshot.lock().unwrap();
        s.active = "inactive".into();
        s.pid = 0;
        s.control_pid = 12;
        drop(s);
        assert!(check_cold_edit_with(&manager, &f.host, &f.config).is_err());
        let mut s = manager.snapshot.lock().unwrap();
        s.control_pid = 0;
        s.job = 5;
        drop(s);
        assert!(check_cold_edit_with(&manager, &f.host, &f.config).is_err());
        manager.snapshot.lock().unwrap().job = 0;
        check_cold_edit_with(&manager, &f.host, &f.config).unwrap();
    }

    #[test]
    fn hotplug_requires_stable_running_systemd_without_jobs_or_helpers() {
        let manager = Fake::new("active", 42);
        let mut snapshot = manager.snapshot("test01").unwrap();
        snapshot.sub = "running".into();
        check_live_snapshot(&snapshot).unwrap();
        for (active, sub, pid, control, job) in [
            ("activating", "start", 42, 0, 0),
            ("deactivating", "stop", 42, 0, 0),
            ("active", "running", 0, 0, 0),
            ("active", "running", 42, 43, 0),
            ("active", "running", 42, 0, 7),
        ] {
            snapshot.active = active.into();
            snapshot.sub = sub.into();
            snapshot.pid = pid;
            snapshot.control_pid = control;
            snapshot.job = job;
            assert!(check_live_snapshot(&snapshot).is_err());
        }
    }

    #[test]
    fn start_submission_rechecks_helpers_jobs_and_query_failure_before_any_action() {
        let manager = Fake::new("inactive", 0);
        for (pid, helper, job) in [(42, 0, 0), (0, 42, 0), (0, 0, 42)] {
            let mut s = manager.snapshot.lock().unwrap();
            s.pid = pid;
            s.control_pid = helper;
            s.job = job;
            drop(s);
            assert!(submit_start(&manager, "test01").is_err());
            assert!(manager.actions.lock().unwrap().is_empty());
        }
        let mut manager = manager;
        manager.offline = true;
        assert!(submit_start(&manager, "test01").is_err());
        assert!(manager.actions.lock().unwrap().is_empty());
    }
    struct FakeApi {
        runtime: Option<std::path::PathBuf>,
        after: Option<Arc<Mutex<Snapshot>>>,
        calls: Mutex<usize>,
    }
    impl FakeApi {
        fn new(after: Option<Arc<Mutex<Snapshot>>>) -> Self {
            Self {
                runtime: None,
                after,
                calls: Mutex::new(0),
            }
        }
    }
    impl Api for FakeApi {
        fn state(&self, _: &std::path::Path, _: u32) -> Result<&'static str> {
            Ok("running")
        }
        fn power_button(&self, _: &std::path::Path, _: u32) -> Result<()> {
            *self.calls.lock().unwrap() += 1;
            if let Some(after) = &self.after {
                let mut s = after.lock().unwrap();
                s.active = "inactive".into();
                s.sub = "dead".into();
                s.pid = 0;
                if let Some(path) = &self.runtime {
                    fs::remove_file(path.join("owner.toml")).unwrap();
                    fs::remove_dir(path).unwrap();
                }
            }
            Ok(())
        }
    }
    #[test]
    fn argv_resolves_raw_volumes_firmware_and_direct_boot_without_shell_splitting() {
        let f = Fixture::new();
        let (_, args) = hypervisor::command_line(&f.host, &f.config).unwrap();
        let args: Vec<_> = args.iter().map(|s| s.to_str().unwrap()).collect();
        assert!(args.contains(&"--firmware"));
        assert!(args.contains(&"boot=2,max=2"));
        assert!(args.iter().any(|s| s.starts_with("path=")
            && s.ends_with(&format!(
                "readonly=off,image_type=raw,id=ch-disk-{}",
                f.config.disk[0].volume_id
            ))));
        assert!(args.contains(&"--serial"));
        assert!(args.contains(&"off"));
        assert!(!args.contains(&"--no-shutdown"));
        let kernel = f.root.join("kernel file");
        fs::write(&kernel, b"kernel").unwrap();
        let mut config = f.config.clone();
        config.boot = instance::BootConfig::Direct {
            kernel: kernel.clone(),
            initramfs: None,
            cmdline: "root=/dev/vda1 secret=keep; $value".into(),
        };
        let (_, args) = hypervisor::command_line(&f.host, &config).unwrap();
        assert!(args.contains(&kernel.as_os_str().to_os_string()));
        assert!(args.contains(&"root=/dev/vda1 secret=keep; $value".into()));
        let mut host = f.host.clone();
        host.runtime_dir = f.root.join("comma,dir");
        assert!(hypervisor::command_line(&host, &config).is_err());
        host.runtime_dir = f.root.join("a".repeat(110));
        assert!(hypervisor::command_line(&host, &config).is_err());
        config.disk[0].volume_id = "d".repeat(32);
        assert!(hypervisor::command_line(&f.host, &config).is_err());
    }
    #[test]
    fn graceful_shutdown_waits_for_exit_and_never_calls_force_stop() {
        let f = Fixture::new();
        f.runtime();
        let manager = Fake::new("active", std::process::id());
        let mut api = FakeApi::new(Some(manager.snapshot.clone()));
        api.runtime = Some(f.host.runtime_dir.join("test01"));
        stop_locked(
            &manager,
            &api,
            &f.host,
            "test01",
            false,
            Duration::from_secs(1),
        )
        .unwrap();
        assert_eq!(*api.calls.lock().unwrap(), 1);
        assert!(manager.actions.lock().unwrap().is_empty());
    }
    #[test]
    fn graceful_api_failure_preserves_vm_runtime_and_never_forces_stop() {
        struct Unavailable;
        impl Api for Unavailable {
            fn state(&self, _: &std::path::Path, _: u32) -> Result<&'static str> {
                Err(error("API unavailable"))
            }
            fn power_button(&self, _: &std::path::Path, _: u32) -> Result<()> {
                Err(error("API unavailable"))
            }
        }
        let f = Fixture::new();
        f.runtime();
        let manager = Fake::new("active", std::process::id());
        let result = stop_locked(
            &manager,
            &Unavailable,
            &f.host,
            "test01",
            false,
            Duration::ZERO,
        )
        .unwrap_err()
        .to_string();
        assert!(result.contains("VM left running"));
        assert!(result.contains("--force"));
        assert!(manager.actions.lock().unwrap().is_empty());
        assert_eq!(manager.snapshot.lock().unwrap().active, "active");
        assert!(f.host.runtime_dir.join("test01/owner.toml").exists());
    }
    #[test]
    fn graceful_timeout_preserves_vm_and_force_works_without_api() {
        let f = Fixture::new();
        f.runtime();
        let manager = Fake::new("active", std::process::id());
        let api = FakeApi::new(None);
        let e = stop_locked(&manager, &api, &f.host, "test01", false, Duration::ZERO)
            .unwrap_err()
            .to_string();
        assert_eq!(*api.calls.lock().unwrap(), 1);
        assert!(e.contains("VM left running"));
        assert!(manager.actions.lock().unwrap().is_empty());
        assert_eq!(manager.snapshot.lock().unwrap().active, "active");
        let dir = f.host.runtime_dir.join("test01");
        fs::remove_file(dir.join("owner.toml")).unwrap();
        fs::remove_dir(dir).unwrap();
        stop_locked(&manager, &api, &f.host, "test01", true, Duration::ZERO).unwrap();
        assert_eq!(*manager.actions.lock().unwrap(), vec!["stop"]);
    }
    #[test]
    fn state_uses_systemd_and_authenticated_api_and_does_not_guess_on_query_failure() {
        let f = Fixture::new();
        let manager = Fake::new("inactive", 0);
        assert_eq!(
            state_with(&manager, &FakeApi::new(None), &f.host, &f.config)
                .unwrap()
                .source,
            "metadata_only"
        );
        f.runtime();
        assert_eq!(
            state_with(&manager, &FakeApi::new(None), &f.host, &f.config)
                .unwrap()
                .name,
            "stopped"
        );
        {
            let mut s = manager.snapshot.lock().unwrap();
            s.active = "active".into();
            s.pid = std::process::id();
        }

        assert_eq!(
            state_with(&manager, &FakeApi::new(None), &f.host, &f.config)
                .unwrap()
                .name,
            "running"
        );
        let mut manager = manager;
        manager.offline = true;
        assert!(state_with(&manager, &FakeApi::new(None), &f.host, &f.config).is_err());
        let state = state(&f.host, &f.config);
        assert_eq!(state.name, "unknown");
        assert!(state.reason.unwrap().contains("configuration differs"));
    }
    #[test]
    fn runtime_cleanup_preserves_unknown_entries_and_other_invocations() {
        let f = Fixture::new();
        f.runtime();
        let dir = f.host.runtime_dir.join("test01");
        assert!(cleanup_runtime(&f.host, &f.config, &"b".repeat(32)).is_err());
        fs::write(dir.join("foreign"), b"retain").unwrap();
        assert!(cleanup_runtime(&f.host, &f.config, &"a".repeat(32)).is_err());
        assert!(dir.join("owner.toml").exists());
        fs::remove_file(dir.join("foreign")).unwrap();
        fs::write(dir.join("api.sock"), b"foreign regular file").unwrap();
        assert!(cleanup_runtime(&f.host, &f.config, &"a".repeat(32)).is_err());
        fs::remove_file(dir.join("api.sock")).unwrap();
        cleanup_runtime(&f.host, &f.config, &"a".repeat(32)).unwrap();
        assert!(!dir.exists());
        cleanup_runtime(&f.host, &f.config, &"a".repeat(32)).unwrap();
        assert!(registered(&f.host, &f.config).unwrap());
    }
    #[test]
    fn registration_and_operation_locks_protect_generation_and_metadata_deletion() {
        let f = Fixture::new();
        let lock = operation_lock(&f.host, "test01").unwrap();
        assert!(instance::delete(&f.host, "test01").is_err());
        drop(lock);
        register(&f.host, &f.config).unwrap();
        let mut wrong = f.config.clone();
        wrong.id = "d".repeat(32);
        assert!(registered(&f.host, &wrong).is_err());
        assert!(instance::delete(&f.host, "test01").is_err());
        assert!(registration_path(&f.host, "test01").exists());
    }
    #[test]
    fn helpers_reject_foreign_pid_and_invocation_and_force_confirms_cleanup() {
        let f = Fixture::new();
        f.runtime();
        let manager = Fake::new("active", 123);
        let mut snapshot = manager.snapshot("test01").unwrap();
        snapshot.control_pid = 456;
        assert!(helper_context(&snapshot, "run", &"a".repeat(32), 123).is_ok());
        assert!(helper_context(&snapshot, "ready", &"a".repeat(32), 456).is_ok());
        assert!(helper_context(&snapshot, "run", &"a".repeat(32), 456).is_err());
        assert!(helper_context(&snapshot, "cleanup", &"b".repeat(32), 456).is_err());
        assert!(helper_context(&snapshot, "cleanup", "bad", 456).is_err());
        stop_locked(
            &manager,
            &FakeApi::new(None),
            &f.host,
            "test01",
            true,
            Duration::ZERO,
        )
        .unwrap();
        assert!(!f.host.runtime_dir.join("test01").exists());
    }

    #[test]
    fn stale_recovery_is_idempotent_and_allows_a_previous_invocation() {
        let f = Fixture::new();
        f.runtime();
        let manager = Fake::new("failed", 0);
        manager.snapshot.lock().unwrap().invocation = "b".repeat(32);
        recover_locked(&manager, &f.host, &f.config).unwrap();
        recover_locked(&manager, &f.host, &f.config).unwrap();
        assert!(!f.host.runtime_dir.join("test01").exists());
        assert!(registration_path(&f.host, "test01").exists());
        assert!(manager.actions.lock().unwrap().is_empty());
    }

    #[test]
    fn recovery_preserves_runtime_for_live_helpers_jobs_bad_owners_and_unknown_files() {
        let f = Fixture::new();
        f.runtime();
        let dir = f.host.runtime_dir.join("test01");
        let manager = Fake::new("inactive", 0);
        for (pid, helper, job) in [(123, 0, 0), (0, 456, 0), (0, 0, 1)] {
            let mut s = manager.snapshot.lock().unwrap();
            s.pid = pid;
            s.control_pid = helper;
            s.job = job;
            drop(s);
            assert!(recover_locked(&manager, &f.host, &f.config).is_err());
            assert!(dir.join("owner.toml").exists());
        }
        let mut s = manager.snapshot.lock().unwrap();
        s.pid = 0;
        s.control_pid = 0;
        s.job = 0;
        drop(s);
        fs::write(dir.join("foreign"), b"retain").unwrap();
        assert!(recover_locked(&manager, &f.host, &f.config).is_err());
        assert!(dir.join("foreign").exists());
        fs::remove_file(dir.join("foreign")).unwrap();
        let mut wrong = f.config.clone();
        wrong.id = "c".repeat(32);
        assert!(recover_locked(&manager, &f.host, &wrong).is_err());
        assert!(dir.join("owner.toml").exists());
        let mut manager = manager;
        manager.offline = true;
        assert!(recover_locked(&manager, &f.host, &f.config).is_err());
        assert!(dir.join("owner.toml").exists());
    }

    #[test]
    fn recovery_rechecks_systemd_before_deleting_any_artifact() {
        struct Changed {
            first: Snapshot,
            reads: Mutex<usize>,
        }
        impl Manager for Changed {
            fn verify(&self, _: &Snapshot) -> Result<()> {
                Ok(())
            }
            fn snapshot(&self, _: &str) -> Result<Snapshot> {
                let mut count = self.reads.lock().unwrap();
                let mut s = self.first.clone();
                if *count > 0 {
                    s.invocation = "b".repeat(32);
                    s.job = 7;
                }
                *count += 1;
                Ok(s)
            }
            fn action(&self, _: &str, _: &str) -> Result<()> {
                panic!("recovery must not submit jobs")
            }
        }
        let f = Fixture::new();
        f.runtime();
        let manager = Changed {
            first: Fake::new("inactive", 0).snapshot("test01").unwrap(),
            reads: Mutex::new(0),
        };
        assert!(recover_locked(&manager, &f.host, &f.config).is_err());
        assert!(f.host.runtime_dir.join("test01/owner.toml").exists());
    }
}
