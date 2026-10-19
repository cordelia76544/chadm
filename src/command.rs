//! Fixed executable lookup, bounded probes and shell-free utility execution. Never invoke a shell.
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

pub fn executable(program: &Path) -> Option<PathBuf> {
    if program.is_absolute() {
        return is_executable(program).then(|| program.to_path_buf());
    }
    if program.components().count() != 1 {
        return None;
    }
    // Build-time paths are part of the trusted package, never the caller's PATH.
    option_env!("CHADM_TOOL_PATH")
        .unwrap_or("/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin")
        .split(':')
        .filter(|dir| Path::new(dir).is_absolute())
        .map(|dir| Path::new(dir).join(program))
        .find(|path| is_executable(path))
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub fn probe(program: &Path, args: &[&str]) -> std::result::Result<String, String> {
    probe_with_timeout(program, args, Duration::from_secs(3))
}

fn probe_with_timeout(
    program: &Path,
    args: &[&str],
    timeout: Duration,
) -> std::result::Result<String, String> {
    probe_with_limit(program, args, timeout, 8192)
}

pub(crate) fn network_output(program: &Path, args: &[&str]) -> std::result::Result<String, String> {
    probe_with_limit(program, args, Duration::from_secs(3), 1024 * 1024)
}

fn probe_with_limit(
    program: &Path,
    args: &[&str],
    timeout: Duration,
    limit: u64,
) -> std::result::Result<String, String> {
    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot execute {program:?}: {error}"))?;
    let stdout = match child.stdout.take() {
        Some(stdout) => stdout,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err("cannot capture command output".into());
        }
    };
    let Some(mut stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("cannot capture command diagnostics".into());
    };
    let (diagnostic_sender, diagnostic_receiver) = mpsc::channel();
    let diagnostics = thread::Builder::new()
        .name("chadm-probe-stderr".into())
        .spawn(move || {
            let mut captured = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                match stderr.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        // Keep draining after the limit: stderr must not block the child.
                        let keep = count.min(8192usize.saturating_sub(captured.len()));
                        captured.extend_from_slice(&buffer[..keep]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
                    Err(_) => break,
                }
            }
            let _ = diagnostic_sender.send(captured);
        });
    if let Err(error) = diagnostics {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("cannot capture command diagnostics: {error}"));
    }
    let (sender, receiver) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("chadm-probe-output".into())
        .spawn(move || {
            let mut bytes = Vec::new();
            let result = stdout.take(limit + 1).read_to_end(&mut bytes);
            let _ = sender.send(result.map(|_| bytes));
        });
    if let Err(error) = reader {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("cannot read command output: {error}"));
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    let detail = match diagnostic_receiver
                        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        Ok(bytes) => String::from_utf8_lossy(&bytes)
                            .chars()
                            .filter(|c| !c.is_control() || *c == '\n')
                            .collect::<String>(),
                        Err(_) => "command diagnostics unavailable before deadline".into(),
                    };
                    return Err(format!(
                        "{program:?} exited with {status}: {}",
                        detail.trim()
                    ));
                }
                let bytes = receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|_| "command output timed out".to_string())?
                    .map_err(|error| format!("cannot read command output: {error}"))?;
                if bytes.len() as u64 > limit {
                    return Err("command output exceeds capture limit".into());
                }
                return Ok(String::from_utf8_lossy(&bytes).trim().to_string());
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match result {
                    Err(error) => format!("cannot wait for {program:?}: {error}"),
                    _ => format!("{program:?} timed out"),
                });
            }
        }
    }
}

/// Run an image utility with bounded diagnostic capture and a conversion timeout.
/// Drain stderr even after the capture limit, so a noisy utility cannot deadlock.
pub(crate) fn convert_image(
    program: &Path,
    args: &[std::ffi::OsString],
) -> std::result::Result<(), String> {
    run_utility(program, args, Duration::from_secs(3600))
}

