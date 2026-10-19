//! Cloud Hypervisor v52 argv and bounded HTTP over the instance UNIX API socket.
use crate::{
    command,
    config::HostConfig,
    error::{Error, Result},
    image,
    instance::{BootConfig, InstanceConfig},
    metadata, volume,
};
use std::ffi::{c_void, OsString};
use std::fs;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;
use std::os::unix::{fs::FileTypeExt, net::UnixStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn error(s: impl Into<String>) -> Error {
    Error::Resource(s.into())
}
/// Comma-separated CH option values cannot escape commas; reject these paths explicitly.
pub(crate) fn option_path(path: &Path) -> Result<String> {
    metadata::validate_path(path)?;
    let text = path
        .to_str()
        .ok_or_else(|| error("Cloud Hypervisor paths must be UTF-8"))?;
    if text.contains(',') {
        return Err(error(format!(
            "Cloud Hypervisor option path {path:?} contains a comma"
        )));
    }
    Ok(text.into())
}
pub fn command_line(
    host: &HostConfig,
    config: &InstanceConfig,
) -> Result<(PathBuf, Vec<OsString>)> {
    crate::instance_config::validate(config)?;
    crate::instance_config::check_boot_files(&config.boot, &host.firmware_path(), true)?;
    let source = image::checked_unlocked(host, &config.image)?;
    if source.config.repo == crate::cloud_init::REPO {
        return Err(error("cloudinit seed cannot be used as a VM base image"));
    }
    if source.config.id != config.image_id {
        return Err(error("instance image generation changed"));
    }
    let program = command::executable(&host.cloud_hypervisor)
        .ok_or_else(|| error("cloud-hypervisor executable was not found"))?;
    let runtime = host.runtime_dir.join(&config.name);
    if ["api.sock", "console.sock"]
        .iter()
        .any(|name| runtime.join(name).as_os_str().as_encoded_bytes().len() >= 108)
    {
        return Err(error(
            "runtime socket path exceeds Linux UNIX socket limit (107 bytes)",
        ));
    }
    if let Some(source) = crate::checkpoint::restore_source(host, config)? {
        return Ok((
            program,
            vec![
                "--api-socket".into(),
                format!("path={}", option_path(&runtime.join("api.sock"))?).into(),
                "--restore".into(),
                format!(
                    "source_url={},resume=true,memory_restore_mode=copy",
                    file_url(&source)?
                )
                .into(),
            ],
        ));
    }
    let mut args: Vec<OsString> = vec![
        "--cpus".into(),
        format!(
            "boot={},max={}{}",
            config.compute.cpus,
            config.compute.cpus,
            if config.windows {
                ",kvm_hyperv=on,max_phys_bits=39"
            } else {
                ""
            }
        )
        .into(),
        "--memory".into(),
        format!("size={}", volume::parse_size(&config.compute.memory)?).into(),
    ];
    match &config.boot {
        BootConfig::Firmware { .. } => {
            args.extend(["--firmware".into(), host.firmware_path().into_os_string()]);
        }
        BootConfig::Direct {
            kernel,
            initramfs,
            cmdline,
        } => {
            args.extend(["--kernel".into(), kernel.as_os_str().into()]);
            if let Some(path) = initramfs {
                args.extend(["--initramfs".into(), path.as_os_str().into()]);
            }
            args.extend(["--cmdline".into(), cmdline.into()]);
        }
    }
    for disk in &config.disk {
        let resolved = volume::load(host, &disk.volume)?;
        if resolved.id != disk.volume_id {
            return Err(error(format!(
                "volume {:?} generation changed",
                disk.volume
            )));
        }
        if disk.name == "root"
            && (resolved.source_image_id.as_deref() != Some(&config.image_id)
                || resolved.source_image.as_deref() != Some(&config.image))
        {
            return Err(error("root volume source does not match instance image"));
        }
        let info = volume::info(host, resolved);
        if !info.available {
            return Err(error(format!(
                "volume {:?} is unavailable: {}",
                disk.volume,
                info.reason.as_deref().unwrap_or("unknown reason")
            )));
        }
        let path = info
            .path
            .ok_or_else(|| error("volume path is unavailable"))?;
        args.extend([
            "--disk".into(),
            format!(
                "path={},readonly=off,image_type=raw,id={}",
                option_path(&path)?,
                disk_id(&disk.volume_id)?
            )
            .into(),
        ]);
    }
    if let Some(seed) = &config.cloud_init {
        let path = crate::cloud_init::resolve(host, seed)?;
        args.extend([
            "--disk".into(),
            format!("path={},readonly=on,image_type=raw", option_path(&path)?).into(),
        ]);
    }
    for net in &config.network {
        args.extend([
            "--net".into(),
            format!("tap={},mac={},num_queues=2", net.tap, net.mac).into(),
        ]);
    }
    args.extend([
        "--api-socket".into(),
        format!("path={}", option_path(&runtime.join("api.sock"))?).into(),
        "--serial".into(),
        format!("socket={}", option_path(&runtime.join("console.sock"))?).into(),
        "--console".into(),
        "off".into(),
    ]);
    if runtime
        .join("console.sock")
        .as_os_str()
        .as_encoded_bytes()
        .len()
        >= 108
        || runtime
            .join("api.sock")
            .as_os_str()
            .as_encoded_bytes()
            .len()
            >= 108
    {
        return Err(error(
            "runtime socket path exceeds Linux UNIX socket limit (107 bytes)",
        ));
    }
    Ok((program, args))
}
pub(crate) fn check_capabilities(program: &Path, windows: bool) -> Result<()> {
    let help = command::network_output(program, &["--help"]).map_err(error)?;
    check_advertised_capabilities(&help, windows)
}
fn check_advertised_capabilities(help: &str, windows: bool) -> Result<()> {
    for required in [
        "--firmware",
        "--kernel",
        "--initramfs",
        "--disk",
        "image_type=",
        "--net",
        "--api-socket",
        "--serial",
        "socket=",
        "--console",
    ] {
        if !help.contains(required) {
            return Err(error(format!("installed Cloud Hypervisor does not advertise required option {required:?}; Phase 7 targets v52")));
        }
    }
    if windows {
        for required in ["kvm_hyperv=", "max_phys_bits="] {
            if !help.contains(required) {
                return Err(error(format!(
                    "installed Cloud Hypervisor does not advertise Windows CPU option {required:?}"
                )));
            }
        }
    }
    Ok(())
}

/// Connect nonblocking, verify SO_PEERCRED against systemd's live MainPID, then use a deadline.
pub(crate) fn connect(path: &Path, pid: u32) -> Result<UnixStream> {
    metadata::directory(
        path.parent()
            .ok_or_else(|| error("invalid VM socket path"))?,
        false,
    )?;
    let m =
        fs::symlink_metadata(path).map_err(|e| metadata::io_error("inspect VM socket", path, e))?;
    use std::os::unix::fs::MetadataExt;
    if !m.file_type().is_socket()
        || m.uid() != metadata::uid()
        || m.nlink() != 1
        || m.mode() & 0o022 != 0
    {
        return Err(error("VM socket has an unsafe type, owner or permissions"));
    }
    let name = path.as_os_str().as_encoded_bytes();
    if name.len() >= 108 {
        return Err(error("VM socket path is too long"));
    }
    #[repr(C)]
    struct SockAddr {
        family: u16,
        path: [u8; 108],
    }
    #[repr(C)]
    struct Poll {
        fd: i32,
        events: i16,
        revents: i16,
    }
    #[repr(C)]
    struct Cred {
        pid: i32,
        uid: u32,
        gid: u32,
    }
    unsafe extern "C" {
        fn socket(domain: i32, kind: i32, protocol: i32) -> i32;
        fn connect(fd: i32, addr: *const c_void, len: u32) -> i32;
        fn poll(fds: *mut Poll, count: usize, timeout: i32) -> i32;
        fn getsockopt(fd: i32, level: i32, option: i32, value: *mut c_void, len: *mut u32) -> i32;
    }
    // SAFETY: Linux AF_UNIX/SOCK_STREAM, nonblocking and close-on-exec.
    let fd = unsafe { socket(1, 1 | 0x800 | 0x80000, 0) };
    if fd < 0 {
        return Err(error(format!(
            "cannot create VM socket: {}",
            std::io::Error::last_os_error()
        )));
    }
    // SAFETY: fd is a new UNIX stream socket, owned from this point onward.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let mut addr = SockAddr {
        family: 1,
        path: [0; 108],
    };
    addr.path[..name.len()].copy_from_slice(name);
    // SAFETY: initialized sockaddr_un, exact pathname length including trailing NUL.
    if unsafe {
        connect(
            fd,
            (&addr as *const SockAddr).cast(),
            (2 + name.len() + 1) as u32,
        )
    } < 0
    {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() != Some(115) {
            return Err(error(format!("cannot connect to VM socket: {e}")));
        }
        let mut p = Poll {
            fd,
            events: 4,
            revents: 0,
        };
        // SAFETY: one initialized pollfd and bounded 2-second wait.
        if unsafe { poll(&mut p, 1, 2000) } <= 0 {
            return Err(error("VM socket connection timed out or was interrupted"));
        }
        let mut code: i32 = 0;
        let mut len = 4u32;
        // SAFETY: SO_ERROR returns one i32 into the provided initialized storage.
        if unsafe { getsockopt(fd, 1, 4, (&mut code as *mut i32).cast(), &mut len) } < 0
            || code != 0
        {
            return Err(error("VM socket connection failed"));
        }
    }
    let mut cred = Cred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<Cred>() as u32;
    // SAFETY: SO_PEERCRED returns struct ucred in the correctly sized buffer.
    if unsafe { getsockopt(fd, 1, 17, (&mut cred as *mut Cred).cast(), &mut len) } < 0
        || cred.pid <= 0
        || cred.pid as u32 != pid
        || cred.uid != metadata::uid()
    {
        return Err(error("VM socket peer does not match systemd MainPID"));
    }
    stream
        .set_nonblocking(false)
        .map_err(|e| error(e.to_string()))?;
    Ok(stream)
}
pub(crate) trait Api {
    fn state(&self, path: &Path, pid: u32) -> Result<&'static str>;
    fn power_button(&self, path: &Path, pid: u32) -> Result<()>;
}
pub(crate) struct Http;
impl Api for Http {
    fn state(&self, path: &Path, pid: u32) -> Result<&'static str> {
        vm_state(path, pid)
    }
    fn power_button(&self, path: &Path, pid: u32) -> Result<()> {
        request(path, pid, "PUT", "vm.power-button").map(|_| ())
    }
}
trait ApiStream: Read + Write {
    fn timeout(&self, remaining: Duration) -> Result<()>;
}
impl ApiStream for UnixStream {
    fn timeout(&self, remaining: Duration) -> Result<()> {
        self.set_read_timeout(Some(remaining))
            .and_then(|_| self.set_write_timeout(Some(remaining)))
            .map_err(|e| error(e.to_string()))
    }
}
pub(crate) fn request(path: &Path, pid: u32, method: &str, endpoint: &str) -> Result<Vec<u8>> {
    let mut stream = connect(path, pid)?;
    exchange(&mut stream, method, endpoint)
}
pub(crate) fn disk_id(volume_id: &str) -> Result<String> {
    if !metadata::valid_id(volume_id) {
        return Err(error("invalid volume generation for disk ID"));
    }
    Ok(format!("ch-disk-{volume_id}"))
}
pub(crate) fn request_body(path: &Path, pid: u32, endpoint: &str, body: &str) -> Result<Vec<u8>> {
    let mut stream = connect(path, pid)?;
    exchange_body(&mut stream, "PUT", endpoint, body)
}
pub(crate) fn file_url(path: &Path) -> Result<String> {
    let text = option_path(path)?;
    if text
        .bytes()
        .any(|b| !b.is_ascii_alphanumeric() && !b"/._-".contains(&b))
    {
        return Err(error("snapshot paths require ASCII letters, digits, '/', '.', '_' or '-' for Cloud Hypervisor file URLs"));
    }
    Ok(format!("file://{text}"))
}
pub(crate) fn snapshot_request(path: &Path, pid: u32, destination: &Path) -> Result<()> {
    let mut stream = connect(path, pid)?;
    let body = format!(
        "{{\"destination_url\":{}}}",
        crate::output::json_string(&file_url(destination)?)
    );
    exchange_timed(
        &mut stream,
        "PUT",
        "vm.snapshot",
        &body,
        Duration::from_secs(1800),
    )
    .map(|_| ())
}
/// Refuse unmanaged live disk/network changes before pairing RAM with storage.
pub(crate) fn check_live_configuration(
    host: &HostConfig,
    config: &InstanceConfig,
    pid: u32,
) -> Result<()> {
    use crate::api_json::Value;
    let bytes = request(
        &host.runtime_dir.join(&config.name).join("api.sock"),
        pid,
        "GET",
        "vm.info",
    )?;
    let value = crate::api_json::parse(&bytes)?;
    let object = value
        .object()?
        .get("config")
        .ok_or_else(|| error("vm.info has no config"))?
        .object()?;
    let cpus = object
        .get("cpus")
        .ok_or_else(|| error("vm.info has no CPU config"))?
        .object()?;
    let memory = object
        .get("memory")
        .ok_or_else(|| error("vm.info has no memory config"))?
        .object()?;
    let number = |value: Option<&Value<'_>>, expected: u64| matches!(value,Some(Value::Atom(raw)) if raw.parse::<u64>().ok()==Some(expected));
    if !number(cpus.get("boot_vcpus"), config.compute.cpus as u64)
        || !number(cpus.get("max_vcpus"), config.compute.cpus as u64)
        || !number(
            memory.get("size"),
            volume::parse_size(&config.compute.memory)?,
        )
    {
        return Err(error(
            "live CPU/memory config differs from instance metadata",
        ));
    }
    let mut expected = Vec::new();
    for disk in &config.disk {
        let v = volume::load(host, &disk.volume)?;
        let info = volume::info(host, v);
        if !info.available {
            return Err(error("live snapshot volume is unavailable"));
        }
        expected.push((
            info.path.ok_or_else(|| error("volume path missing"))?,
            false,
        ));
    }
    if let Some(seed) = &config.cloud_init {
        expected.push((crate::cloud_init::resolve(host, seed)?, true));
    }
    let disks = object
        .get("disks")
        .ok_or_else(|| error("vm.info has no disks"))?
        .array()?;
    if disks.len() != expected.len() {
        return Err(error(
            "live disk topology differs from instance metadata; finish hotplug before suspend",
        ));
    }
    for disk in disks {
        let disk = disk.object()?;
        let index=expected.iter().position(|(path,readonly)|disk.get("path").is_some_and(|v|v.is_string(&path.to_string_lossy())) && matches!(disk.get("readonly"),Some(Value::Atom(value)) if *value==if *readonly{"true"}else{"false"}));
        if let Some(index) = index {
            expected.remove(index);
        } else {
            return Err(error(
                "live disk path/access differs from instance metadata",
            ));
        }
    }
    let networks = match object.get("net") {
        Some(Value::Array(v)) => v.as_slice(),
        Some(Value::Atom("null")) | None => &[],
        _ => return Err(error("invalid live network configuration")),
    };
    if networks.len() != config.network.len() {
        return Err(error(
            "live network topology differs from instance metadata",
        ));
    }
    for (net, expected) in networks.iter().zip(&config.network) {
        let net = net.object()?;
        if !net.get("tap").is_some_and(|v| v.is_string(&expected.tap))
            || !net.get("mac").is_some_and(|v| v.is_string(&expected.mac))
        {
            return Err(error("live TAP/MAC differs from instance metadata"));
        }
    }
    for key in [
        "devices",
        "user_devices",
        "fs",
        "pmem",
        "vsock",
        "vdpa",
        "tpm",
        "generic_vhost_user",
    ] {
        match object.get(key) {
            None | Some(Value::Atom("null")) => (),
            Some(Value::Array(v)) if v.is_empty() => (),
            _ => {
                return Err(error(
                    "live VM contains devices outside supported suspend topology",
                ))
            }
        }
    }
    Ok(())
}
fn exchange(stream: &mut impl ApiStream, method: &str, endpoint: &str) -> Result<Vec<u8>> {
    exchange_body(stream, method, endpoint, "")
}
fn exchange_body(
    stream: &mut impl ApiStream,
    method: &str,
    endpoint: &str,
    body: &str,
) -> Result<Vec<u8>> {
    exchange_timed(stream, method, endpoint, body, Duration::from_secs(2))
}
fn exchange_timed(
    stream: &mut impl ApiStream,
    method: &str,
    endpoint: &str,
    body: &str,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    stream.timeout(timeout)?;
    let request = format!("{method} /api/v1/{endpoint} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
    stream
        .write_all(request.as_bytes())
        .map_err(|_| error("cannot send Cloud Hypervisor API request"))?;
    let mut bytes = Vec::new();
    let mut header_end = None;
    let mut length = 0;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(error("Cloud Hypervisor API response timed out"));
        }
        stream.timeout(remaining)?;
        let mut chunk = [0; 4096];
        let n = stream.read(&mut chunk).map_err(|_| {
            error("cannot read Cloud Hypervisor API response (timeout or connection failure)")
        })?;
        if n == 0 {
            return Err(error("Cloud Hypervisor API response ended prematurely"));
        }
        bytes.extend_from_slice(&chunk[..n]);
        if header_end.is_none() {
            if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                if end > 16 * 1024 {
                    return Err(error("API headers exceed 16 KiB"));
                }
                length = response_length(&bytes[..end])?;
                header_end = Some(end + 4);
            } else if bytes.len() > 16 * 1024 {
                return Err(error("API headers exceed 16 KiB"));
            }
        }
        if let Some(end) = header_end {
            if length > 1024 * 1024 {
                return Err(error("API body exceeds 1 MiB"));
            }
            if bytes.len() >= end + length {
                return Ok(bytes[end..end + length].to_vec());
            }
        }
    }
}
fn response_length(header: &[u8]) -> Result<usize> {
    let text = std::str::from_utf8(header).map_err(|_| error("invalid API response header"))?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .ok_or_else(|| error("missing API response status"))?;
    let words: Vec<_> = status.split_whitespace().collect();
    if words.len() < 2 || !["HTTP/1.1", "HTTP/1.0"].contains(&words[0]) {
        return Err(error("invalid API response status"));
    }
    let code = words[1]
        .parse::<u16>()
        .map_err(|_| error("invalid API response code"))?;
    if ![200, 204].contains(&code) {
        return Err(error(format!(
            "Cloud Hypervisor API request failed (HTTP {code}); response body omitted"
        )));
    }
    let mut length = None;
    for line in lines {
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| error("invalid API response header"))?;
        if key.eq_ignore_ascii_case("transfer-encoding") {
            return Err(error("unsupported API transfer encoding"));
        }
        if key.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                return Err(error("duplicate API content-length"));
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| error("invalid API content-length"))?,
            );
        }
    }
    match (code, length) {
        (204, None) => Ok(0),
        (_, Some(n)) => Ok(n),
        _ => Err(error("missing API content-length")),
    }
}
pub(crate) fn vm_state(path: &Path, pid: u32) -> Result<&'static str> {
    state_field(&request(path, pid, "GET", "vm.info")?)
}

