//! systemd's unit/cgroup is authoritative. Never signal a PID from a file.
use crate::{
    command,
    error::{Error, Result},
    metadata,
};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const TEMPLATE: &str = include_str!("../packaging/systemd/chadm-instance@.service");
pub const HELPER: &str = match option_env!("CHADM_HELPER") {
    Some(path) => path,
    None => "/usr/local/bin/chadm",
};

/// Deployment artifacts can be root-owned symlinks (NixOS) and hardlinked in
/// the Nix store. Mutable resource metadata still uses the stricter primitives.
pub(crate) fn deployment_path(path: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    metadata::validate_path(path)?;
    let root_uid = std::fs::metadata("/")
        .map_err(|e| metadata::io_error("inspect filesystem root", Path::new("/"), e))?
        .uid();
    command::trusted_file(path, Path::new("/"), root_uid, false)
}
#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub load: String,
    pub active: String,
    pub sub: String,
    pub pid: u32,
    pub control_pid: u32,
    pub job: u32,
    pub cgroup: String,
    pub invocation: String,
    pub fragment: PathBuf,
    pub drop_ins: String,
}
pub fn unit(name: &str) -> Result<String> {
    metadata::validate_name("instance", name)?;
    Ok(format!("chadm-instance@{name}.service"))
}
fn error(s: impl Into<String>) -> Error {
    Error::Resource(s.into())
}
pub(crate) trait Manager {
    fn verify(&self, snapshot: &Snapshot) -> Result<()> {
        verify_template(snapshot)
    }
    fn quiescent(&self, snapshot: &Snapshot) -> Result<()> {
        empty_cgroup(snapshot)
    }
    fn snapshot(&self, name: &str) -> Result<Snapshot>;
    fn action(&self, action: &str, name: &str) -> Result<()>;
}
pub(crate) struct System;
impl System {
    fn executable() -> Result<PathBuf> {
        command::executable(Path::new("systemctl")).ok_or_else(|| error("systemctl was not found"))
    }
    pub(crate) fn reload() -> Result<()> {
        command::run_utility(
            &Self::executable()?,
            &["--system".into(), "daemon-reload".into()],
            Duration::from_secs(10),
        )
        .map_err(error)
    }
}
impl Manager for System {
    fn snapshot(&self, name: &str) -> Result<Snapshot> {
        let text = command::network_output(&Self::executable()?, &["--system", "--no-pager", "--all", "show", &unit(name)?, "--property=LoadState,ActiveState,SubState,MainPID,ControlPID,InvocationID,FragmentPath,DropInPaths,Job,ControlGroup"])
            .map_err(|e| error(format!("cannot query systemd unit state: {e}")))?;
        parse(&text)
    }
    fn action(&self, action: &str, name: &str) -> Result<()> {
        if !["start", "stop", "reset-failed"].contains(&action) {
            return Err(error("unsupported systemd action"));
        }
        command::run_utility(
            &Self::executable()?,
            &[
                "--system".into(),
                "--no-block".into(),
                action.into(),
                unit(name)?.into(),
            ],
            Duration::from_secs(5),
        )
        .map_err(error)
    }
}
fn parse(text: &str) -> Result<Snapshot> {
    let mut fields = std::collections::HashMap::new();
    for line in text.lines() {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| error("invalid systemd state response"))?;
        if fields.insert(key, value).is_some() {
            return Err(error("duplicate systemd state property"));
        }
    }
    let get = |key| {
        fields
            .get(key)
            .copied()
            .ok_or_else(|| error(format!("systemd did not return {key}")))
    };
    let pid = get("MainPID")?
        .parse::<u32>()
        .map_err(|_| error("invalid systemd MainPID"))?;
    Ok(Snapshot {
        load: get("LoadState")?.into(),
        active: get("ActiveState")?.into(),
        sub: get("SubState")?.into(),
        pid,
        control_pid: get("ControlPID")?
            .parse()
            .map_err(|_| error("invalid systemd ControlPID"))?,
        job: if get("Job")?.is_empty() {
            0
        } else {
            get("Job")?
                .parse()
                .map_err(|_| error("invalid systemd Job"))?
        },
        cgroup: get("ControlGroup")?.into(),
        invocation: get("InvocationID")?.into(),
        fragment: get("FragmentPath")?.into(),
        drop_ins: get("DropInPaths")?.into(),
    })
}
pub(crate) fn verify_template(snapshot: &Snapshot) -> Result<()> {
    if snapshot.load != "loaded"
        || snapshot.fragment.file_name().and_then(|n| n.to_str()) != Some("chadm-instance@.service")
        || !snapshot.drop_ins.is_empty()
    {
        return Err(error("install the shared chadm-instance@.service template and run systemctl daemon-reload; per-instance units and drop-ins are not supported in Phase 7"));
    }
    let template = deployment_path(&snapshot.fragment)?;
    if std::fs::read_to_string(&template)
        .map_err(|e| metadata::io_error("read systemd template", &template, e))?
        != TEMPLATE
    {
        return Err(error("installed systemd template differs from this chadm build; install the matching shared template and reload systemd"));
    }
    let binary = deployment_path(Path::new(HELPER))?;
    if command::executable(&binary).is_none() {
        return Err(error(format!("install this chadm build at {HELPER}")));
    }
    Ok(())
}
pub(crate) fn terminal(snapshot: &Snapshot) -> bool {
    snapshot.pid == 0
        && snapshot.control_pid == 0
        && snapshot.job == 0
        && (["inactive", "failed"].contains(&snapshot.active.as_str())
            || snapshot.load == "not-found")
}

