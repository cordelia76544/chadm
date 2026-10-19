//! OpenZFS datasets/zvols; all destructive commands target one verified object.
use super::{
    file::{check_file_with, FileDriver},
    StorageDriver, VolumeIdentity, VolumeStat,
};
use crate::{
    command,
    error::{Error, Result},
    metadata,
    repo::{Driver, RepoConfig},
    volume::VolumeConfig,
};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::AsRawFd,
        unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const BLOCK_SIZE: u64 = 16384;
const PROPERTIES: &str = "type,guid,referenced,mountpoint,mounted,canmount,readonly,volsize,volblocksize,volmode,chadm:managed-by,chadm:repo-id,chadm:kind,chadm:name,chadm:volume-id";
fn error(message: impl Into<String>) -> Error {
    Error::Resource(message.into())
}

pub fn validate_dataset(name: &str) -> Result<()> {
    if name.len() >= 256
        || name.split('/').count() < 2
        || name.split('/').any(|part| {
            part.is_empty()
                || part.len() > 63
                || !part.as_bytes()[0].is_ascii_alphabetic()
                || !part
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
        })
    {
        return Err(error("ZFS dataset must be a child path such as tank/chadm, with safe alphanumeric components; pool roots, snapshots and traversal are not accepted"));
    }
    Ok(())
}
pub fn aligned_size(size: u64) -> Result<u64> {
    let result = size
        .checked_add(BLOCK_SIZE - 1)
        .map(|n| n / BLOCK_SIZE * BLOCK_SIZE)
        .ok_or_else(|| error("ZFS volume size overflow"))?;
    crate::volume::validate_size(result)?;
    Ok(result)
}
pub(crate) fn require_root() -> Result<()> {
    if metadata::uid() != 0 {
        return Err(error("ZFS mutations require root"));
    }
    Ok(())
}
trait Commands {
    fn run(&self, args: &[String]) -> Result<String>;
}
struct System;
/// Resolve package/merged-usr symlinks while checking every directory and link.
/// Execute the checked final path, rather than the original alias.
fn trusted_executable(path: &Path, root: &Path, root_uid: u32) -> Result<PathBuf> {
    command::trusted_file(path, root, root_uid, true)
}

