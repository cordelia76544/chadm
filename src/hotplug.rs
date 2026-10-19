//! Disk hotplug with durable reservation, authenticated API and guest eject confirmation.
use crate::{
    api_json,
    config::HostConfig,
    error::{Error, Result},
    hypervisor,
    instance::{self, DiskConfig, InstanceConfig},
    lifecycle, metadata,
    output::json_string,
    systemd::Snapshot,
};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

fn error(text: impl Into<String>) -> Error {
    Error::Resource(text.into())
}
const JOURNAL: &str = "hotplug.toml";
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Intent {
    managed_by: String,
    instance_id: String,
    instance: String,
    invocation: String,
    attach: bool,
    volume: String,
    volume_id: String,
    device_id: String,
    path: PathBuf,
    before: Vec<DiskConfig>,
    after: Vec<DiskConfig>,
}
fn journal(host: &HostConfig, config: &InstanceConfig) -> PathBuf {
    host.layout().instances.join(&config.name).join(JOURNAL)
}
fn valid_device_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || [b'-', b'_'].contains(&b))
}
pub(crate) fn pending(host: &HostConfig, config: &InstanceConfig) -> Result<Option<Intent>> {
    let path = journal(host, config);
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(metadata::io_error("inspect hotplug intent", &path, e)),
        Ok(_) => (),
    }
    let intent: Intent = toml::from_str(&metadata::read(&path)?)
        .map_err(|_| error("invalid hotplug intent TOML; preserve it for inspection"))?;
    validate_intent(config, &intent)?;
    Ok(Some(intent))
}
fn validate_intent(config: &InstanceConfig, intent: &Intent) -> Result<()> {
    metadata::validate_name("volume", &intent.volume)?;
    metadata::validate_path(&intent.path)?;
    if intent.managed_by != "chadm"
        || intent.instance != config.name
        || intent.instance_id != config.id
        || !metadata::valid_id(&intent.invocation)
        || !metadata::valid_id(&intent.volume_id)
        || !valid_device_id(&intent.device_id)
        || (config.disk != intent.before && config.disk != intent.after)
    {
        return Err(error(
            "hotplug intent ownership or configuration differs; refusing operation",
        ));
    }
    let (larger, smaller) = if intent.attach {
        (&intent.after, &intent.before)
    } else {
        (&intent.before, &intent.after)
    };
    let index = larger
        .iter()
        .position(|d| d.volume == intent.volume && d.volume_id == intent.volume_id)
        .ok_or_else(|| error("hotplug intent volume mismatch"))?;
    if index == 0 || larger[index].name == "root" {
        return Err(error("root volume cannot be hotplugged"));
    }
    let mut expected = larger.clone();
    expected.remove(index);
    if &expected != smaller {
        return Err(error("hotplug intent changes more than one data volume"));
    }
    for disks in [&intent.before, &intent.after] {
        let mut candidate = config.clone();
        candidate.disk = disks.clone();
        crate::instance_config::validate(&candidate)?;
    }
    Ok(())
}
/// Include both sides in reference checks until an uncertain operation is resolved.
pub(crate) fn reserve(host: &HostConfig, config: &mut InstanceConfig) -> Result<()> {
    if let Some(intent) = pending(host, config)? {
        for disk in intent.before.into_iter().chain(intent.after) {
            if !config.disk.contains(&disk) {
                config.disk.push(disk);
            }
        }
    }
    Ok(())
}
pub(crate) fn ensure_no_pending(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    if pending(host, config)?.is_some() {
        return Err(error("instance has an unfinished disk hotplug; retry the same attach/detach, or stop it and run instance recover"));
    }
    Ok(())
}
fn clear(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    let path = journal(host, config);
    fs::remove_file(&path)
        .map_err(|e| metadata::io_error("remove completed hotplug intent", &path, e))?;
    metadata::sync_directory(
        path.parent()
            .ok_or_else(|| error("invalid hotplug journal path"))?,
    )
}
pub(crate) fn recover_stopped(host: &HostConfig, config: &InstanceConfig) -> Result<()> {
    if pending(host, config)?.is_some() {
        lifecycle::check_cold_edit(host, config)?;
        // Keep the last published configuration. The stopped VMM cannot still
        // hold the additional disk; reservations can now be released safely.
        clear(host, config)?;
    }
    Ok(())
}