fn unpopulated(text: &str) -> Result<()> {
    let mut populated = None;
    for line in text.lines() {
        let fields: Vec<_> = line.split_whitespace().collect();
        if fields.len() != 2 {
            return Err(error("invalid cgroup.events response"));
        }
        if fields[0] == "populated" && populated.replace(fields[1]).is_some() {
            return Err(error("duplicate cgroup population field"));
        }
    }
    if populated != Some("0") {
        return Err(error(
            "unit cgroup is populated or unverifiable; refusing recovery",
        ));
    }
    Ok(())
}

fn empty_cgroup(snapshot: &Snapshot) -> Result<()> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    if !terminal(snapshot) {
        return Err(error(
            "unit has processes, a pending job or a transition; refusing recovery",
        ));
    }
    // systemd clears ControlGroup after the unit's cgroup has been released.
    if snapshot.cgroup.is_empty() {
        return Ok(());
    }
    let path = Path::new(&snapshot.cgroup);
    if !path.is_absolute()
        || path == Path::new("/")
        || path.components().any(|c| {
            !matches!(
                c,
                std::path::Component::RootDir | std::path::Component::Normal(_)
            )
        })
    {
        return Err(error("unsafe systemd ControlGroup path"));
    }
    let root = Path::new("/sys/fs/cgroup");
    metadata::directory(root, false)?;
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| error(format!("cannot verify cgroup mount: {e}")))?;
    if !mounts.lines().any(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        fields.get(4) == Some(&"/sys/fs/cgroup") && fields.windows(2).any(|p| p == ["-", "cgroup2"])
    }) {
        return Err(error(
            "stale recovery with a retained cgroup requires cgroup v2 at /sys/fs/cgroup",
        ));
    }
    let dir = root.join(
        path.strip_prefix("/")
            .map_err(|_| error("invalid cgroup path"))?,
    );
    match std::fs::symlink_metadata(&dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(metadata::io_error("inspect unit cgroup", &dir, e)),
        Ok(_) => metadata::directory(&dir, false)?,
    }
    let events = dir.join("cgroup.events");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(0x20000 | 0x800)
        .open(&events)
        .map_err(|e| metadata::io_error("open unit cgroup events", &events, e))?;
    let mut text = String::new();
    file.take(4097)
        .read_to_string(&mut text)
        .map_err(|e| metadata::io_error("read unit cgroup events", &events, e))?;
    if text.len() > 4096 {
        return Err(error("cgroup events exceeds capture limit"));
    }
    unpopulated(&text)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn parses_unit_properties_and_refuses_bad_or_ambiguous_responses() {
        let text="LoadState=loaded\nActiveState=active\nSubState=running\nMainPID=123\nControlPID=0\nInvocationID=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nFragmentPath=/etc/systemd/system/chadm-instance@.service\nDropInPaths=\nJob=0\nControlGroup=\n";
        let s = parse(text).unwrap();
        assert_eq!(s.pid, 123);
        assert!(!terminal(&s));
        assert!(parse(&text.replace("MainPID=123", "MainPID=abc")).is_err());
        assert!(parse(&format!("{text}MainPID=123\n")).is_err());
        assert!(parse("LoadState=loaded").is_err());
        let mut s = s;
        s.active = "inactive".into();
        assert!(!terminal(&s));
        s.pid = 0;
        assert!(terminal(&s));
    }
    #[test]
    fn shared_template_and_names_are_constrained() {
        assert_eq!(unit("test-01").unwrap(), "chadm-instance@test-01.service");
        assert!(unit("../test").is_err());
        assert!(unit("a;reboot").is_err());
        assert!(TEMPLATE.contains(&format!("ExecStart={HELPER} __service run %i")));
        assert!(TEMPLATE.contains("ExecStopPost="));
        assert!(TEMPLATE.contains("KillMode=control-group"));
        assert!(!TEMPLATE.contains("%I"));
        assert!(!TEMPLATE.contains("PIDFile="));
        let s = Snapshot {
            load: "loaded".into(),
            active: "inactive".into(),
            sub: "dead".into(),
            pid: 0,
            control_pid: 0,
            job: 0,
            cgroup: String::new(),
            invocation: String::new(),
            fragment: "/etc/systemd/system/chadm-instance@test01.service".into(),
            drop_ins: String::new(),
        };
        assert!(verify_template(&s).is_err());
    }

    #[test]
    fn terminal_requires_no_helper_or_job_and_empty_cgroup_is_strict() {
        let text = "LoadState=loaded\nActiveState=inactive\nSubState=dead\nMainPID=0\nControlPID=0\nInvocationID=\nFragmentPath=/etc/systemd/system/chadm-instance@.service\nDropInPaths=\nJob=\nControlGroup=\n";
        let mut s = parse(text).unwrap();
        assert!(terminal(&s));
        assert!(empty_cgroup(&s).is_ok());
        s.job = 4;
        assert!(!terminal(&s));
        assert!(empty_cgroup(&s).is_err());
        s.job = 0;
        s.control_pid = 8;
        assert!(!terminal(&s));
        s.control_pid = 0;
        s.cgroup = "/../../tmp".into();
        assert!(empty_cgroup(&s).is_err());
        for value in [
            "",
            "populated 1\n",
            "populated 0\npopulated 0\n",
            "populated\n",
            "populated unknown\n",
        ] {
            assert!(unpopulated(value).is_err());
        }
        assert!(unpopulated("populated 0\nfrozen 0\n").is_ok());
        assert!(parse(&text.replace("Job=\n", "Job=bad\n")).is_err());
    }
}