// Parse structure with bounded depth and duplicate-key rejection.
fn state_field(bytes: &[u8]) -> Result<&'static str> {
    let value = crate::api_json::parse(bytes)?;
    let object = value.object()?;
    let state = object
        .get("state")
        .ok_or_else(|| error("vm.info has no top-level state"))?;
    for (raw, state_name) in [
        ("Running", "running"),
        ("Created", "starting"),
        ("Paused", "paused"),
        ("Shutdown", "shutdown"),
    ] {
        if state.is_string(raw) {
            return Ok(state_name);
        }
    }
    Err(error("unknown vm.info state"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::net::UnixListener;
    #[test]
    fn windows_cpu_options_must_be_advertised_before_starting() {
        let base = "--firmware --kernel --initramfs --disk image_type= --net --api-socket --serial socket= --console";
        check_advertised_capabilities(base, false).unwrap();
        for suffix in ["", " kvm_hyperv=", " max_phys_bits="] {
            assert!(check_advertised_capabilities(&format!("{base}{suffix}"), true).is_err());
        }
        check_advertised_capabilities(&format!("{base} kvm_hyperv= max_phys_bits="), true).unwrap();
    }
    #[test]
    fn parses_only_top_level_vm_state_with_bounded_json() {
        assert_eq!(state_field(br#"{"config":{"state":"Shutdown","cmdline":"state\":\"Paused"},"state":"Running"}"#).unwrap(),"running");
        for (raw, expected) in [
            ("Created", "starting"),
            ("Paused", "paused"),
            ("Shutdown", "shutdown"),
        ] {
            assert_eq!(
                state_field(
                    format!("{{\"state\":\"{raw}\",\"other\":[null,true,2,\"a\\n\\u0041\"]}}")
                        .as_bytes()
                )
                .unwrap(),
                expected
            );
        }
        for raw in [
            r#"{"config":{"state":"Running"}}"#,
            r#"{"state":"Running","state":"Paused"}"#,
            r#"{"state":"Unexpected"}"#,
            r#"{"state":"Running"}garbage"#,
            r#"{"state":null}"#,
        ] {
            assert!(state_field(raw.as_bytes()).is_err(), "{raw}");
        }
        let nested = format!(
            "{{\"config\":{}0{},\"state\":\"Running\"}}",
            "[".repeat(80),
            "]".repeat(80)
        );
        assert!(state_field(nested.as_bytes()).is_err());
    }
    #[test]
    fn api_headers_reject_errors_duplicate_lengths_and_oversized_protocol_variants() {
        assert_eq!(response_length(b"HTTP/1.1 204 No Content").unwrap(), 0);
        assert_eq!(
            response_length(b"HTTP/1.1 200 OK\r\nContent-Length: 10").unwrap(),
            10
        );
        for raw in [
            b"HTTP/1.1 500 Error\r\nContent-Length: 0".as_slice(),
            b"HTTP/1.1 200 OK",
            b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1",
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked",
        ] {
            assert!(response_length(raw).is_err());
        }
    }
    #[test]
    #[ignore = "requires AF_UNIX sockets; sandbox blocks socket creation"]
    fn unix_http_validates_peer_and_reads_split_headers_without_waiting_for_eof() {
        let root =
            std::env::temp_dir().join(format!("chadm-api-{}", metadata::random_id().unwrap()));
        fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
        let path = root.join("api.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut bytes = [0; 1024];
            let n = stream.read(&mut bytes).unwrap();
            let request = std::str::from_utf8(&bytes[..n]).unwrap();
            assert!(request.starts_with("GET /api/v1/vm.info HTTP/1.1"));
            let body = b"{\"config\":{},\"state\":\"Running\"}";
            let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len());
            stream.write_all(&header.as_bytes()[..8]).unwrap();
            stream.write_all(&header.as_bytes()[8..]).unwrap();
            stream.write_all(body).unwrap();
        });
        assert_eq!(vm_state(&path, std::process::id()).unwrap(), "running");
        server.join().unwrap();
        // The listening socket remains after the thread exited; peer PID mismatch
        // is checked before any request could be delivered to a replacement server.
        let listener = UnixListener::bind(root.join("other.sock")).unwrap();
        assert!(request(
            &root.join("other.sock"),
            std::process::id() + 1,
            "GET",
            "vm.info"
        )
        .unwrap_err()
        .to_string()
        .contains("peer"));
        drop(listener);
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    struct Stream {
        input: std::io::Cursor<Vec<u8>>,
        output: Vec<u8>,
    }
    impl Read for Stream {
        fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
            let len = b.len().min(3);
            self.input.read(&mut b[..len])
        }
    }
    impl Write for Stream {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.output.extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl ApiStream for Stream {
        fn timeout(&self, _: Duration) -> Result<()> {
            Ok(())
        }
    }
    fn stream(raw: &[u8]) -> Stream {
        Stream {
            input: std::io::Cursor::new(raw.to_vec()),
            output: Vec::new(),
        }
    }
    #[test]
    fn fragmented_http_requests_and_premature_or_oversized_responses() {
        let mut s = stream(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}EXTRA");
        assert_eq!(exchange(&mut s, "GET", "vm.info").unwrap(), b"{}");
        assert!(std::str::from_utf8(&s.output)
            .unwrap()
            .starts_with("GET /api/v1/vm.info HTTP/1.1\r\n"));
        let mut s = stream(b"HTTP/1.1 204 No Content\r\n\r\n");
        assert!(exchange(&mut s, "PUT", "vm.power-button")
            .unwrap()
            .is_empty());
        for raw in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nshort".as_slice(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 1048577\r\n\r\n",
            b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\n\r\n",
        ] {
            assert!(exchange(&mut stream(raw), "GET", "vm.info").is_err());
        }
    }
    #[test]
    fn hotplug_request_has_exact_json_body_and_byte_length() {
        let mut s = stream(b"HTTP/1.1 204 No Content\r\n\r\n");
        let body = r#"{"path":"/storage/中文\\disk\".raw","id":"disk0","image_type":"Raw"}"#;
        exchange_body(&mut s, "PUT", "vm.add-disk", body).unwrap();
        let request = std::str::from_utf8(&s.output).unwrap();
        assert!(request.starts_with("PUT /api/v1/vm.add-disk HTTP/1.1\r\n"));
        assert!(request.contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(request.ends_with(&format!("\r\n\r\n{body}")));
    }
}