pub(crate) fn run_utility(
    program: &Path,
    args: &[std::ffi::OsString],
    timeout: Duration,
) -> std::result::Result<(), String> {
    let mut child = Command::new(program)
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot execute {program:?}: {error}"))?;
    let Some(mut stderr) = child.stderr.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return Err("cannot capture command diagnostics".into());
    };
    let (sender, receiver) = mpsc::channel();
    let reader = thread::Builder::new()
        .name("chadm-utility-stderr".into())
        .spawn(move || {
            let mut captured = Vec::new();
            let mut buffer = [0u8; 4096];
            let result = loop {
                match stderr.read(&mut buffer) {
                    Ok(0) => break Ok(captured),
                    Ok(count) => {
                        let keep = count.min(8192usize.saturating_sub(captured.len()));
                        captured.extend_from_slice(&buffer[..keep]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => (),
                    Err(error) => break Err(error),
                }
            };
            let _ = sender.send(result);
        });
    if let Err(error) = reader {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("cannot capture command diagnostics: {error}"));
    }
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let bytes = receiver
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .map_err(|_| "command diagnostics timed out".to_string())?
                    .map_err(|error| format!("cannot read command diagnostics: {error}"))?;
                if status.success() {
                    return Ok(());
                }
                let detail: String = String::from_utf8_lossy(&bytes)
                    .chars()
                    .filter(|c| !c.is_control() || *c == '\n')
                    .collect();
                return Err(format!(
                    "{program:?} exited with {status}: {}",
                    detail.trim()
                ));
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(100)),
            result => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(match result {
                    Err(error) => format!("cannot wait for command: {error}"),
                    _ => format!("command timed out after {} seconds", timeout.as_secs_f64()),
                });
            }
        }
    }
}