impl Commands for System {
    fn run(&self, args: &[String]) -> Result<String> {
        let program = command::executable(Path::new("zfs"))
            .ok_or_else(|| error("zfs executable was not found"))?;
        let root_uid = fs::metadata("/")
            .map_err(|e| metadata::io_error("inspect root ownership", Path::new("/"), e))?
            .uid();
        let program = trusted_executable(&program, Path::new("/"), root_uid)?;
        if !fs::symlink_metadata("/dev/zfs").is_ok_and(|m| m.file_type().is_char_device()) {
            return Err(error(
                "ZFS is unavailable: /dev/zfs character device is missing",
            ));
        }
        if args
            .first()
            .is_some_and(|arg| ["get", "list", "version"].contains(&arg.as_str()))
        {
            let refs: Vec<_> = args.iter().map(String::as_str).collect();
            command::network_output(&program, &refs)
                .map_err(|e| error(format!("cannot query ZFS with arguments {args:?}: {e}")))
        } else {
            require_root()?;
            let args: Vec<_> = args.iter().map(Into::into).collect();
            command::run_utility(&program, &args, Duration::from_secs(30))
                .map_err(|e| error(format!("ZFS operation failed: {e}")))?;
            Ok(String::new())
        }
    }
}
pub(crate) fn version() -> Result<String> {
    System.run(&["version".into()])
}
#[derive(Debug)]
struct Properties(HashMap<String, (String, String)>);
impl Properties {
    fn parse(dataset: &str, output: &str) -> Result<Self> {
        let mut values = HashMap::new();
        for line in output.lines() {
            let fields: Vec<_> = line.split('\t').collect();
            if fields.len() != 4
                || fields[0] != dataset
                || !PROPERTIES.split(',').any(|p| p == fields[1])
                || values
                    .insert(fields[1].into(), (fields[2].into(), fields[3].into()))
                    .is_some()
            {
                return Err(error("invalid or ambiguous ZFS property response"));
            }
        }
        if values.len() != PROPERTIES.split(',').count() {
            return Err(error("incomplete ZFS property response"));
        }
        Ok(Self(values))
    }
    fn value(&self, name: &str) -> Result<&str> {
        self.0
            .get(name)
            .map(|p| p.0.as_str())
            .ok_or_else(|| error("missing ZFS property"))
    }
    fn number(&self, name: &str) -> Result<u64> {
        self.value(name)?
            .parse()
            .map_err(|_| error(format!("invalid ZFS numeric property {name}")))
    }
    fn expect(&self, name: &str, value: &str) -> Result<()> {
        if self.value(name)? != value {
            return Err(error(format!(
                "ZFS property {name} does not match managed metadata"
            )));
        }
        Ok(())
    }
    fn local(&self, name: &str, value: &str) -> Result<()> {
        self.expect(name, value)?;
        if self.0.get(name).is_none_or(|p| p.1 != "local") {
            return Err(error(format!(
                "ZFS ownership property {name} must be local, not inherited"
            )));
        }
        Ok(())
    }
}
fn get(commands: &impl Commands, dataset: &str) -> Result<Properties> {
    Properties::parse(
        dataset,
        &commands.run(&[
            "get".into(),
            "-Hp".into(),
            "-o".into(),
            "name,property,value,source".into(),
            PROPERTIES.into(),
            dataset.into(),
        ])?,
    )
}
fn tree(commands: &impl Commands, dataset: &str) -> Result<Vec<String>> {
    let output = commands.run(&[
        "list".into(),
        "-H".into(),
        "-t".into(),
        "all".into(),
        "-o".into(),
        "name".into(),
        "-r".into(),
        dataset.into(),
    ])?;
    let mut names = Vec::new();
    for name in output.lines() {
        if name != dataset
            && !name.starts_with(&format!("{dataset}/"))
            && !name.starts_with(&format!("{dataset}@"))
            && !name.starts_with(&format!("{dataset}#"))
        {
            return Err(error(
                "ZFS list returned an object outside the requested dataset",
            ));
        }
        if names.iter().any(|n| n == name) {
            return Err(error("duplicate ZFS dataset response"));
        }
        names.push(name.into());
    }
    if !names.iter().any(|n| n == dataset) {
        return Err(error("ZFS dataset listing is incomplete"));
    }
    Ok(names)
}
fn owned(
    props: &Properties,
    repo: &RepoConfig,
    kind: &str,
    name: &str,
    guid: Option<u64>,
) -> Result<()> {
    props.local("chadm:managed-by", "chadm")?;
    props.local("chadm:repo-id", &repo.id)?;
    props.local("chadm:kind", kind)?;
    props.local("chadm:name", name)?;
    if guid.is_some_and(|g| props.number("guid").ok() != Some(g)) || props.number("guid")? == 0 {
        return Err(error("ZFS GUID does not match managed metadata"));
    }
    Ok(())
}
fn repo_dataset(repo: &RepoConfig) -> Result<&str> {
    if repo.driver != Driver::Zfs {
        return Err(error("repo does not use the ZFS driver"));
    }
    let dataset = repo
        .pool
        .as_deref()
        .ok_or_else(|| error("ZFS repo dataset is missing"))?;
    validate_dataset(dataset)?;
    metadata::validate_name("repo", &repo.name)?;
    if !metadata::valid_id(&repo.id) {
        return Err(error("invalid ZFS repo ownership ID"));
    }
    Ok(dataset)
}
fn container(repo: &RepoConfig) -> Result<String> {
    let name = format!("{}/volumes", repo_dataset(repo)?);
    validate_dataset(&name)?;
    Ok(name)
}
fn volume_dataset(repo: &RepoConfig, name: &str) -> Result<String> {
    metadata::validate_name("volume", name)?;
    let dataset = format!("{}/{}", container(repo)?, name);
    validate_dataset(&dataset)?;
    Ok(dataset)
}
fn verify_namespace(commands: &impl Commands, repo: &RepoConfig) -> Result<()> {
    let root = get(commands, repo_dataset(repo)?)?;
    owned(&root, repo, "repo", &repo.name, repo.zfs_guid)?;
    root.expect("type", "filesystem")?;
    root.local(
        "mountpoint",
        repo.path
            .to_str()
            .ok_or_else(|| error("ZFS mountpoint must be UTF-8"))?,
    )?;
    root.expect("readonly", "off")?;
    root.expect("canmount", "on")?;
    root.expect("mounted", "yes")?;
    let volumes = get(commands, &container(repo)?)?;
    owned(&volumes, repo, "volumes", "volumes", repo.volumes_guid)?;
    volumes.expect("type", "filesystem")?;
    volumes.local("mountpoint", "none")?;
    volumes.expect("canmount", "off")?;
    volumes.expect("mounted", "no")?;
    volumes.expect("readonly", "off")?;
    Ok(())
}
fn mounted_from(text: &str, dataset: &str, path: &Path) -> bool {
    let wanted = path.to_string_lossy();
    text.lines().any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        let Some(separator) = fields.iter().position(|f| *f == "-") else {
            return false;
        };
        fields.len() > separator + 2
            && fields.len() > 5
            && fields[separator + 1] == "zfs"
            && fields[separator + 2] == dataset
            && fields[4].replace("\\040", " ").replace("\\134", "\\") == wanted
            && fields[5].split(',').any(|o| o == "rw")
    })
}
fn verify_mount(repo: &RepoConfig) -> Result<()> {
    let mut text = String::new();
    File::open("/proc/self/mountinfo")
        .and_then(|file| file.take(1024 * 1024 + 1).read_to_string(&mut text))
        .map_err(|e| error(format!("cannot verify ZFS mount source: {e}")))?;
    if text.len() > 1024 * 1024 || !mounted_from(&text, repo_dataset(repo)?, &repo.path) {
        return Err(error(
            "repo mountpoint is not the expected writable ZFS filesystem",
        ));
    }
    Ok(())
}
pub(crate) fn verify_repo(repo: &RepoConfig) -> Result<()> {
    verify_namespace(&System, repo)?;
    verify_mount(repo)
}
pub(crate) fn ensure_volume_absent(repo: &RepoConfig, name: &str) -> Result<()> {
    let dataset = volume_dataset(repo, name)?;
    if tree(&System, &container(repo)?)?.iter().any(|n| {
        n == &dataset
            || n.starts_with(&format!("{dataset}/"))
            || n.starts_with(&format!("{dataset}@"))
            || n.starts_with(&format!("{dataset}#"))
    }) {
        return Err(error(format!(
            "ZFS volume {name:?} already exists; refusing to adopt it"
        )));
    }
    Ok(())
}
fn properties(repo: &RepoConfig, kind: &str, name: &str) -> Vec<String> {
    [
        "chadm:managed-by=chadm".to_string(),
        format!("chadm:repo-id={}", repo.id),
        format!("chadm:kind={kind}"),
        format!("chadm:name={name}"),
    ]
    .into_iter()
    .flat_map(|p| ["-o".into(), p])
    .collect()
}
pub(crate) fn preflight_repo(dataset: &str) -> Result<()> {
    validate_dataset(dataset)?;
    // Reserve room for the longest managed volume name before any native create.
    validate_dataset(&format!("{dataset}/volumes/{}", "v".repeat(63)))?;
    let parent = dataset
        .rsplit_once('/')
        .ok_or_else(|| error("pool roots cannot be managed repos"))?
        .0;
    get(&System, parent)?.expect("type", "filesystem")?;
    if tree(&System, parent)?
        .iter()
        .any(|name| name == dataset || name.starts_with(&format!("{dataset}/")))
    {
        return Err(error(
            "ZFS repo dataset already exists; refusing to adopt it",
        ));
    }
    Ok(())
}
pub(crate) fn create_root(repo: &RepoConfig) -> Result<u64> {
    require_root()?;
    let root = repo_dataset(repo)?;
    let parent = root
        .rsplit_once('/')
        .ok_or_else(|| error("pool roots cannot be managed repos"))?
        .0;
    let parent_props = get(&System, parent)?;
    parent_props.expect("type", "filesystem")?;
    if tree(&System, parent)?
        .iter()
        .any(|n| n == root || n.starts_with(&format!("{root}/")))
    {
        return Err(error(
            "ZFS repo dataset already exists; refusing to adopt it",
        ));
    }
    let mut args = vec!["create".into()];
    args.extend(properties(repo, "repo", &repo.name));
    for property in [
        format!("mountpoint={}", repo.path.display()),
        "canmount=on".into(),
        "readonly=off".into(),
        "sharenfs=off".into(),
        "sharesmb=off".into(),
        "devices=off".into(),
        "setuid=off".into(),
        "exec=off".into(),
    ] {
        args.extend(["-o".into(), property]);
    }
    args.push(root.into());
    System.run(&args)?;
    let props = get(&System, root)?;
    owned(&props, repo, "repo", &repo.name, None)?;
    verify_mount(repo)?;
    props.number("guid")
}
pub(crate) fn create_container(repo: &RepoConfig) -> Result<u64> {
    let mut args = vec!["create".into()];
    args.extend(properties(repo, "volumes", "volumes"));
    args.extend([
        "-o".into(),
        "mountpoint=none".into(),
        "-o".into(),
        "canmount=off".into(),
        "-o".into(),
        "readonly=off".into(),
        container(repo)?,
    ]);
    System.run(&args)?;
    let props = get(&System, &container(repo)?)?;
    owned(&props, repo, "volumes", "volumes", None)?;
    props.number("guid")
}
pub(crate) fn ensure_empty_repo(repo: &RepoConfig) -> Result<()> {
    let mut actual = tree(&System, repo_dataset(repo)?)?;
    actual.sort();
    let mut expected = vec![repo_dataset(repo)?.into(), container(repo)?];
    expected.sort();
    if actual != expected {
        return Err(error("ZFS repo contains volumes, snapshots, bookmarks or unmanaged descendants; refusing deletion"));
    }
    Ok(())
}
fn destroy_owned(
    commands: &impl Commands,
    repo: &RepoConfig,
    dataset: &str,
    kind: &str,
    name: &str,
    guid: u64,
    volume_id: Option<&str>,
) -> Result<()> {
    let props = get(commands, dataset)?;
    owned(&props, repo, kind, name, Some(guid))?;
    props.expect(
        "type",
        if kind == "volume" {
            "volume"
        } else {
            "filesystem"
        },
    )?;
    if let Some(id) = volume_id {
        props.local("chadm:volume-id", id)?;
    }
    if tree(commands, dataset)? != vec![dataset] {
        return Err(error(
            "ZFS object has snapshots, bookmarks or descendants; refusing non-recursive deletion",
        ));
    }
    commands.run(&["destroy".into(), dataset.into()])?;
    Ok(())
}
fn check_native_snapshot(
    commands: &impl Commands,
    repo: &RepoConfig,
    volume: &VolumeConfig,
    id: &str,
    identity: u64,
    restoring: bool,
) -> Result<()> {
    if !metadata::valid_id(id) || volume.repo != repo.name || volume.repo_id != repo.id {
        return Err(error("snapshot parent ownership differs"));
    }
    let dataset = volume_dataset(repo, &volume.name)?;
    let snapshot = format!("{dataset}@chadm-{id}");
    let props = get(commands, &snapshot)?;
    owned(&props, repo, "snapshot", id, Some(identity))?;
    props.local("chadm:volume-id", &volume.id)?;
    props.expect("type", "snapshot")?;
    if restoring {
        let output = commands.run(&[
            "list".into(),
            "-Hp".into(),
            "-t".into(),
            "snapshot,bookmark".into(),
            "-o".into(),
            "name".into(),
            "-s".into(),
            "createtxg".into(),
            "-r".into(),
            dataset.clone(),
        ])?;
        let names: Vec<_> = output.lines().collect();
        let prefix = format!("{dataset}@");
        if names
            .iter()
            .any(|name| !name.starts_with(&prefix) || name.len() == prefix.len())
            || names.last().copied() != Some(snapshot.as_str())
        {
            return Err(error("ZFS rollback requires the latest snapshot and no bookmarks; remove conflicting objects explicitly first"));
        }
    }
    Ok(())
}
fn native_snapshot_action(
    commands: &impl Commands,
    repo: &RepoConfig,
    volume: &VolumeConfig,
    id: &str,
    identity: u64,
    restore: bool,
) -> Result<()> {
    check_native_snapshot(commands, repo, volume, id, identity, restore)?;
    commands.run(&[
        if restore { "rollback" } else { "destroy" }.into(),
        format!("{}@chadm-{id}", volume_dataset(repo, &volume.name)?),
    ])?;
    Ok(())
}
pub(crate) fn delete_repo(repo: &RepoConfig) -> Result<()> {
    require_root()?;
    ensure_empty_repo(repo)?;
    verify_repo(repo)?;
    destroy_owned(
        &System,
        repo,
        &container(repo)?,
        "volumes",
        "volumes",
        repo.volumes_guid
            .ok_or_else(|| error("missing volumes GUID"))?,
        None,
    )?;
    destroy_owned(
        &System,
        repo,
        repo_dataset(repo)?,
        "repo",
        &repo.name,
        repo.zfs_guid.ok_or_else(|| error("missing repo GUID"))?,
        None,
    )
}
pub(crate) fn rollback_repo(repo: &RepoConfig) -> Result<()> {
    // A missing GUID means creation result is uncertain; never guess ownership.
    if repo.zfs_guid.is_none() {
        return Err(error(
            "ZFS creation result is uncertain; retain the repo intent",
        ));
    }
    verify_mount(repo)?;
    for entry in fs::read_dir(&repo.path)
        .map_err(|e| metadata::io_error("inspect rollback repo", &repo.path, e))?
    {
        let entry =
            entry.map_err(|e| metadata::io_error("inspect rollback entry", &repo.path, e))?;
        let name = entry.file_name();
        if name == "images" || name == "volumes" {
            metadata::directory(&entry.path(), false)?;
            if fs::read_dir(entry.path())
                .map_err(|e| metadata::io_error("inspect rollback directory", &entry.path(), e))?
                .next()
                .is_some()
            {
                return Err(error("ZFS rollback repo contains payload or unknown files; retaining ownership evidence"));
            }
        } else if name != ".chadm-repo.toml" {
            return Err(error(
                "ZFS rollback repo contains unknown files; retaining ownership evidence",
            ));
        }
    }
    if let Some(guid) = repo.volumes_guid {
        destroy_owned(
            &System,
            repo,
            &container(repo)?,
            "volumes",
            "volumes",
            guid,
            None,
        )?;
    }
    destroy_owned(
        &System,
        repo,
        repo_dataset(repo)?,
        "repo",
        &repo.name,
        repo.zfs_guid
            .ok_or_else(|| error("ZFS creation uncertain; dataset ownership must be inspected"))?,
        None,
    )
}

