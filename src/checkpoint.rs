//! Durable disk snapshots and suspend checkpoints. Callers serialize mutations
//! with the instance operation lock and global metadata lock.
use crate::{
    config::HostConfig,
    error::{Error, Result},
    instance::{self, InstanceConfig},
    metadata::{self, Lock},
    output::json_string,
    repo::{self, RepoConfig},
    storage::{self, StorageDriver},
    volume::{self, VolumeConfig},
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

fn error(message: impl Into<String>) -> Error {
    Error::Resource(message.into())
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Disk,
    Suspend,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Creating,
    Ready,
    Saved,
    Suspended,
    Restoring,
    Resuming,
    Consumed,
    Deleting,
}
impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Ready => "ready",
            Self::Saved => "saved",
            Self::Suspended => "suspended",
            Self::Restoring => "restoring",
            Self::Resuming => "resuming",
            Self::Consumed => "consumed",
            Self::Deleting => "deleting",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Disk {
    pub volume: VolumeConfig,
    pub repo: RepoConfig,
    #[serde(
        default,
        with = "crate::serde_guid",
        skip_serializing_if = "Option::is_none"
    )]
    pub identity: Option<u64>,
    pub deleted: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StateFile {
    pub name: String,
    #[serde(with = "crate::serde_guid")]
    pub inode: Option<u64>,
    pub size: u64,
    pub modified: i64,
    pub modified_nsec: i64,
    pub deleted: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    pub schema_version: u32,
    pub managed_by: String,
    pub name: String,
    pub id: String,
    pub kind: Kind,
    pub phase: Phase,
    pub created_at_unix: u64,
    pub instance: InstanceConfig,
    pub data_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub vmm_version: String,
    pub invocation: String,
    pub restore_invocation: String,
    pub disks: Vec<Disk>,
    pub files: Vec<StateFile>,
}
fn root(host: &HostConfig) -> PathBuf {
    host.data_dir.join("snapshots")
}
pub(crate) fn directory(host: &HostConfig, record: &Record) -> PathBuf {
    root(host).join(&record.name)
}
pub(crate) fn state_path(host: &HostConfig, record: &Record) -> PathBuf {
    directory(host, record).join("state")
}
fn same_config(a: &InstanceConfig, b: &InstanceConfig) -> bool {
    toml::to_string(a).ok() == toml::to_string(b).ok()
}
fn valid_state_file(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || [b'-', b'_', b'.'].contains(&b))
}