/// Resolve every link and validate every ancestor before reading deployment artifacts.
pub(crate) fn trusted_file(
    path: &Path,
    root: &Path,
    root_uid: u32,
    executable: bool,
) -> crate::error::Result<PathBuf> {
    use crate::metadata;
    use std::os::unix::fs::MetadataExt;
    let error = |message: &str| crate::error::Error::Resource(message.into());
    use std::collections::VecDeque;
    use std::path::Component;
    let inspect = |path: &Path| {
        fs::symlink_metadata(path)
            .map_err(|e| metadata::io_error("inspect deployment path", path, e))
    };
    let base = inspect(root)?;
    if !base.is_dir() || base.uid() != root_uid || base.mode() & 0o022 != 0 {
        return Err(crate::error::Error::Resource(format!("untrusted deployment directory {root:?}; require root ownership and no group/other write access")));
    }
    let relative = path
        .strip_prefix(root)
        .map_err(|_| error("deployment path is outside trusted filesystem root"))?;
    let mut pending: VecDeque<_> = relative
        .components()
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    let mut resolved = root.to_path_buf();
    let mut links = 0;
    while let Some(part) = pending.pop_front() {
        let part_path = Path::new(&part);
        match part_path.components().next() {
            Some(Component::CurDir) => continue,
            Some(Component::ParentDir) => {
                if resolved == root {
                    return Err(error("deployment link escapes trusted filesystem root"));
                }
                resolved.pop();
                continue;
            }
            Some(Component::Normal(_)) => (),
            _ => return Err(error("invalid deployment path component")),
        }
        let candidate = resolved.join(part);
        let stat = inspect(&candidate)?;
        if stat.uid() != root_uid {
            return Err(crate::error::Error::Resource(format!(
                "untrusted deployment path {candidate:?}; require root ownership"
            )));
        }
        if stat.file_type().is_symlink() {
            links += 1;
            if links > 40 {
                return Err(error("too many symlinks in deployment path"));
            }
            let target = fs::read_link(&candidate)
                .map_err(|e| metadata::io_error("resolve deployment symlink", &candidate, e))?;
            let target = if target.is_absolute() {
                resolved = root.to_path_buf();
                target
                    .strip_prefix(root)
                    .map_err(|_| error("deployment link escapes trusted filesystem root"))?
                    .to_path_buf()
            } else {
                target
            };
            for component in target.components().rev() {
                pending.push_front(component.as_os_str().to_os_string());
            }
            continue;
        }
        // /nix/store can be root-owned 1775. The sticky bit prevents a
        // different owner from replacing existing entries; every child is
        // still checked for root ownership and write permissions below.
        let sticky_ancestor = !pending.is_empty() && stat.is_dir() && stat.mode() & 0o1000 != 0;
        if stat.mode() & 0o022 != 0 && !sticky_ancestor {
            return Err(crate::error::Error::Resource(format!(
                "untrusted deployment path {candidate:?}; group/other write access is forbidden"
            )));
        }
        if pending.is_empty() {
            if !stat.is_file() || (executable && stat.mode() & 0o111 == 0) {
                return Err(crate::error::Error::Resource(format!(
                    "deployment {candidate:?} must be an trusted regular file"
                )));
            }
            return Ok(candidate);
        }
        if !stat.is_dir() {
            return Err(crate::error::Error::Resource(format!(
                "deployment ancestor {candidate:?} is not a directory"
            )));
        }
        resolved = candidate;
    }
    Err(error("deployment resolves to a directory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_failure_and_timeout() {
        assert!(probe(Path::new("/bin/false"), &[]).is_err());
        assert_eq!(probe(Path::new("/bin/echo"), &["hello"]).unwrap(), "hello");
        assert!(
            probe_with_timeout(Path::new("/bin/sleep"), &["2"], Duration::from_millis(50))
                .unwrap_err()
                .contains("timed out")
        );
    }

    #[test]
    fn query_failure_keeps_bounded_stderr_without_deadlocking() {
        let missing = "/nonexistent-chadm-probe-diagnostics";
        let args = vec![missing; 1024];
        let error =
            probe_with_timeout(Path::new("/bin/ls"), &args, Duration::from_secs(5)).unwrap_err();
        assert!(error.contains("exited with"), "{error}");
        assert!(error.contains(missing), "{error}");
        assert!(error.len() < 9000);
    }

    #[test]
    fn image_utility_reports_failure_and_timeout() {
        let args = vec!["/nonexistent-chadm-image-utility-test".into()];
        let error = run_utility(Path::new("/bin/ls"), &args, Duration::from_secs(3)).unwrap_err();
        assert!(error.contains("exited with"));
        assert!(error.contains("nonexistent-chadm-image-utility-test"));
        let error = run_utility(
            Path::new("/bin/sleep"),
            &["2".into()],
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.contains("timed out"));
    }

    #[test]
    fn bounded_output_is_rejected_instead_of_silently_truncated() {
        let error = probe_with_limit(
            Path::new("/bin/echo"),
            &["0123456789"],
            Duration::from_secs(1),
            4,
        )
        .unwrap_err();
        assert!(error.contains("exceeds capture limit"));
    }

    #[test]
    fn rejects_relative_executable_paths() {
        assert!(executable(Path::new("./cloud-hypervisor")).is_none());
        assert!(executable(Path::new("../../bin/echo")).is_none());
    }

    #[test]
    fn deployment_links_allow_readonly_hardlinks_but_validate_every_ancestor() {
        use std::os::unix::fs::{symlink, DirBuilderExt};
        let root = std::env::temp_dir().join(format!(
            "chadm-deployment-{}",
            crate::metadata::random_id().unwrap()
        ));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        fs::DirBuilder::new()
            .mode(0o755)
            .create(root.join("store"))
            .unwrap();
        let file = root.join("store/template");
        fs::write(&file, "template").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
        fs::hard_link(&file, root.join("store/alias")).unwrap();
        symlink("store/template", root.join("template")).unwrap();
        let resolve =
            |name: &str| trusted_file(&root.join(name), &root, crate::metadata::uid(), false);
        assert_eq!(resolve("template").unwrap(), file);
        fs::set_permissions(root.join("store"), fs::Permissions::from_mode(0o1775)).unwrap();
        assert_eq!(resolve("template").unwrap(), file);
        assert!(crate::metadata::open_regular(&file, false).is_err());
        assert!(trusted_file(&file, &root, crate::metadata::uid(), true).is_err());
        // A root-controlled alias must not skip an unsafe intermediate link.
        fs::DirBuilder::new()
            .mode(0o755)
            .create(root.join("unsafe"))
            .unwrap();
        symlink("../store/template", root.join("unsafe/alias")).unwrap();
        symlink("unsafe/alias", root.join("indirect")).unwrap();
        fs::set_permissions(root.join("unsafe"), fs::Permissions::from_mode(0o777)).unwrap();
        assert!(resolve("indirect").is_err());
        assert!(resolve("store").is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