pub struct ZfsDriver {
    repo: RepoConfig,
}
impl ZfsDriver {
    fn snapshot_name(&self, volume: &VolumeConfig, id: &str) -> Result<String> {
        if !metadata::valid_id(id) {
            return Err(error("invalid ZFS snapshot generation"));
        }
        Ok(format!("{}@chadm-{id}", self.dataset(volume)?))
    }
    pub fn new(repo: RepoConfig) -> Result<Self> {
        if repo.driver != Driver::Zfs {
            return Err(error("ZFS driver requires a ZFS repo"));
        }
        crate::repo::verify(&repo)?;
        Ok(Self { repo })
    }
    fn files(&self) -> FileDriver {
        FileDriver {
            repo: self.repo.clone(),
        }
    }
    fn dataset(&self, volume: &VolumeConfig) -> Result<String> {
        if volume.repo != self.repo.name
            || volume.repo_id != self.repo.id
            || !metadata::valid_id(&volume.id)
        {
            return Err(error("zvol does not belong to this repo"));
        }
        volume_dataset(&self.repo, &volume.name)
    }
    fn checked_properties(&self, volume: &VolumeConfig) -> Result<Properties> {
        let props = get(&System, &self.dataset(volume)?)?;
        let guid = volume
            .zfs_guid
            .ok_or_else(|| error("zvol GUID is missing"))?;
        owned(&props, &self.repo, "volume", &volume.name, Some(guid))?;
        props.local("chadm:volume-id", &volume.id)?;
        props.expect("type", "volume")?;
        props.expect("volsize", &volume.size_bytes.to_string())?;
        props.expect("volblocksize", &BLOCK_SIZE.to_string())?;
        props.expect("readonly", "off")?;
        props.expect("volmode", "dev")?;
        Ok(props)
    }
    fn make_volume(
        &self,
        volume: &VolumeConfig,
        source: Option<(&crate::image::ImageConfig, &Path)>,
    ) -> Result<VolumeIdentity> {
        require_root()?;
        crate::volume::validate_size(volume.size_bytes)?;
        if volume.size_bytes % BLOCK_SIZE != 0 {
            return Err(error("ZFS volume size must be a multiple of 16 KiB"));
        }
        ensure_volume_absent(&self.repo, &volume.name)?;
        let dataset = self.dataset(volume)?;
        let mut args = vec![
            "create".into(),
            "-V".into(),
            volume.size_bytes.to_string(),
            "-b".into(),
            BLOCK_SIZE.to_string(),
        ];
        args.extend(properties(&self.repo, "volume", &volume.name));
        args.extend([
            "-o".into(),
            format!("chadm:volume-id={}", volume.id),
            "-o".into(),
            "volmode=dev".into(),
            "-o".into(),
            "readonly=off".into(),
            dataset.clone(),
        ]);
        System.run(&args)?;
        let props = get(&System, &dataset)?;
        owned(&props, &self.repo, "volume", &volume.name, None)?;
        props.local("chadm:volume-id", &volume.id)?;
        let guid = props.number("guid")?;
        let mut completed = volume.clone();
        completed.inode = 0;
        completed.zfs_guid = Some(guid);
        let result = (|| {
            self.checked_properties(&completed)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            let mut output = loop {
                match self.open_device(&completed, true) {
                    Ok(file) => break file,
                    Err(Error::Io { source, .. })
                        if (source.kind() == std::io::ErrorKind::NotFound
                            || source.raw_os_error() == Some(6))
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(100))
                    }
                    Err(e) => return Err(e),
                }
            };
            if let Some((image, source)) = source {
                if volume.size_bytes < image.virtual_size_bytes {
                    return Err(error("root zvol is smaller than source image"));
                }
                let input = metadata::open_regular(source, false)?;
                check_file_with(&input, &image.id, c"user.chadm.image-id")?;
                let stat = input
                    .metadata()
                    .map_err(|e| metadata::io_error("inspect clone source", source, e))?;
                if stat.ino() != image.inode
                    || stat.len() != image.virtual_size_bytes
                    || stat.mode() & 0o222 != 0
                {
                    return Err(error("source image changed before zvol copy"));
                }
                let copied = std::io::copy(&mut input.take(image.virtual_size_bytes), &mut output)
                    .map_err(|e| error(format!("copy image into zvol failed: {e}")))?;
                if copied != image.virtual_size_bytes {
                    return Err(error("short image copy into zvol"));
                }
                output
                    .flush()
                    .and_then(|_| output.sync_all())
                    .map_err(|e| error(format!("sync zvol failed: {e}")))?;
            }
            self.checked_properties(&completed)?;
            Ok(VolumeIdentity {
                inode: 0,
                zfs_guid: Some(guid),
            })
        })();
        match result {
            Ok(identity) => Ok(identity),
            Err(e) => match self.delete_volume(&completed) {
                Ok(()) => Err(e),
                Err(cleanup) => Err(error(format!(
                    "{e}; zvol rollback incomplete: {cleanup}; keep transaction evidence"
                ))),
            },
        }
    }
    fn open_device(&self, volume: &VolumeConfig, write: bool) -> Result<File> {
        let path = self.get_volume_path(volume)?;
        metadata::directory(
            path.parent().ok_or_else(|| error("invalid zvol path"))?,
            false,
        )?;
        let link = fs::symlink_metadata(&path)
            .map_err(|e| metadata::io_error("inspect zvol link", &path, e))?;
        if !link.file_type().is_symlink() || link.uid() != 0 {
            return Err(error("zvol path must be a root-owned device symlink"));
        }
        let target = fs::canonicalize(&path)
            .map_err(|e| metadata::io_error("resolve zvol device", &path, e))?;
        let name = target.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if target.parent() != Some(Path::new("/dev"))
            || !name.strip_prefix("zd").is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|c| c.is_ascii_digit())
            })
        {
            return Err(error(
                "zvol link does not point to a whole ZFS block device",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(write)
            .custom_flags(0x20000 | 0x800 | 0x80000)
            .open(&target)
            .map_err(|e| metadata::io_error("open zvol device", &target, e))?;
        let stat = file
            .metadata()
            .map_err(|e| metadata::io_error("inspect zvol device", &target, e))?;
        if !stat.file_type().is_block_device() || stat.uid() != 0 {
            return Err(error("zvol target is not a root-owned block device"));
        }
        unsafe extern "C" {
            fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
        }
        let mut native_name = [0u8; 256];
        let mut size = 0u64;
        // SAFETY: BLKZNAME = _IOR(0x12,125,char[256]), confirmed in installed OpenZFS headers;
        // BLKGETSIZE64 writes one u64. Both use the same pinned block-device fd before any write.
        if unsafe { ioctl(file.as_raw_fd(), 0x8100127d, native_name.as_mut_ptr()) } != 0
            || unsafe { ioctl(file.as_raw_fd(), 0x80081272, &mut size) } != 0
        {
            return Err(error(
                "cannot verify native zvol name and capacity; refusing block-device access",
            ));
        }
        let end = native_name
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| error("invalid native zvol name"))?;
        if native_name[..end] != *self.dataset(volume)?.as_bytes() || size != volume.size_bytes {
            return Err(error(
                "native zvol device does not match dataset or capacity",
            ));
        }
        Ok(file)
    }
}
impl StorageDriver for ZfsDriver {
    fn snapshot_exists(&self, volume: &VolumeConfig, id: &str) -> Result<bool> {
        Ok(tree(&System, &self.dataset(volume)?)?.contains(&self.snapshot_name(volume, id)?))
    }
    fn snapshot_volume(&self, volume: &VolumeConfig, id: &str) -> Result<u64> {
        self.check_volume(volume)?;
        self.open_device(volume, false)?
            .sync_all()
            .map_err(|e| error(format!("sync zvol before snapshot: {e}")))?;
        let snapshot = self.snapshot_name(volume, id)?;
        if tree(&System, &self.dataset(volume)?)?.contains(&snapshot) {
            return Err(error("ZFS snapshot already exists; refusing adoption"));
        }
        let mut args = vec!["snapshot".into()];
        args.extend(properties(&self.repo, "snapshot", id));
        args.extend([
            "-o".into(),
            format!("chadm:volume-id={}", volume.id),
            snapshot.clone(),
        ]);
        System.run(&args)?;
        let props = get(&System, &snapshot)?;
        owned(&props, &self.repo, "snapshot", id, None)?;
        props.local("chadm:volume-id", &volume.id)?;
        props.expect("type", "snapshot")?;
        props.number("guid")
    }
    fn check_snapshot(
        &self,
        volume: &VolumeConfig,
        id: &str,
        identity: u64,
        restoring: bool,
    ) -> Result<()> {
        self.check_volume(volume)?;
        check_native_snapshot(&System, &self.repo, volume, id, identity, restoring)
    }