pub(crate) fn records(host: &HostConfig) -> Result<Vec<Record>> {
    let path = root(host);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(metadata::io_error("inspect snapshots", &path, e)),
        Ok(_) => (),
    }
    metadata::directory(&path, false)?;
    let mut records = Vec::new();
    let mut identities = std::collections::HashSet::new();
    for entry in fs::read_dir(&path).map_err(|e| metadata::io_error("list snapshots", &path, e))? {
        let entry = entry.map_err(|e| metadata::io_error("list snapshots", &path, e))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| error("invalid snapshot name"))?;
        metadata::validate_name("snapshot", &name)?;
        metadata::directory(&entry.path(), false)?;
        let config_path = entry.path().join("config.toml");
        let record: Record = toml::from_str(&metadata::read(&config_path)?)
            .map_err(|_| error("invalid snapshot manifest; preserve transaction evidence"))?;
        validate(host, &name, &record)?;
        if !identities.insert(record.id.clone()) {
            return Err(error(
                "duplicate snapshot generation; preserve metadata for inspection",
            ));
        }
        records.push(record);
    }
    records.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(records)
}
fn validate(host: &HostConfig, name: &str, r: &Record) -> Result<()> {
    crate::instance_config::validate(&r.instance)?;
    if r.schema_version != 1
        || r.managed_by != "chadm"
        || r.name != name
        || !metadata::valid_id(&r.id)
        || r.data_dir != host.data_dir
        || r.runtime_dir != host.runtime_dir
        || r.disks.len() != r.instance.disk.len()
    {
        return Err(error("snapshot ownership/configuration differs"));
    }
    if r.kind == Kind::Suspend && !metadata::valid_id(&r.invocation) {
        return Err(error("suspend invocation is invalid"));
    }
    if !r.restore_invocation.is_empty() && !metadata::valid_id(&r.restore_invocation) {
        return Err(error("restore invocation is invalid"));
    }
    let mut names = std::collections::HashSet::new();
    for file in &r.files {
        if !valid_state_file(&file.name) || file.inode.is_none() || !names.insert(&file.name) {
            return Err(error("invalid saved state file manifest"));
        }
    }
    for (disk, reference) in r.disks.iter().zip(&r.instance.disk) {
        if disk.volume.name != reference.volume
            || disk.volume.id != reference.volume_id
            || disk.volume.repo != disk.repo.name
            || disk.volume.repo_id != disk.repo.id
            || disk.identity == Some(0)
        {
            return Err(error("snapshot disk generation/repo differs"));
        }
    }
    if r.phase != Phase::Creating
        && r.phase != Phase::Deleting
        && r.disks.iter().any(|d| d.identity.is_none() || d.deleted)
    {
        return Err(error("snapshot disk manifest is incomplete"));
    }
    for entry in fs::read_dir(directory(host, r)).map_err(|e| error(e.to_string()))? {
        let name = entry.map_err(|e| error(e.to_string()))?.file_name();
        if name != "config.toml" && !(r.kind == Kind::Suspend && name == "state") {
            return Err(error(
                "unexpected or interrupted snapshot metadata; inspect before continuing",
            ));
        }
    }
    Ok(())
}
pub(crate) fn load(host: &HostConfig, name: &str) -> Result<Record> {
    metadata::validate_name("snapshot", name)?;
    records(host)?
        .into_iter()
        .find(|r| r.name == name)
        .ok_or_else(|| error(format!("snapshot {name:?} was not found")))
}
pub(crate) fn save(host: &HostConfig, r: &Record, new: bool) -> Result<()> {
    let dir = directory(host, r);
    let encoded =
        toml::to_string_pretty(r).map_err(|e| error(format!("cannot serialize snapshot: {e}")))?;
    if encoded.len() > 65536 {
        return Err(error("snapshot manifest exceeds 64 KiB"));
    }
    if new {
        metadata::write_new(&dir.join("config.toml"), &encoded)?;
    } else {
        metadata::open_regular(&dir.join("config.toml"), false)?;
        let temporary = dir.join(".next.toml");
        metadata::write_new(&temporary, &encoded)?;
        fs::rename(&temporary, dir.join("config.toml"))
            .map_err(|e| metadata::io_error("publish snapshot state", &dir, e))?;
    }
    metadata::sync_directory(&dir)?;
    metadata::sync_directory(&root(host))
}
pub(crate) fn ensure_config_editable(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    if records(host)?
        .iter()
        .any(|r| r.instance.id == config.id || r.instance.name == config.name)
    {
        return Err(error("instance has snapshots or a suspend checkpoint; delete them explicitly before editing/deleting the instance"));
    }
    Ok(())
}
pub(crate) fn ensure_volume_deletable(host: &HostConfig, volume: &VolumeConfig) -> Result<()> {
    if records(host)?.iter().any(|r| {
        r.disks
            .iter()
            .any(|d| d.volume.id == volume.id || d.volume.name == volume.name)
    }) {
        return Err(error(
            "volume is referenced by a snapshot or suspend checkpoint",
        ));
    }
    Ok(())
}
pub(crate) fn ensure_repo_deletable(host: &HostConfig, repo: &RepoConfig) -> Result<()> {
    if records(host)?.iter().any(|r| {
        r.disks
            .iter()
            .any(|d| d.repo.id == repo.id || d.repo.name == repo.name)
    }) {
        return Err(error(
            "repo is referenced by a snapshot or suspend checkpoint",
        ));
    }
    Ok(())
}
pub(crate) fn ensure_image_deletable(
    host: &HostConfig,
    image: &crate::image::ImageConfig,
) -> Result<()> {
    if records(host)?.iter().any(|r| {
        r.instance.image_id == image.id
            || r.instance.image == image.name
            || r.instance
                .cloud_init
                .as_ref()
                .is_some_and(|s| s.image_id == image.id || s.image == image.name)
    }) {
        return Err(error(
            "image is referenced by a snapshot or suspend checkpoint",
        ));
    }
    Ok(())
}
pub(crate) fn ensure_plain_start(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    for r in records(host)?
        .iter()
        .filter(|r| r.instance.id == config.id || r.instance.name == config.name)
    {
        current(host, r, config)?;
        if r.kind == Kind::Suspend || r.phase != Phase::Ready {
            return Err(error("instance has a suspend/unfinished snapshot transaction; use instance resume or finish the snapshot operation before starting"));
        }
    }
    Ok(())
}
pub(crate) fn ensure_can_capture(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    crate::hotplug::ensure_no_pending(host, config)?;
    ensure_plain_start(host, config)
}
fn current(host: &HostConfig, r: &Record, config: &InstanceConfig) -> Result<()> {
    if !same_config(&r.instance, config) {
        return Err(error(
            "instance configuration differs from snapshot; refusing restore/start",
        ));
    }
    for disk in &r.disks {
        if volume::load(host, &disk.volume.name)? != disk.volume
            || repo::load(host, &disk.repo.name)? != disk.repo
        {
            return Err(error(
                "snapshot volume/repo generation or configuration changed",
            ));
        }
    }
    Ok(())
}
fn driver(disk: &Disk) -> Result<Box<dyn StorageDriver>> {
    storage::driver(disk.repo.clone())
}