trait DeviceApi {
    fn info(&self) -> Result<Vec<u8>>;
    fn mutate(&self, endpoint: &str, body: &str) -> Result<()>;
    fn check(&self) -> Result<()>;
}
struct Live<'a> {
    host: &'a HostConfig,
    config: &'a InstanceConfig,
    snapshot: &'a Snapshot,
}
impl DeviceApi for Live<'_> {
    fn info(&self) -> Result<Vec<u8>> {
        self.check()?;
        hypervisor::request(
            &self
                .host
                .runtime_dir
                .join(&self.config.name)
                .join("api.sock"),
            self.snapshot.pid,
            "GET",
            "vm.info",
        )
    }
    fn mutate(&self, endpoint: &str, body: &str) -> Result<()> {
        self.check()?;
        hypervisor::request_body(
            &self
                .host
                .runtime_dir
                .join(&self.config.name)
                .join("api.sock"),
            self.snapshot.pid,
            endpoint,
            body,
        )
        .map(|_| ())
    }
    fn check(&self) -> Result<()> {
        lifecycle::recheck_hotplug(self.host, self.config, self.snapshot)
    }
}
#[derive(Debug, PartialEq)]
enum Presence {
    Absent,
    Present,
    Ejecting,
}
fn presence(bytes: &[u8], id: &str, path: &Path) -> Result<Presence> {
    let value = api_json::parse(bytes)?;
    let object = value.object()?;
    if !object.get("state").is_some_and(|v| v.is_string("Running")) {
        return Err(error("disk hotplug requires a running VM"));
    }
    let tree = object
        .get("device_tree")
        .ok_or_else(|| error("vm.info has no device_tree; disk hotplug requires eject tracking"))?
        .object()?;
    for (key, node) in tree {
        if !node
            .object()?
            .get("id")
            .is_some_and(|value| value.is_string(key))
        {
            return Err(error(
                "vm.info device_tree node identity is invalid; cannot confirm disk eject",
            ));
        }
    }
    let config = object
        .get("config")
        .ok_or_else(|| error("vm.info has no config"))?
        .object()?;
    let disks = config
        .get("disks")
        .ok_or_else(|| error("vm.info has no disk configuration"))?
        .array()?;
    let mut found = false;
    for disk in disks {
        let disk = disk.object()?;
        let same_id = disk.get("id").is_some_and(|v| v.is_string(id));
        let same_path = disk
            .get("path")
            .is_some_and(|v| v.is_string(&path.to_string_lossy()));
        if same_id || same_path {
            if found
                || !same_id
                || !same_path
                || !disk.get("readonly").is_some_and(|v| v.is_false())
            {
                return Err(error(
                    "live disk identity/path/permissions differ; refusing hotplug",
                ));
            }
            found = true;
        }
    }
    match (found, tree.contains_key(id)) {
        (true, true) => Ok(Presence::Present),
        (false, true) => Ok(Presence::Ejecting),
        (false, false) => Ok(Presence::Absent),
        _ => Err(error(
            "live disk config/device tree disagree; preserve hotplug intent",
        )),
    }
}
fn find_existing_id(bytes: &[u8], path: &Path) -> Result<String> {
    let value = api_json::parse(bytes)?;
    let config = value
        .object()?
        .get("config")
        .ok_or_else(|| error("vm.info has no config"))?
        .object()?;
    let disks = config
        .get("disks")
        .ok_or_else(|| error("vm.info has no disks"))?
        .array()?;
    let mut result = None;
    for disk in disks {
        let disk = disk.object()?;
        if disk
            .get("path")
            .is_some_and(|v| v.is_string(&path.to_string_lossy()))
        {
            let id = match disk.get("id") {
                Some(api_json::Value::String(id)) if valid_device_id(id) => *id,
                _ => return Err(error("live disk has no usable device ID")),
            };
            if result.replace(id.to_owned()).is_some() {
                return Err(error("multiple live disks use the volume path"));
            }
        }
    }
    let id =
        result.ok_or_else(|| error("attached volume was not found in live VM configuration"))?;
    if presence(bytes, &id, path)? != Presence::Present {
        return Err(error("disk is already ejecting; preserve its reservation"));
    }
    Ok(id)
}
fn apply(api: &impl DeviceApi, intent: &Intent, timeout: Duration) -> Result<()> {
    let status = presence(&api.info()?, &intent.device_id, &intent.path)?;
    match (intent.attach, status) {
        (true, Presence::Absent) => {
            let path = intent
                .path
                .to_str()
                .ok_or_else(|| error("Cloud Hypervisor paths must be UTF-8"))?;
            let body = format!(
                "{{\"path\":{},\"id\":{},\"readonly\":false,\"image_type\":\"Raw\"}}",
                json_string(path),
                json_string(&intent.device_id)
            );
            api.mutate("vm.add-disk", &body)?;
        }
        (false, Presence::Present) => {
            api.mutate(
                "vm.remove-device",
                &format!("{{\"id\":{}}}", json_string(&intent.device_id)),
            )?;
        }
        (true, Presence::Ejecting) => {
            return Err(error("disk is still ejecting; cannot reattach it"))
        }
        _ => (),
    }
    let deadline = Instant::now() + timeout;
    loop {
        let status = presence(&api.info()?, &intent.device_id, &intent.path)?;
        if (intent.attach && status == Presence::Present)
            || (!intent.attach && status == Presence::Absent)
        {
            return api.check();
        }
        if Instant::now() >= deadline {
            return Err(error(
                "disk hotplug confirmation timed out; Guest may not have completed PCI eject",
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(crate) fn execute(
    host: &HostConfig,
    before: &InstanceConfig,
    after: &InstanceConfig,
    volume: &str,
    path: &Path,
    attach: bool,
    snapshot: &Snapshot,
) -> Result<()> {
    instance::check_edit_directory(host, before)?;
    // Persisted disks must also be expressible by CH's cold-start argv.
    hypervisor::option_path(path)?;
    let api = Live {
        host,
        config: before,
        snapshot,
    };
    let intent = if let Some(intent) = pending(host, before)? {
        if intent.volume != volume
            || intent.attach != attach
            || intent.invocation != snapshot.invocation
        {
            return Err(error("another hotplug intent or invocation is pending; stop the instance and run instance recover"));
        }
        if intent.path != path {
            return Err(error("pending hotplug volume path changed"));
        }
        intent
    } else {
        let disk = (if attach { &after.disk } else { &before.disk })
            .iter()
            .find(|d| d.volume == volume)
            .ok_or_else(|| error("hotplug disk configuration missing"))?;
        let device_id = if attach {
            hypervisor::disk_id(&disk.volume_id)?
        } else {
            find_existing_id(&api.info()?, path)?
        };
        let intent = Intent {
            managed_by: "chadm".into(),
            instance_id: before.id.clone(),
            instance: before.name.clone(),
            invocation: snapshot.invocation.clone(),
            attach,
            volume: volume.into(),
            volume_id: disk.volume_id.clone(),
            device_id,
            path: path.into(),
            before: before.disk.clone(),
            after: after.disk.clone(),
        };
        validate_intent(before, &intent)?;
        // Preflight the response shape and identity before publishing or mutating.
        presence(&api.info()?, &intent.device_id, path)?;
        let encoded =
            toml::to_string(&intent).map_err(|_| error("cannot serialize hotplug intent"))?;
        metadata::write_new(&journal(host, before), &encoded)?;
        metadata::sync_directory(&host.layout().instances.join(&before.name))?;
        intent
    };
    let result = complete(&api, host, before, &intent, Duration::from_secs(30));
    result.map_err(|e| error(format!("{e}; inspect disk hotplug state; any retained hotplug.toml keeps the volume reserved; retry the same command, or stop instance {:?} and run instance recover", before.name)))
}
fn complete(
    api: &impl DeviceApi,
    host: &HostConfig,
    before: &InstanceConfig,
    intent: &Intent,
    timeout: Duration,
) -> Result<()> {
    apply(api, intent, timeout)?;
    let mut published = before.clone();
    published.disk = intent.after.clone();
    instance::replace_config(host, &published)?;
    clear(host, &published)
}

pub(crate) fn resume_config(
    host: &HostConfig,
    config: &InstanceConfig,
    volume: &str,
    attach: bool,
) -> Result<Option<InstanceConfig>> {
    let Some(intent) = pending(host, config)? else {
        return Ok(None);
    };
    if intent.volume != volume || intent.attach != attach {
        return Err(error(
            "another disk hotplug is pending; resolve it before editing volumes",
        ));
    }
    let mut after = config.clone();
    after.disk = intent.after;
    Ok(Some(after))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{image, repo, volume};
    use std::{
        cell::{Cell, RefCell},
        collections::VecDeque,
        os::unix::fs::DirBuilderExt,
    };

    fn response(id: &str, path: &Path, configured: bool, in_tree: bool) -> Vec<u8> {
        let disk = if configured {
            format!(
                "{{\"path\":{},\"id\":{},\"readonly\":false}}",
                json_string(&path.to_string_lossy()),
                json_string(id)
            )
        } else {
            String::new()
        };
        let node = if in_tree {
            format!("{}:{{\"id\":{}}}", json_string(id), json_string(id))
        } else {
            String::new()
        };
        format!(
            "{{\"state\":\"Running\",\"config\":{{\"disks\":[{disk}]}},\"device_tree\":{{{node}}}}}"
        )
        .into_bytes()
    }
    struct Fake {
        responses: RefCell<VecDeque<Vec<u8>>>,
        requests: RefCell<Vec<(String, String)>>,
        fail_mutation: Cell<bool>,
        changed: Cell<bool>,
    }
    impl Fake {
        fn new(responses: Vec<Vec<u8>>) -> Self {
            Self {
                responses: RefCell::new(responses.into()),
                requests: RefCell::new(Vec::new()),
                fail_mutation: Cell::new(false),
                changed: Cell::new(false),
            }
        }
    }
    impl DeviceApi for Fake {
        fn info(&self) -> Result<Vec<u8>> {
            self.check()?;
            let mut responses = self.responses.borrow_mut();
            if responses.len() > 1 {
                Ok(responses.pop_front().unwrap())
            } else {
                Ok(responses.front().unwrap().clone())
            }
        }
        fn mutate(&self, endpoint: &str, body: &str) -> Result<()> {
            self.check()?;
            self.requests
                .borrow_mut()
                .push((endpoint.into(), body.into()));
            if self.fail_mutation.get() {
                Err(error("API reply lost after mutation"))
            } else {
                Ok(())
            }
        }
        fn check(&self) -> Result<()> {
            if self.changed.get() {
                Err(error("invocation changed"))
            } else {
                Ok(())
            }
        }
    }
    struct Fixture {
        host: HostConfig,
        root: PathBuf,
        before: InstanceConfig,
        intent: Intent,
    }
    impl Fixture {
        fn new(attach: bool) -> Self {
            let root = std::env::temp_dir()
                .join(format!("chadm-hotplug-{}", metadata::random_id().unwrap()));
            fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
            let host = HostConfig {
                data_dir: root.join("data"),
                runtime_dir: root.join("run"),
                ..Default::default()
            };
            repo::create(&host, "local", &root.join("storage")).unwrap();
            let source = root.join("base.raw");
            fs::File::create(&source).unwrap().set_len(1 << 20).unwrap();
            image::create(&host, "base", "local", &source).unwrap();
            let mut options = instance::CreateOptions::new("test01", "base");
            options.no_cloud_init = true;
            let mut config = instance::create(&host, &options).unwrap();
            let data = volume::create(&host, "data", "local", 1 << 20).unwrap();
            let path = volume::info(&host, data.clone()).path.unwrap();
            let mut larger = config.disk.clone();
            larger.push(DiskConfig {
                name: "data".into(),
                volume: "data".into(),
                volume_id: data.id.clone(),
            });
            let smaller = config.disk.clone();
            if !attach {
                config.disk = larger.clone();
                instance::replace_config(&host, &config).unwrap();
            }
            let intent = Intent {
                managed_by: "chadm".into(),
                instance_id: config.id.clone(),
                instance: config.name.clone(),
                invocation: "a".repeat(32),
                attach,
                volume: "data".into(),
                volume_id: data.id.clone(),
                device_id: hypervisor::disk_id(&data.id).unwrap(),
                path,
                before: if attach {
                    smaller.clone()
                } else {
                    larger.clone()
                },
                after: if attach { larger } else { smaller },
            };
            metadata::write_new(&journal(&host, &config), &toml::to_string(&intent).unwrap())
                .unwrap();
            Self {
                host,
                root,
                before: config,
                intent,
            }
        }
        fn response(&self, configured: bool, in_tree: bool) -> Vec<u8> {
            response(
                &self.intent.device_id,
                &self.intent.path,
                configured,
                in_tree,
            )
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn attach_persists_only_after_confirmation_and_reserves_until_then() {
        let f = Fixture::new(true);
        assert!(volume::delete(&f.host, "data").is_err());
        let mut options = instance::CreateOptions::new("other", "base");
        options.no_cloud_init = true;
        instance::create(&f.host, &options).unwrap();
        assert!(volume::attach(&f.host, "data", "other")
            .unwrap_err()
            .to_string()
            .contains("attached to instance"));
        assert_eq!(
            instance::show(&f.host, "test01").unwrap().config.disk.len(),
            1
        );
        assert_eq!(instance::list(&f.host).unwrap()[0].config.disk.len(), 1);
        let api = Fake::new(vec![f.response(false, false), f.response(true, true)]);
        complete(&api, &f.host, &f.before, &f.intent, Duration::ZERO).unwrap();
        let after = instance::load(&f.host, "test01").unwrap();
        assert_eq!(after.disk, f.intent.after);
        assert!(pending(&f.host, &after).unwrap().is_none());
        assert!(volume::delete(&f.host, "data").is_err());
        let requests = api.requests.borrow();
        assert_eq!(requests[0].0, "vm.add-disk");
        let body = api_json::parse(requests[0].1.as_bytes()).unwrap();
        assert!(body.object().unwrap()["path"].is_string(f.intent.path.to_str().unwrap()));
        assert!(body.object().unwrap()["image_type"].is_string("Raw"));
    }
    #[test]
    fn detach_waits_for_tree_eject_before_releasing_volume() {
        let f = Fixture::new(false);
        let api = Fake::new(vec![f.response(true, true), f.response(false, true)]);
        assert!(complete(&api, &f.host, &f.before, &f.intent, Duration::ZERO).is_err());
        assert_eq!(
            instance::load(&f.host, "test01").unwrap().disk,
            f.intent.before
        );
        assert!(volume::delete(&f.host, "data").is_err());
        assert!(pending(&f.host, &f.before).unwrap().is_some());
        // A retry sees an eject already in progress, and does not request removal twice.
        let api = Fake::new(vec![f.response(false, true), f.response(false, false)]);
        complete(&api, &f.host, &f.before, &f.intent, Duration::ZERO).unwrap();
        assert!(api.requests.borrow().is_empty());
        volume::delete(&f.host, "data").unwrap();
    }
    #[test]
    fn lost_api_reply_keeps_intent_and_retry_confirms_without_duplicate_add() {
        let f = Fixture::new(true);
        let api = Fake::new(vec![f.response(false, false)]);
        api.fail_mutation.set(true);
        assert!(complete(&api, &f.host, &f.before, &f.intent, Duration::ZERO).is_err());
        assert!(volume::delete(&f.host, "data").is_err());
        let api = Fake::new(vec![f.response(true, true)]);
        complete(&api, &f.host, &f.before, &f.intent, Duration::ZERO).unwrap();
        assert!(api.requests.borrow().is_empty());
    }
    #[test]
    fn stopped_recovery_preserves_published_config_and_releases_pending_attach() {
        let f = Fixture::new(true);
        assert!(ensure_no_pending(&f.host, &f.before).is_err());
        assert!(volume::attach(&f.host, "data", "test01").is_err());
        recover_stopped(&f.host, &f.before).unwrap();
        assert_eq!(
            instance::load(&f.host, "test01").unwrap().disk,
            f.intent.before
        );
        volume::delete(&f.host, "data").unwrap();
        let f = Fixture::new(false);
        recover_stopped(&f.host, &f.before).unwrap();
        assert_eq!(
            instance::load(&f.host, "test01").unwrap().disk,
            f.intent.before
        );
        assert!(volume::delete(&f.host, "data").is_err());
    }
    #[test]
    fn foreign_identity_paused_vm_and_changed_invocation_cannot_mutate() {
        let f = Fixture::new(true);
        let api = Fake::new(vec![f.response(false, false)]);
        api.changed.set(true);
        assert!(complete(&api, &f.host, &f.before, &f.intent, Duration::ZERO).is_err());
        assert!(api.requests.borrow().is_empty());
        for raw in [
            response(&f.intent.device_id, Path::new("/foreign.raw"), true, true),
            response("other", &f.intent.path, true, true),
            f.response(true, false),
            String::from_utf8(f.response(false, false))
                .unwrap()
                .replace("Running", "Paused")
                .into_bytes(),
            String::from_utf8(f.response(true, true))
                .unwrap()
                .replace("\"readonly\":false", "\"readonly\":true")
                .into_bytes(),
            br#"{"state":"Running","config":{"disks":[]},"device_tree":{"nodes":{}}}"#.to_vec(),
            br#"{"state":"Running","config":{"disks":[]},"device_tree":null}"#.to_vec(),
        ] {
            let api = Fake::new(vec![raw]);
            assert!(apply(&api, &f.intent, Duration::ZERO).is_err());
            assert!(api.requests.borrow().is_empty());
        }
    }
    #[test]
    fn legacy_disk_id_and_escaped_paths_are_verified_structurally() {
        let path = Path::new("/storage/中文 \\\"disk.raw");
        let raw = response("_disk1", path, true, true);
        assert_eq!(find_existing_id(&raw, path).unwrap(), "_disk1");
        assert_eq!(presence(&raw, "_disk1", path).unwrap(), Presence::Present);
        assert!(find_existing_id(&raw, Path::new("/storage/other.raw")).is_err());
    }
    #[test]
    fn corrupted_intent_and_root_or_multiple_disk_edits_are_rejected() {
        let mut f = Fixture::new(true);
        f.intent.instance_id = "b".repeat(32);
        assert!(validate_intent(&f.before, &f.intent).is_err());
        f.intent.instance_id = f.before.id.clone();
        f.intent.after[0].volume_id = "b".repeat(32);
        assert!(validate_intent(&f.before, &f.intent).is_err());
    }
}
