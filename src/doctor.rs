use crate::command::{executable, probe};
use crate::config::HostConfig;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::FileTypeExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Warn,
    Fail,
    NotApplicable,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ok => "OK",
            Self::Warn => "WARN",
            Self::Fail => "FAIL",
            Self::NotApplicable => "N/A",
        }
    }
}

#[derive(Debug)]
pub struct Check {
    pub name: &'static str,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug)]
pub struct Report {
    pub checks: Vec<Check>,
}

impl Report {
    pub fn json(&self) -> String {
        use crate::output::json_string;
        let checks = self
            .checks
            .iter()
            .map(|check| {
                format!(
                    "{{\"name\":{},\"status\":{},\"detail\":{}}}",
                    json_string(check.name),
                    json_string(check.status.label()),
                    json_string(&check.detail)
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"healthy\":{},\"checks\":[{checks}]}}", self.healthy())
    }
    pub fn healthy(&self) -> bool {
        self.checks.iter().all(|check| check.status != Status::Fail)
    }
}

pub fn run(config: &HostConfig) -> Report {
    let mut checks = vec![kvm_check()];
    checks.push(tool(
        "cloud-hypervisor",
        &config.cloud_hypervisor,
        &["--version"],
        true,
    ));
    let mut systemd = tool("systemd", Path::new("systemctl"), &["--version"], true);
    if systemd.status == Status::Ok {
        if !Path::new("/run/systemd/system").is_dir() {
            systemd.status = Status::Fail;
            systemd.detail = "systemd is installed but is not the active system manager".into();
        } else if let Some(path) = executable(Path::new("systemctl")) {
            if let Err(error) = probe(&path, &["show", "--property=Version", "--value"]) {
                systemd.status = Status::Fail;
                systemd.detail = format!("cannot contact the system manager: {error}");
            }
        }
    }
    checks.push(systemd);
    checks.push(tool("iproute2", Path::new("ip"), &["-Version"], true));
    checks.push(tool(
        "qemu-img",
        Path::new("qemu-img"),
        &["--version"],
        false,
    ));
    let firmware = config.firmware_path();
    let firmware_check = crate::instance_config::check_boot_files(
        &crate::instance::BootConfig::Firmware { firmware: None },
        &firmware,
        true,
    );
    checks.push(Check {
        name: "firmware",
        status: if firmware_check.is_ok() {
            Status::Ok
        } else if fs::symlink_metadata(&firmware)
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            Status::Warn
        } else {
            Status::Fail
        },
        detail: match firmware_check {
            Ok(()) => format!(
                "{} (readability only; boot compatibility is not verified)",
                firmware.display()
            ),
            Err(e) => format!(
                "fixed firmware {}: {e}; required for firmware boot",
                firmware.display()
            ),
        },
    });
    checks.push(match crate::repo::list(config) {
        Ok(repos) if repos.is_empty() => Check {
            name: "repos",
            status: Status::NotApplicable,
            detail: "no repositories configured".into(),
        },
        Ok(repos) => {
            let unavailable: Vec<_> = repos
                .iter()
                .filter(|repo| !repo.available)
                .map(|repo| {
                    format!(
                        "{}: {}",
                        repo.config.name,
                        repo.reason.as_deref().unwrap_or("unavailable")
                    )
                })
                .collect();
            Check {
                name: "repos",
                status: if unavailable.is_empty() {
                    Status::Ok
                } else {
                    Status::Fail
                },
                detail: if unavailable.is_empty() {
                    format!("{} file repositories available", repos.len())
                } else {
                    unavailable.join("; ")
                },
            }
        }
        Err(error) => Check {
            name: "repos",
            status: Status::Fail,
            detail: error.to_string(),
        },
    });
    checks.push(match crate::volume::list(config) {
        Ok(volumes) if volumes.is_empty() => Check {
            name: "volumes",
            status: Status::NotApplicable,
            detail: "no volumes configured".into(),
        },
        Ok(volumes) => {
            let unavailable: Vec<_> = volumes
                .iter()
                .filter(|volume| !volume.available)
                .map(|volume| {
                    format!(
                        "{}: {}",
                        volume.config.name,
                        volume.reason.as_deref().unwrap_or("unavailable")
                    )
                })
                .collect();
            Check {
                name: "volumes",
                status: if unavailable.is_empty() {
                    Status::Ok
                } else {
                    Status::Fail
                },
                detail: if unavailable.is_empty() {
                    format!("{} volumes available", volumes.len())
                } else {
                    unavailable.join("; ")
                },
            }
        }
        Err(error) => Check {
            name: "volumes",
            status: Status::Fail,
            detail: error.to_string(),
        },
    });
    checks.push(match crate::image::list(config) {
        Ok(images) if images.is_empty() => Check {
            name: "images",
            status: Status::NotApplicable,
            detail: "no images configured".into(),
        },
        Ok(images) => {
            let unavailable: Vec<_> = images
                .iter()
                .filter(|image| !image.available)
                .map(|image| {
                    format!(
                        "{}: {}",
                        image.config.name,
                        image.reason.as_deref().unwrap_or("unavailable")
                    )
                })
                .collect();
            Check {
                name: "images",
                status: if unavailable.is_empty() {
                    Status::Ok
                } else {
                    Status::Fail
                },
                detail: if unavailable.is_empty() {
                    format!("{} images available", images.len())
                } else {
                    unavailable.join("; ")
                },
            }
        }
        Err(error) => Check {
            name: "images",
            status: Status::Fail,
            detail: error.to_string(),
        },
    });
    checks.push(match crate::instance::list(config) {
        Ok(instances) if instances.is_empty() => Check {
            name: "instances",
            status: Status::NotApplicable,
            detail: "no instances configured".into(),
        },
        Ok(instances) => {
            let unavailable: Vec<_> = instances
                .iter()
                .filter(|instance| !instance.available)
                .map(|instance| {
                    format!(
                        "{}: {}",
                        instance.config.name,
                        instance.reason.as_deref().unwrap_or("unavailable")
                    )
                })
                .collect();
            Check {
                name: "instances",
                status: if unavailable.is_empty() {
                    Status::Ok
                } else {
                    Status::Fail
                },
                detail: if unavailable.is_empty() {
                    format!(
                        "{} instance configurations and disks available",
                        instances.len()
                    )
                } else {
                    unavailable.join("; ")
                },
            }
        }
        Err(error) => Check {
            name: "instances",
            status: Status::Fail,
            detail: error.to_string(),
        },
    });
    checks.push(tool(
        "bridge utility",
        Path::new("bridge"),
        &["-Version"],
        false,
    ));
    checks.push(bridge_check(config));
    checks.push(Check {
        name: "TUN/TAP",
        status: if fs::symlink_metadata("/dev/net/tun")
            .is_ok_and(|m| m.file_type().is_char_device())
        {
            Status::Ok
        } else {
            Status::Fail
        },
        detail:
            "/dev/net/tun character device check only; CAP_NET_ADMIN is tested during preparation"
                .into(),
    });
    checks.push(match crate::repo::list(config) {
        Ok(repos)
            if !repos
                .iter()
                .any(|r| r.config.driver == crate::repo::Driver::Zfs) =>
        {
            Check {
                name: "ZFS",
                status: Status::NotApplicable,
                detail: "no ZFS repos configured".into(),
            }
        }
        Ok(_) => {
            let result = crate::storage::zfs::version();
            match result {
                Ok(version) => Check {
                    name: "ZFS",
                    status: Status::Ok,
                    detail: version.trim().replace('\n', "; "),
                },
                Err(reason) => Check {
                    name: "ZFS",
                    status: Status::Fail,
                    detail: format!(
                        "ZFS unavailable: {reason}; requires /dev/zfs and loaded kernel module"
                    ),
                },
            }
        }
        Err(e) => Check {
            name: "ZFS",
            status: Status::Fail,
            detail: format!("cannot determine enabled ZFS repos: {e}"),
        },
    });
    let template = Path::new("/etc/systemd/system/chadm-instance@.service");
    checks.push(match crate::checkpoint::list(config) {
        Ok(snapshots) if snapshots.is_empty() => Check {
            name: "snapshots",
            status: Status::NotApplicable,
            detail: "no snapshots or suspend checkpoints".into(),
        },
        Ok(snapshots) => {
            let problems: Vec<_> = snapshots
                .iter()
                .filter(|s| !s.available)
                .map(|s| {
                    format!(
                        "{}: {}",
                        s.name,
                        s.reason.as_deref().unwrap_or("unavailable")
                    )
                })
                .collect();
            Check {
                name: "snapshots",
                status: if problems.is_empty() {
                    Status::Ok
                } else {
                    Status::Fail
                },
                detail: if problems.is_empty() {
                    format!("{} snapshot/checkpoint records available", snapshots.len())
                } else {
                    problems.join("; ")
                },
            }
        }
        Err(e) => Check {
            name: "snapshots",
            status: Status::Fail,
            detail: e.to_string(),
        },
    });
    checks.push(Check {
        name: "service template",
        status: if crate::systemd::deployment_path(template).ok().and_then(|p| fs::read_to_string(p).ok()).is_some_and(|s|s == crate::systemd::TEMPLATE) {Status::Ok} else {Status::Warn},
        detail: "shared template at /etc/systemd/system/chadm-instance@.service; startup also verifies loaded template and installed helper".into(),
    });
    checks.push(Check {
        name: "configuration",
        status: Status::Ok,
        detail: format!(
            "data={:?}, runtime={:?}; no directories created",
            config.data_dir, config.runtime_dir
        ),
    });
    Report { checks }
}

fn bridge_check(config: &HostConfig) -> Check {
    let result = crate::instance::list(config).map(|instances| {
        let bridges: std::collections::BTreeSet<_> = instances
            .iter()
            .flat_map(|instance| instance.config.network.iter().map(|net| net.bridge.clone()))
            .collect();
        let mut failures = Vec::new();
        for bridge in &bridges {
            if let Err(error) = crate::network::bridge_status(bridge) {
                failures.push(error.to_string());
            }
        }
        (bridges.len(), failures)
    });
    let (status, detail) = match result {
        Ok((0, _)) => (Status::NotApplicable, "no instance bridges configured".into()),
        Ok((count, failures)) if failures.is_empty() => (Status::Ok, format!("{count} Linux bridges exist and are administratively UP; DHCP connectivity is not tested")),
        Ok((_, failures)) => (Status::Fail, failures.join("; ")),
        Err(error) => (Status::Fail, error.to_string()),
    };
    Check {
        name: "bridges",
        status,
        detail,
    }
}

fn kvm_check() -> Check {
    let path = Path::new("/dev/kvm");
    let result = fs::metadata(path).and_then(|metadata| {
        if !metadata.file_type().is_char_device() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "not a character device",
            ));
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map(|_| ())
    });
    Check {
        name: "KVM",
        status: if result.is_ok() {
            Status::Ok
        } else {
            Status::Fail
        },
        detail: match result {
            Ok(()) => "/dev/kvm is accessible (VM creation is not tested)".into(),
            Err(error) => format!("/dev/kvm is unavailable to the current user: {error}"),
        },
    }
}

fn tool(name: &'static str, program: &Path, args: &[&str], required: bool) -> Check {
    let result = executable(program)
        .ok_or_else(|| format!("{program:?} not found in trusted system directories"))
        .and_then(|path| probe(&path, args));
    match result {
        Ok(output) => Check {
            name,
            status: Status::Ok,
            detail: output
                .lines()
                .next()
                .unwrap_or("command succeeded")
                .chars()
                .filter(|c| !c.is_control())
                .take(256)
                .collect(),
        },
        Err(detail) => Check {
            name,
            status: if required { Status::Fail } else { Status::Warn },
            detail,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warnings_do_not_fail_doctor() {
        let mut report = Report {
            checks: vec![Check {
                name: "firmware",
                status: Status::Warn,
                detail: String::new(),
            }],
        };
        assert!(report.healthy());
        report.checks.push(Check {
            name: "KVM",
            status: Status::Fail,
            detail: String::new(),
        });
        assert!(!report.healthy());
    }
}