pub(crate) fn begin(
    host: &HostConfig,
    config: &InstanceConfig,
    name: &str,
    kind: Kind,
    invocation: &str,
    version: &str,
) -> Result<Record> {
    metadata::validate_name("snapshot", name)?;
    ensure_can_capture(host, config)?;
    if records(host)?.iter().any(|r| r.name == name) {
        return Err(error(format!("snapshot {name:?} already exists")));
    }
    let mut disks = Vec::new();
    for reference in &config.disk {
        let volume = volume::load(host, &reference.volume)?;
        if volume.id != reference.volume_id {
            return Err(error("snapshot volume generation changed"));
        }
        let repo = repo::load(host, &volume.repo)?;
        storage::driver(repo.clone())?.check_volume(&volume)?;
        disks.push(Disk {
            volume,
            repo,
            identity: None,
            deleted: false,
        });
    }
    let r = Record {
        schema_version: 1,
        managed_by: "chadm".into(),
        name: name.into(),
        id: metadata::random_id()?,
        kind,
        phase: Phase::Creating,
        created_at_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| error("host clock precedes Unix epoch"))?
            .as_secs(),
        instance: config.clone(),
        data_dir: host.data_dir.clone(),
        runtime_dir: host.runtime_dir.clone(),
        vmm_version: version.into(),
        invocation: invocation.into(),
        restore_invocation: String::new(),
        disks,
        files: Vec::new(),
    };
    metadata::directory(&root(host), true)?;
    metadata::create_directory(&directory(host, &r))?;
    save(host, &r, true)?;
    Ok(r)
}
pub(crate) fn capture_disks(host: &HostConfig, r: &mut Record) -> Result<()> {
    for index in 0..r.disks.len() {
        let d = &r.disks[index];
        let identity = driver(d)?.snapshot_volume(&d.volume, &r.id)?;
        r.disks[index].identity = Some(identity);
        save(host, r, false)?;
    }
    Ok(())
}
pub fn create(host: &HostConfig, name: &str, instance: &str) -> Result<()> {
    host.validate()?;
    metadata::validate_name("instance", instance)?;
    metadata::validate_name("snapshot", name)?;
    let Some(read_lock) = Lock::acquire(&host.data_dir, false)? else {
        return Err(error(format!("instance {instance:?} was not found")));
    };
    instance::load(host, instance)?;
    drop(read_lock);
    let _operation = crate::lifecycle::operation_lock(host, instance)?;
    let _lock = Lock::acquire(&host.data_dir, true)?;
    instance::configs_unlocked(host)?;
    let config = instance::load(host, instance)?;
    crate::lifecycle::check_cold_edit(host, &config)?;
    let mut r = begin(host, &config, name, Kind::Disk, "", "")?;
    capture_disks(host, &mut r)?;
    crate::lifecycle::check_cold_edit(host, &config)?;
    r.phase = Phase::Ready;
    save(host, &r, false)
}
pub(crate) fn preflight_restore(host: &HostConfig, r: &Record) -> Result<()> {
    current(host, r, &instance::load(host, &r.instance.name)?)?;
    for d in &r.disks {
        driver(d)?.check_snapshot(
            &d.volume,
            &r.id,
            d.identity
                .ok_or_else(|| error("snapshot payload is incomplete"))?,
            true,
        )?;
    }
    Ok(())
}
pub(crate) fn restore_disks(host: &HostConfig, r: &mut Record, complete: Phase) -> Result<()> {
    preflight_restore(host, r)?;
    r.phase = Phase::Restoring;
    save(host, r, false)?;
    for d in &r.disks {
        driver(d)?.restore_snapshot(
            &d.volume,
            &r.id,
            d.identity
                .ok_or_else(|| error("snapshot payload is incomplete"))?,
        )?;
    }
    r.phase = complete;
    save(host, r, false)
}
pub fn restore(host: &HostConfig, name: &str) -> Result<()> {
    host.validate()?;
    let initial = load(host, name)?;
    let _operation = crate::lifecycle::operation_lock(host, &initial.instance.name)?;
    let _lock = Lock::acquire(&host.data_dir, true)?;
    let mut r = load(host, name)?;
    if r.id != initial.id {
        return Err(error("snapshot generation changed during operation"));
    }
    if r.kind != Kind::Disk || ![Phase::Ready, Phase::Restoring].contains(&r.phase) {
        return Err(error("snapshot is not a restorable disk snapshot; use instance resume for suspend checkpoints"));
    }
    let config = instance::load(host, &r.instance.name)?;
    crate::lifecycle::check_cold_edit(host, &config)?;
    // A second incomplete operation on the same instance must never be bypassed.
    for other in records(host)? {
        if other.id != r.id
            && other.instance.id == r.instance.id
            && (other.kind == Kind::Suspend || other.phase != Phase::Ready)
        {
            return Err(error("another checkpoint transaction blocks restore"));
        }
    }
    restore_disks(host, &mut r, Phase::Ready)
}