    fn restore_snapshot(&self, volume: &VolumeConfig, id: &str, identity: u64) -> Result<()> {
        self.check_volume(volume)?;
        native_snapshot_action(&System, &self.repo, volume, id, identity, true)?;
        self.check_volume(volume).map(|_| ())
    }
    fn delete_snapshot(&self, volume: &VolumeConfig, id: &str, identity: u64) -> Result<()> {
        self.check_volume(volume)?;
        native_snapshot_action(&System, &self.repo, volume, id, identity, false)
    }
    fn import_image(&self, image: &crate::image::ImageConfig, source: &Path) -> Result<u64> {
        self.files().import_image(image, source)
    }
    fn get_image_path(&self, image: &crate::image::ImageConfig) -> Result<PathBuf> {
        self.files().get_image_path(image)
    }
    fn check_image(&self, image: &crate::image::ImageConfig) -> Result<VolumeStat> {
        self.files().check_image(image)
    }
    fn delete_image(&self, image: &crate::image::ImageConfig) -> Result<()> {
        self.files().delete_image(image)
    }
    fn clone_image(
        &self,
        volume: &VolumeConfig,
        image: &crate::image::ImageConfig,
        source: &Path,
    ) -> Result<VolumeIdentity> {
        self.make_volume(volume, Some((image, source)))
    }
    fn create_volume(&self, volume: &VolumeConfig) -> Result<VolumeIdentity> {
        self.make_volume(volume, None)
    }
    fn volume_exists(&self, volume: &VolumeConfig) -> Result<bool> {
        let dataset = self.dataset(volume)?;
        Ok(tree(&System, &container(&self.repo)?)?
            .iter()
            .any(|n| n == &dataset))
    }
    fn volume_marker_path(&self, volume: &VolumeConfig) -> Result<PathBuf> {
        self.dataset(volume)?;
        Ok(self
            .repo
            .path
            .join("volumes")
            .join(format!("{}.chadm.toml", volume.name)))
    }
    fn sync_volume_storage(&self) -> Result<()> {
        metadata::sync_directory(&self.repo.path.join("volumes"))
    }
    fn get_volume_path(&self, volume: &VolumeConfig) -> Result<PathBuf> {
        Ok(Path::new("/dev/zvol").join(self.dataset(volume)?))
    }
    fn check_volume(&self, volume: &VolumeConfig) -> Result<VolumeStat> {
        let props = self.checked_properties(volume)?;
        self.open_device(volume, false)?;
        Ok(VolumeStat {
            allocated_bytes: props.number("referenced")?,
        })
    }
    fn delete_volume(&self, volume: &VolumeConfig) -> Result<()> {
        require_root()?;
        self.checked_properties(volume)?;
        destroy_owned(
            &System,
            &self.repo,
            &self.dataset(volume)?,
            "volume",
            &volume.name,
            volume
                .zfs_guid
                .ok_or_else(|| error("zvol GUID is missing"))?,
            Some(&volume.id),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    struct ExecutableFixture(PathBuf);
    impl ExecutableFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "chadm-zfs-executable-{}",
                metadata::random_id().unwrap()
            ));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            for dir in ["usr", "usr/bin", "usr/sbin"] {
                fs::DirBuilder::new()
                    .mode(0o755)
                    .create(root.join(dir))
                    .unwrap();
            }
            fs::write(
                root.join("usr/sbin/zfs"),
                b"test executable, never executed",
            )
            .unwrap();
            fs::set_permissions(root.join("usr/sbin/zfs"), fs::Permissions::from_mode(0o755))
                .unwrap();
            std::os::unix::fs::symlink("../sbin/zfs", root.join("usr/bin/zfs")).unwrap();
            std::os::unix::fs::symlink("usr/bin", root.join("bin")).unwrap();
            Self(root)
        }
        fn resolve(&self, path: &str) -> Result<PathBuf> {
            // This fixture acts as the filesystem root; no privileged chown is needed.
            trusted_executable(&self.0.join(path), &self.0, metadata::uid())
        }
    }
    impl Drop for ExecutableFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn packaged_zfs_and_merged_usr_symlinks_resolve_to_the_checked_binary() {
        let f = ExecutableFixture::new();
        let expected = f.0.join("usr/sbin/zfs");
        assert_eq!(f.resolve("usr/bin/zfs").unwrap(), expected);
        assert_eq!(f.resolve("bin/zfs").unwrap(), expected);
        assert_eq!(f.resolve("usr/sbin/zfs").unwrap(), expected);
        std::os::unix::fs::symlink(&expected, f.0.join("absolute")).unwrap();
        assert_eq!(f.resolve("absolute").unwrap(), expected);
    }

    #[test]
    fn executable_resolution_rejects_writable_paths_bad_targets_and_link_loops() {
        let f = ExecutableFixture::new();
        let binary = f.0.join("usr/sbin/zfs");
        for mode in [0o777, 0o644] {
            fs::set_permissions(&binary, fs::Permissions::from_mode(mode)).unwrap();
            assert!(f.resolve("usr/bin/zfs").is_err());
        }
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        for ancestor in ["usr/bin", "usr/sbin"] {
            fs::set_permissions(f.0.join(ancestor), fs::Permissions::from_mode(0o777)).unwrap();
            assert!(f.resolve("usr/bin/zfs").is_err());
            fs::set_permissions(f.0.join(ancestor), fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::os::unix::fs::symlink("missing", f.0.join("dangling")).unwrap();
        std::os::unix::fs::symlink("loop", f.0.join("loop")).unwrap();
        std::os::unix::fs::symlink("../outside", f.0.join("escape")).unwrap();
        assert!(f.resolve("dangling").is_err());
        assert!(f.resolve("loop").is_err());
        assert!(f.resolve("escape").is_err());
        assert!(f.resolve("usr/sbin").is_err());
        assert!(trusted_executable(&binary, &f.0, metadata::uid().wrapping_add(1)).is_err());
    }

    #[test]
    fn installed_zfs_path_is_checked_without_executing_it_or_accessing_the_pool() {
        let Some(program) = command::executable(Path::new("zfs")) else {
            return;
        };
        let uid = fs::symlink_metadata("/").unwrap().uid();
        let resolved = trusted_executable(&program, Path::new("/"), uid).unwrap();
        assert_eq!(resolved, fs::canonicalize(program).unwrap());
        assert!(fs::symlink_metadata(resolved).unwrap().is_file());
    }
    fn repo() -> RepoConfig {
        RepoConfig {
            schema_version: 1,
            managed_by: "chadm".into(),
            id: "a".repeat(32),
            name: "fast".into(),
            driver: Driver::Zfs,
            kind: crate::repo::RepoKind::Storage,
            path: "/var/lib/chadm/zfs-repos/fast".into(),
            pool: Some("tank/chadm".into()),
            zfs_guid: Some(1),
            volumes_guid: Some(2),
        }
    }
    fn property_text(dataset: &str, kind: &str, name: &str, guid: u64) -> String {
        let repo = repo();
        PROPERTIES
            .split(',')
            .map(|property| {
                let value = match property {
                    "type" => {
                        if kind == "volume" {
                            "volume".into()
                        } else {
                            "filesystem".into()
                        }
                    }
                    "guid" => guid.to_string(),
                    "referenced" => "4096".into(),
                    "mountpoint" => {
                        if kind == "repo" {
                            repo.path.display().to_string()
                        } else {
                            "none".into()
                        }
                    }
                    "mounted" => {
                        if kind == "repo" {
                            "yes".into()
                        } else {
                            "no".into()
                        }
                    }
                    "canmount" => {
                        if kind == "repo" {
                            "on".into()
                        } else {
                            "off".into()
                        }
                    }
                    "readonly" => "off".into(),
                    "volsize" => "1048576".into(),
                    "volblocksize" => "16384".into(),
                    "volmode" => "dev".into(),
                    "chadm:managed-by" => "chadm".into(),
                    "chadm:repo-id" => repo.id.clone(),
                    "chadm:kind" => kind.into(),
                    "chadm:name" => name.into(),
                    "chadm:volume-id" => "b".repeat(32),
                    _ => unreachable!(),
                };
                format!(
                    "{dataset}\t{property}\t{value}\t{}\n",
                    if property.starts_with("chadm:") || property == "mountpoint" {
                        "local"
                    } else {
                        "-"
                    }
                )
            })
            .collect::<Vec<_>>()
            .join("")
    }
    struct Fake {
        props: HashMap<String, String>,
        names: HashMap<String, String>,
        calls: RefCell<Vec<Vec<String>>>,
    }
    impl Fake {
        fn new() -> Self {
            Self {
                props: HashMap::new(),
                names: HashMap::new(),
                calls: RefCell::new(Vec::new()),
            }
        }
        fn volume() -> Self {
            let mut fake = Self::new();
            fake.props.insert(
                "tank/chadm/volumes/data".into(),
                property_text("tank/chadm/volumes/data", "volume", "data", 3),
            );
            fake.names.insert(
                "tank/chadm/volumes/data".into(),
                "tank/chadm/volumes/data\n".into(),
            );
            fake
        }
        fn destroyed(&self) -> bool {
            self.calls
                .borrow()
                .iter()
                .any(|args| args.first().is_some_and(|arg| arg == "destroy"))
        }
    }
    impl Commands for Fake {
        fn run(&self, args: &[String]) -> Result<String> {
            self.calls.borrow_mut().push(args.to_vec());
            match args[0].as_str() {
                "get" => self
                    .props
                    .get(args.last().unwrap())
                    .cloned()
                    .ok_or_else(|| error("query failed")),
                "list" => self
                    .names
                    .get(args.last().unwrap())
                    .cloned()
                    .ok_or_else(|| error("query failed")),
                "destroy" | "rollback" => Ok(String::new()),
                _ => Err(error("unexpected command")),
            }
        }
    }
    #[test]
    fn dataset_names_never_address_pool_roots_snapshots_or_shell_options() {
        for name in [
            "tank",
            "/tank/chadm",
            "tank/../chadm",
            "tank/./chadm",
            "tank//chadm",
            "tank/chadm@snap",
            "tank/chadm#bookmark",
            "tank/chadm;reboot",
            "-r/tank",
            "tank/ch adm",
            "tank/chadm\nother",
            "tank/chadm%part",
        ] {
            assert!(validate_dataset(name).is_err(), "{name}");
        }
        assert!(validate_dataset("tank/chadm").is_ok());
        assert!(validate_dataset("Tank_01/VM-Store.test").is_ok());
        assert!(aligned_size(i64::MAX as u64).is_err());
        assert_eq!(aligned_size(16385).unwrap(), 32768);
        assert_eq!(aligned_size(16384).unwrap(), 16384);
    }
    #[test]
    fn native_snapshot_actions_require_guid_local_owner_and_latest_rollback_without_force() {
        let repo = repo();
        let id = "d".repeat(32);
        let dataset = "tank/chadm/volumes/data";
        let snapshot = format!("{dataset}@chadm-{id}");
        let volume = VolumeConfig {
            schema_version: 1,
            managed_by: "chadm".into(),
            id: "b".repeat(32),
            name: "data".into(),
            repo: repo.name.clone(),
            repo_id: repo.id.clone(),
            size_bytes: 1 << 20,
            inode: 0,
            zfs_guid: Some(3),
            source_image: None,
            source_image_id: None,
        };
        let valid = property_text(&snapshot, "snapshot", &id, u64::MAX)
            .replace("\ttype\tfilesystem\t", "\ttype\tsnapshot\t");
        let mut fake = Fake::new();
        fake.props.insert(snapshot.clone(), valid.clone());
        fake.names.insert(dataset.into(), format!("{snapshot}\n"));
        native_snapshot_action(&fake, &repo, &volume, &id, u64::MAX, true).unwrap();
        assert_eq!(
            fake.calls.borrow().last().unwrap(),
            &vec!["rollback".to_string(), snapshot.clone()]
        );
        native_snapshot_action(&fake, &repo, &volume, &id, u64::MAX, false).unwrap();
        assert_eq!(
            fake.calls.borrow().last().unwrap(),
            &vec!["destroy".to_string(), snapshot.clone()]
        );
        fake.names.insert(
            dataset.into(),
            format!("{snapshot}\n{dataset}@foreign-newer\n"),
        );
        assert!(native_snapshot_action(&fake, &repo, &volume, &id, u64::MAX, true).is_err());
        for bad in [
            valid.replace(&format!("\tguid\t{}\t-", u64::MAX), "\tguid\t1\t-"),
            valid.replace(
                "\tchadm:kind\tsnapshot\tlocal",
                "\tchadm:kind\tsnapshot\tinherited from tank",
            ),
            valid.replace(
                &format!("\tchadm:volume-id\t{}\tlocal", volume.id),
                "\tchadm:volume-id\tforeign\tlocal",
            ),
        ] {
            fake.props.insert(snapshot.clone(), bad);
            assert!(native_snapshot_action(&fake, &repo, &volume, &id, u64::MAX, false).is_err());
        }
        assert!(!fake.calls.borrow().iter().any(|args| args
            .iter()
            .any(|arg| ["-r", "-R", "-f"].contains(&arg.as_str()))
            && ["rollback", "destroy"].contains(&args[0].as_str())));
    }
    #[test]
    fn properties_require_exact_dataset_and_complete_unambiguous_fields() {
        let text = property_text("tank/chadm", "repo", "fast", 1);
        assert!(Properties::parse("tank/chadm", &text).is_ok());
        assert!(Properties::parse("tank/foreign", &text).is_err());
        assert!(
            Properties::parse("tank/chadm", &(text.clone() + "tank/chadm\tguid\t1\t-\n")).is_err()
        );
        assert!(Properties::parse(
            "tank/chadm",
            &text.lines().skip(1).collect::<Vec<_>>().join("\n")
        )
        .is_err());
        let bad = text.replace("\tguid\t1\t-", "\tguid\tnan\t-");
        assert!(owned(
            &Properties::parse("tank/chadm", &bad).unwrap(),
            &repo(),
            "repo",
            "fast",
            Some(1)
        )
        .is_err());
    }
    #[test]
    fn native_guid_local_markers_and_volume_id_gate_all_destruction() {
        let dataset = "tank/chadm/volumes/data";
        let valid = property_text(dataset, "volume", "data", 3);
        for text in [
            valid.replace("\tguid\t3\t-", "\tguid\t4\t-"),
            valid.replace("chadm:repo-id\t", "chadm:repo-id\twrong-"),
            valid.replace(
                "\tchadm:kind\tvolume\tlocal",
                "\tchadm:kind\tforeign\tlocal",
            ),
            valid.replace("\tchadm:name\tdata\tlocal", "\tchadm:name\tother\tlocal"),
            valid.replace(
                "\tchadm:managed-by\tchadm\tlocal",
                "\tchadm:managed-by\tchadm\tinherited from tank",
            ),
            valid.replace(
                &format!("\tchadm:volume-id\t{}\tlocal", "b".repeat(32)),
                &format!("\tchadm:volume-id\t{}\tlocal", "c".repeat(32)),
            ),
        ] {
            let mut fake = Fake::volume();
            fake.props.insert(dataset.into(), text);
            assert!(destroy_owned(
                &fake,
                &repo(),
                dataset,
                "volume",
                "data",
                3,
                Some(&"b".repeat(32))
            )
            .is_err());
            assert!(!fake.destroyed());
        }
        let fake = Fake::volume();
        destroy_owned(
            &fake,
            &repo(),
            dataset,
            "volume",
            "data",
            3,
            Some(&"b".repeat(32)),
        )
        .unwrap();
        assert_eq!(
            fake.calls.borrow().last().unwrap(),
            &vec!["destroy".to_string(), dataset.to_string()]
        );
    }
    #[test]
    fn snapshots_bookmarks_children_and_query_failure_are_never_destroyed_recursively() {
        let dataset = "tank/chadm/volumes/data";
        for suffix in ["@snap", "#bookmark", "/foreign"] {
            let mut fake = Fake::volume();
            fake.names
                .insert(dataset.into(), format!("{dataset}\n{dataset}{suffix}\n"));
            assert!(destroy_owned(
                &fake,
                &repo(),
                dataset,
                "volume",
                "data",
                3,
                Some(&"b".repeat(32))
            )
            .is_err());
            assert!(!fake.destroyed());
        }
        let mut fake = Fake::volume();
        fake.names.clear();
        assert!(destroy_owned(
            &fake,
            &repo(),
            dataset,
            "volume",
            "data",
            3,
            Some(&"b".repeat(32))
        )
        .is_err());
        assert!(!fake.destroyed());
        fake.names.insert(dataset.into(), "tank/foreign\n".into());
        assert!(tree(&fake, dataset).is_err());
    }
    #[test]
    fn namespace_requires_both_container_guids_and_expected_mount_policy() {
        let mut fake = Fake::new();
        fake.props.insert(
            "tank/chadm".into(),
            property_text("tank/chadm", "repo", "fast", 1),
        );
        fake.props.insert(
            "tank/chadm/volumes".into(),
            property_text("tank/chadm/volumes", "volumes", "volumes", 2),
        );
        verify_namespace(&fake, &repo()).unwrap();
        let text = fake
            .props
            .get("tank/chadm/volumes")
            .unwrap()
            .replace("\tguid\t2\t-", "\tguid\t99\t-");
        fake.props.insert("tank/chadm/volumes".into(), text);
        assert!(verify_namespace(&fake, &repo()).is_err());
        fake.props.insert(
            "tank/chadm/volumes".into(),
            property_text("tank/chadm/volumes", "volumes", "volumes", 2)
                .replace("\tmountpoint\tnone\tlocal", "\tmountpoint\t/foreign\tlocal"),
        );
        assert!(verify_namespace(&fake, &repo()).is_err());
    }
    #[test]
    fn kernel_mount_source_must_match_zfs_dataset_and_writable_mountpoint() {
        let text =
            "42 1 0:45 / /var/lib/chadm/zfs-repos/fast rw,relatime - zfs tank/chadm rw,xattr\n";
        assert!(mounted_from(
            text,
            "tank/chadm",
            Path::new("/var/lib/chadm/zfs-repos/fast")
        ));
        assert!(!mounted_from(
            text,
            "tank/foreign",
            Path::new("/var/lib/chadm/zfs-repos/fast")
        ));
        assert!(!mounted_from(
            &text.replace(" - zfs ", " - ext4 "),
            "tank/chadm",
            Path::new("/var/lib/chadm/zfs-repos/fast")
        ));
        assert!(!mounted_from(
            &text.replace(" rw,relatime ", " ro,relatime "),
            "tank/chadm",
            Path::new("/var/lib/chadm/zfs-repos/fast")
        ));
        assert!(mounted_from(
            "42 1 0:45 / /test\\040dir rw - zfs tank/chadm rw",
            "tank/chadm",
            Path::new("/test dir")
        ));
    }
}