pub(crate) fn capture_state_files(host: &HostConfig, r: &mut Record) -> Result<()> {
    let path = state_path(host, r);
    metadata::directory(&path, false)?;
    let mut files = Vec::new();
    for entry in
        fs::read_dir(&path).map_err(|e| metadata::io_error("inspect VMM snapshot", &path, e))?
    {
        let entry = entry.map_err(|e| error(e.to_string()))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| error("invalid VMM snapshot filename"))?;
        if !valid_state_file(&name) {
            return Err(error("unexpected VMM snapshot filename"));
        }
        let file = metadata::open_regular(&entry.path(), false)?;
        file.set_permissions(fs::Permissions::from_mode(0o400))
            .map_err(|e| error(e.to_string()))?;
        file.sync_all().map_err(|e| error(e.to_string()))?;
        let m = file.metadata().map_err(|e| error(e.to_string()))?;
        files.push(StateFile {
            name,
            inode: Some(m.ino()),
            size: m.len(),
            modified: m.mtime(),
            modified_nsec: m.mtime_nsec(),
            deleted: false,
        });
    }
    for required in ["config.json", "state.json", "memory-ranges"] {
        if !files.iter().any(|f| f.name == required && f.size > 0) {
            return Err(error(format!("VMM snapshot is missing {required}")));
        }
    }
    files.sort_by(|a, b| a.name.cmp(&b.name));
    metadata::sync_directory(&path)?;
    r.files = files;
    save(host, r, false)
}
pub(crate) fn check_state_files(host: &HostConfig, r: &Record) -> Result<()> {
    let dir = state_path(host, r);
    metadata::directory(&dir, false)?;
    for required in ["config.json", "state.json", "memory-ranges"] {
        if !r
            .files
            .iter()
            .any(|f| f.name == required && !f.deleted && f.size > 0)
        {
            return Err(error("saved VMM state is incomplete"));
        }
    }
    let mut count = 0;
    for entry in fs::read_dir(&dir).map_err(|e| error(e.to_string()))? {
        let entry = entry.map_err(|e| error(e.to_string()))?;
        let name = entry.file_name();
        let expected = r
            .files
            .iter()
            .find(|f| name == f.name.as_str() && !f.deleted)
            .ok_or_else(|| error("unknown file in saved VMM state"))?;
        check_state_file(&entry.path(), expected)?;
        count += 1;
    }
    if count != r.files.iter().filter(|f| !f.deleted).count() {
        return Err(error("saved VMM state files are missing"));
    }
    Ok(())
}
fn check_state_file(path: &Path, expected: &StateFile) -> Result<()> {
    let file = metadata::open_regular(path, false)?;
    let m = file.metadata().map_err(|e| error(e.to_string()))?;
    if Some(m.ino()) != expected.inode
        || m.len() != expected.size
        || m.mtime() != expected.modified
        || m.mtime_nsec() != expected.modified_nsec
        || m.mode() & 0o222 != 0
    {
        return Err(error(
            "saved VMM state file identity/size/modification changed",
        ));
    }
    Ok(())
}
pub(crate) fn delete_locked(host: &HostConfig, r: &mut Record, discard: bool) -> Result<()> {
    if r.phase == Phase::Restoring {
        return Err(error(
            "finish the interrupted disk restore before deleting its snapshot",
        ));
    }
    if r.kind == Kind::Suspend && !discard {
        return Err(error("deleting a suspend checkpoint discards saved execution state; pass --discard explicitly"));
    }
    r.phase = Phase::Deleting;
    save(host, r, false)?;
    for index in 0..r.disks.len() {
        let disk = &r.disks[index];
        if disk.deleted {
            continue;
        }
        let backend = driver(disk)?;
        if backend.snapshot_exists(&disk.volume, &r.id)? {
            backend.delete_snapshot(&disk.volume,&r.id,disk.identity.ok_or_else(||error("snapshot creation was interrupted before identity was recorded; preserve native object for ownership audit"))?)?;
        }
        r.disks[index].deleted = true;
        save(host, r, false)?;
    }
    let state = state_path(host, r);
    if r.kind == Kind::Suspend && state.try_exists().map_err(|e| error(e.to_string()))? {
        metadata::directory(&state, false)?;
        // Unknown/partial files from an interrupted API snapshot are preserved.
        for entry in fs::read_dir(&state).map_err(|e| error(e.to_string()))? {
            let entry = entry.map_err(|e| error(e.to_string()))?;
            if !r.files.iter().any(|f| entry.file_name() == f.name.as_str()) {
                return Err(error(
                    "unrecorded partial VMM state; preserve it for inspection",
                ));
            }
        }
        for index in 0..r.files.len() {
            let file = &r.files[index];
            let path = state.join(&file.name);
            if !file.deleted && path.try_exists().map_err(|e| error(e.to_string()))? {
                check_state_file(&path, file)?;
                fs::remove_file(&path).map_err(|e| error(e.to_string()))?;
                metadata::sync_directory(&state)?;
            }
            r.files[index].deleted = true;
            save(host, r, false)?;
        }
        fs::remove_dir(&state).map_err(|e| error(e.to_string()))?;
    }
    let path = directory(host, r);
    // Validate the manifest directory before removing the last ownership evidence.
    validate(host, &r.name, r)?;
    fs::remove_file(path.join("config.toml")).map_err(|e| error(e.to_string()))?;
    fs::remove_dir(&path).map_err(|e| error(e.to_string()))?;
    metadata::sync_directory(&root(host))
}
pub fn delete(host: &HostConfig, name: &str, discard: bool) -> Result<()> {
    host.validate()?;
    let initial = load(host, name)?;
    let _operation = crate::lifecycle::operation_lock(host, &initial.instance.name)?;
    let _lock = Lock::acquire(&host.data_dir, true)?;
    let mut r = load(host, name)?;
    if r.id != initial.id {
        return Err(error("snapshot generation changed during operation"));
    }
    if r.kind == Kind::Suspend {
        crate::lifecycle::check_cold_edit(host, &instance::load(host, &r.instance.name)?)?;
    }
    delete_locked(host, &mut r, discard)
}
pub(crate) fn suspend_record(host: &HostConfig, config: &InstanceConfig) -> Result<Option<Record>> {
    let mut found = None;
    for r in records(host)? {
        if (r.instance.id == config.id || r.instance.name == config.name) && r.kind == Kind::Suspend
        {
            current(host, &r, config)?;
            if found.replace(r).is_some() {
                return Err(error("multiple suspend checkpoints for one instance"));
            }
        }
    }
    Ok(found)
}
pub(crate) fn restore_source(
    host: &HostConfig,
    config: &InstanceConfig,
) -> Result<Option<PathBuf>> {
    let Some(r) = suspend_record(host, config)? else {
        ensure_plain_start(host, config)?;
        return Ok(None);
    };
    if r.phase != Phase::Resuming {
        return Err(error("instance has saved state; use instance resume, or snapshot delete --discard before cold boot"));
    }
    check_state_files(host, &r)?;
    Ok(Some(state_path(host, &r)))
}
pub(crate) fn consume_restore(
    host: &HostConfig,
    config: &InstanceConfig,
    invocation: &str,
) -> Result<()> {
    if let Some(mut r) = suspend_record(host, config)? {
        if r.phase != Phase::Resuming {
            return Err(error("restore is not authorized or was already attempted"));
        }
        r.phase = Phase::Consumed;
        r.restore_invocation = invocation.into();
        save(host, &r, false)?;
    }
    Ok(())
}
pub(crate) fn status(host: &HostConfig, config: &InstanceConfig) -> Result<Option<&'static str>> {
    if let Some(r) = suspend_record(host, config)? {
        return Ok(Some(match r.phase {
            Phase::Saved | Phase::Suspended => "suspended",
            Phase::Restoring => "restoring",
            Phase::Resuming => "resuming",
            _ => "checkpoint-pending",
        }));
    }
    Ok(records(host)?
        .iter()
        .find(|r| r.instance.id == config.id && r.phase != Phase::Ready)
        .map(|r| r.phase.label()))
}

pub struct SnapshotInfo {
    pub name: String,
    pub instance: String,
    pub kind: Kind,
    pub phase: Phase,
    pub created_at_unix: u64,
    pub volumes: Vec<(String, String, String)>,
    pub available: bool,
    pub reason: Option<String>,
}
impl SnapshotInfo {
    pub fn json(&self) -> String {
        let volumes = self
            .volumes
            .iter()
            .map(|(volume, repo, backend)| {
                format!(
                    "{{\"volume\":{},\"repo\":{},\"backend\":{}}}",
                    json_string(volume),
                    json_string(repo),
                    json_string(backend)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"name\":{},\"instance\":{},\"kind\":{},\"phase\":{},\"created_at_unix\":{},\"volumes\":[{}],\"available\":{},\"reason\":{}}}",json_string(&self.name),json_string(&self.instance),json_string(if self.kind==Kind::Disk{"disk"}else{"suspend"}),json_string(self.phase.label()),self.created_at_unix,volumes,self.available,self.reason.as_ref().map(|s|json_string(s)).unwrap_or_else(||"null".into()))
    }
}
fn info(host: &HostConfig, r: Record) -> SnapshotInfo {
    let checked = (|| {
        current(host, &r, &instance::load(host, &r.instance.name)?)?;
        if ![
            Phase::Ready,
            Phase::Saved,
            Phase::Suspended,
            Phase::Resuming,
            Phase::Consumed,
        ]
        .contains(&r.phase)
        {
            return Err(error("snapshot transaction is incomplete"));
        }
        for d in &r.disks {
            driver(d)?.check_snapshot(
                &d.volume,
                &r.id,
                d.identity
                    .ok_or_else(|| error("snapshot payload is incomplete"))?,
                false,
            )?;
        }
        if r.kind == Kind::Suspend {
            check_state_files(host, &r)?;
        }
        Ok(())
    })();
    SnapshotInfo {
        name: r.name,
        instance: r.instance.name,
        kind: r.kind,
        phase: r.phase,
        created_at_unix: r.created_at_unix,
        volumes: r
            .disks
            .iter()
            .map(|d| {
                (
                    d.volume.name.clone(),
                    d.repo.name.clone(),
                    if d.repo.driver == repo::Driver::Zfs {
                        "zfs-native"
                    } else {
                        "file-reflink-or-copy"
                    }
                    .into(),
                )
            })
            .collect(),
        available: checked.is_ok(),
        reason: checked.err().map(|e: Error| e.to_string()),
    }
}
pub fn list(host: &HostConfig) -> Result<Vec<SnapshotInfo>> {
    host.validate()?;
    let _lock = Lock::acquire(&host.data_dir, false)?;
    Ok(records(host)?.into_iter().map(|r| info(host, r)).collect())
}
pub fn show(host: &HostConfig, name: &str) -> Result<SnapshotInfo> {
    host.validate()?;
    let _lock = Lock::acquire(&host.data_dir, false)?;
    Ok(info(host, load(host, name)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, DirBuilderExt};
    struct Fixture {
        root: PathBuf,
        host: HostConfig,
        config: InstanceConfig,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "chadm-save-state-{}",
                metadata::random_id().unwrap()
            ));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let host = HostConfig {
                data_dir: root.join("data"),
                runtime_dir: root.join("run"),
                ..Default::default()
            };
            repo::create(&host, "local", &root.join("storage")).unwrap();
            let source = root.join("base.raw");
            fs::File::create(&source).unwrap().set_len(1 << 20).unwrap();
            crate::image::create(&host, "base", "local", &source).unwrap();
            let mut options = instance::CreateOptions::new("test01", "base");
            options.no_cloud_init = true;
            let config = instance::create(&host, &options).unwrap();
            metadata::write_new(&host.firmware_path(), "test firmware").unwrap();
            Self { root, host, config }
        }
        fn saved(&self) -> Record {
            let mut record = begin(
                &self.host,
                &self.config,
                "saved",
                Kind::Suspend,
                &"a".repeat(32),
                "test-version",
            )
            .unwrap();
            capture_disks(&self.host, &mut record).unwrap();
            let state = state_path(&self.host, &record);
            metadata::create_directory(&state).unwrap();
            for name in ["config.json", "state.json", "memory-ranges"] {
                metadata::write_new(&state.join(name), "private test state").unwrap();
            }
            capture_state_files(&self.host, &mut record).unwrap();
            record.phase = Phase::Suspended;
            save(&self.host, &record, false).unwrap();
            record
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }
    #[test]
    fn saved_state_blocks_cold_boot_edits_and_implicit_discard() {
        let f = Fixture::new();
        let mut r = f.saved();
        assert_eq!(status(&f.host, &f.config).unwrap(), Some("suspended"));
        assert!(ensure_plain_start(&f.host, &f.config).is_err());
        assert!(ensure_config_editable(&f.host, &f.config).is_err());
        assert!(delete_locked(&f.host, &mut r, false).is_err());
        assert!(show(&f.host, "saved").unwrap().available);
        assert!(restore(&f.host, "saved").is_err());
        delete_locked(&f.host, &mut r, true).unwrap();
        assert!(records(&f.host).unwrap().is_empty());
        ensure_plain_start(&f.host, &f.config).unwrap();
    }
    #[test]
    fn restore_is_consumed_before_exec_and_cannot_be_replayed_after_a_crash() {
        let f = Fixture::new();
        let mut r = f.saved();
        restore_disks(&f.host, &mut r, Phase::Resuming).unwrap();
        let (_, args) = crate::hypervisor::command_line(&f.host, &f.config).unwrap();
        let args: Vec<_> = args.iter().map(|arg| arg.to_str().unwrap()).collect();
        assert!(args.contains(&"--restore"));
        assert!(!args.contains(&"--disk"));
        assert!(!args.contains(&"--cpus"));
        assert!(args.iter().any(|arg| arg.starts_with("source_url=file://")
            && arg.ends_with("resume=true,memory_restore_mode=copy")));
        assert_eq!(
            restore_source(&f.host, &f.config).unwrap(),
            Some(state_path(&f.host, &r))
        );
        consume_restore(&f.host, &f.config, &"b".repeat(32)).unwrap();
        let r = load(&f.host, "saved").unwrap();
        assert_eq!(r.phase, Phase::Consumed);
        assert_eq!(r.restore_invocation, "b".repeat(32));
        assert!(restore_source(&f.host, &f.config).is_err());
        assert!(consume_restore(&f.host, &f.config, &"c".repeat(32)).is_err());
        assert!(ensure_plain_start(&f.host, &f.config).is_err());
        assert!(show(&f.host, "saved").unwrap().available);
    }
    #[test]
    fn saved_memory_tamper_and_unknown_files_are_never_restored_or_unlinked() {
        let f = Fixture::new();
        let mut r = f.saved();
        let outside = f.root.join("outside");
        metadata::write_new(&outside, "do not delete").unwrap();
        let ram = state_path(&f.host, &r).join("memory-ranges");
        fs::remove_file(&ram).unwrap();
        symlink(&outside, &ram).unwrap();
        assert!(check_state_files(&f.host, &r).is_err());
        assert!(delete_locked(&f.host, &mut r, true).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "do not delete");
        assert!(fs::symlink_metadata(ram).unwrap().file_type().is_symlink());
    }
    #[test]
    fn partial_suspend_and_partial_delete_keep_reservations_until_explicit_cleanup() {
        let f = Fixture::new();
        let mut r = begin(
            &f.host,
            &f.config,
            "partial",
            Kind::Suspend,
            &"a".repeat(32),
            "test",
        )
        .unwrap();
        assert!(ensure_plain_start(&f.host, &f.config).is_err());
        assert!(restore_source(&f.host, &f.config).is_err());
        delete_locked(&f.host, &mut r, true).unwrap();
        let mut r = f.saved();
        // Simulate death after one payload was deleted but before the manifest update.
        let disk = &r.disks[0];
        driver(disk)
            .unwrap()
            .delete_snapshot(&disk.volume, &r.id, disk.identity.unwrap())
            .unwrap();
        delete_locked(&f.host, &mut r, true).unwrap();
        assert!(records(&f.host).unwrap().is_empty());
    }
}
