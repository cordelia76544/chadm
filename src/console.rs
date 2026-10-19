//! Small foreground serial client. No supervisor, PTY allocation or guest API commands.
use crate::{
    config::HostConfig,
    error::{Error, Result},
    metadata,
};
use std::io::{self, Read, Write};
use std::os::{fd::AsRawFd, unix::net::UnixStream};
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

fn error(message: impl Into<String>) -> Error {
    Error::Resource(message.into())
}
fn io_error(action: &str, e: io::Error) -> Error {
    error(format!("console {action}: {e}"))
}

// Linux glibc/musl termios ABI on x86_64 and aarch64.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Termios {
    input: u32,
    output: u32,
    control: u32,
    local: u32,
    line: u8,
    chars: [u8; 32],
    input_speed: u32,
    output_speed: u32,
}
#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
unsafe extern "C" {
    fn isatty(fd: i32) -> i32;
    fn tcgetattr(fd: i32, state: *mut Termios) -> i32;
    fn tcsetattr(fd: i32, action: i32, state: *const Termios) -> i32;
    fn cfmakeraw(state: *mut Termios);
    fn poll(fds: *mut PollFd, count: usize, timeout: i32) -> i32;
    fn read(fd: i32, buffer: *mut u8, size: usize) -> isize;
    fn signal(number: i32, handler: usize) -> usize;
    fn siginterrupt(number: i32, interrupt: i32) -> i32;
}
struct Terminal {
    fd: i32,
    original: Option<Termios>,
}
impl Terminal {
    fn raw(fd: i32) -> Result<Self> {
        let mut guard = Self { fd, original: None };
        // SAFETY: isatty only inspects a live standard input descriptor.
        if unsafe { isatty(fd) } == 0 {
            return Ok(guard);
        }
        let mut original = Termios {
            input: 0,
            output: 0,
            control: 0,
            local: 0,
            line: 0,
            chars: [0; 32],
            input_speed: 0,
            output_speed: 0,
        };
        // SAFETY: initialized correctly aligned Linux termios storage.
        if unsafe { tcgetattr(fd, &mut original) } != 0 {
            return Err(io_error(
                "read terminal settings",
                io::Error::last_os_error(),
            ));
        }
        guard.original = Some(original);
        let mut raw = original;
        // SAFETY: cfmakeraw mutates one valid termios; TCSANOW applies it to this tty.
        unsafe {
            cfmakeraw(&mut raw);
        }
        if unsafe { tcsetattr(fd, 0, &raw) } != 0 {
            return Err(io_error("enter raw mode", io::Error::last_os_error()));
        }
        Ok(guard)
    }
}
impl Drop for Terminal {
    fn drop(&mut self) {
        if let Some(original) = &self.original {
            // SAFETY: saved settings belong to the same live terminal descriptor.
            for _ in 0..3 {
                if unsafe { tcsetattr(self.fd, 0, original) } == 0 {
                    break;
                }
                if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
        }
    }
}
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);
extern "C" fn interrupt(number: i32) {
    INTERRUPTED.store(number, Ordering::Relaxed);
}
struct Signals(Vec<(i32, usize)>);
impl Signals {
    fn install() -> Result<Self> {
        INTERRUPTED.store(0, Ordering::Relaxed);
        let mut guard = Self(Vec::new());
        for number in [1, 2, 3, 15] {
            // SAFETY: process-local Linux signal handler performs only a lock-free atomic store.
            let previous = unsafe { signal(number, interrupt as *const () as usize) };
            if previous == usize::MAX {
                return Err(io_error(
                    "install signal handler",
                    io::Error::last_os_error(),
                ));
            }
            guard.0.push((number, previous));
            // SAFETY: make blocking terminal I/O return EINTR so guards can restore settings.
            if unsafe { siginterrupt(number, 1) } != 0 {
                return Err(io_error(
                    "configure signal handling",
                    io::Error::last_os_error(),
                ));
            }
        }
        Ok(guard)
    }
}
impl Drop for Signals {
    fn drop(&mut self) {
        for &(number, previous) in &self.0 {
            // SAFETY: restores the saved Linux signal handler.
            unsafe {
                signal(number, previous);
            }
        }
    }
}
fn input_prefix(input: &[u8]) -> (&[u8], bool) {
    match input.iter().position(|byte| *byte == 0x1d) {
        Some(index) => (&input[..index], true),
        None => (input, false),
    }
}

pub fn connect(host: &HostConfig, name: &str) -> Result<()> {
    host.validate()?;
    metadata::validate_name("instance", name)?;
    let (mut stream, _session) = crate::lifecycle::console_stream(host, name)?;
    stream
        .set_nonblocking(true)
        .map_err(|e| io_error("configure socket", e))?;
    eprintln!("Connected to {name}; press Ctrl+] to disconnect.");
    let _signals = Signals::install()?;
    let _terminal = Terminal::raw(0)?;
    relay(&mut stream, 0, &mut io::stdout().lock())
}

fn write_output(output: &mut impl Write, mut bytes: &[u8]) -> Result<()> {
    while !bytes.is_empty() {
        if INTERRUPTED.load(Ordering::Relaxed) != 0 {
            return Err(error("console output interrupted"));
        }
        match output.write(bytes) {
            Ok(0) => return Err(error("console output closed")),
            Ok(count) => bytes = &bytes[count..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io_error("write output", e)),
        }
    }
    output.flush().map_err(|e| io_error("flush output", e))
}

fn relay(stream: &mut UnixStream, input: i32, output: &mut impl Write) -> Result<()> {
    let mut pending = Vec::new();
    let mut written = 0;
    let mut detach = None;
    let mut buffer = [0u8; 4096];
    loop {
        let signal = INTERRUPTED.load(Ordering::Relaxed);
        if signal != 0 {
            return Err(error(format!("console interrupted by signal {signal}")));
        }
        if let Some(deadline) = detach {
            if written == pending.len() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(error("console timed out sending input before disconnect"));
            }
        }
        let mut fds = [
            PollFd {
                fd: if detach.is_none() && pending.is_empty() {
                    input
                } else {
                    -1
                },
                events: 1,
                revents: 0,
            },
            PollFd {
                fd: stream.as_raw_fd(),
                events: 1 | if written < pending.len() { 4 } else { 0 },
                revents: 0,
            },
        ];
        // SAFETY: two live pollfd entries, bounded wait lets signal handling finish.
        let ready = unsafe { poll(fds.as_mut_ptr(), fds.len(), 100) };
        if ready < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(io_error("poll", e));
        }
        if fds.iter().any(|fd| fd.revents & 32 != 0) {
            return Err(error("console descriptor became invalid"));
        }
        if fds[0].revents & 8 != 0 {
            return Err(error("console input became unavailable"));
        }
        if fds[1].revents & (1 | 8 | 16) != 0 {
            match stream.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => {
                    write_output(output, &buffer[..count])?;
                }
                Err(e)
                    if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                        .contains(&e.kind()) => {}
                Err(e) => return Err(io_error("read socket", e)),
            }
        }
        if fds[0].revents & (1 | 16) != 0 {
            // SAFETY: valid input fd and initialized writable buffer; poll reported readiness/EOF.
            let count = unsafe { read(input, buffer.as_mut_ptr(), buffer.len()) };
            if count < 0 {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::Interrupted {
                    return Err(io_error("read input", e));
                }
            } else {
                let (prefix, exit) = input_prefix(&buffer[..count as usize]);
                pending.extend_from_slice(prefix);
                if exit || count == 0 {
                    detach = Some(Instant::now() + Duration::from_secs(2));
                }
            }
        }
        if written < pending.len() {
            match stream.write(&pending[written..]) {
                Ok(0) => return Err(error("console socket closed while sending input")),
                Ok(count) => {
                    written += count;
                    if written == pending.len() {
                        pending.clear();
                        written = 0;
                    }
                }
                Err(e)
                    if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted]
                        .contains(&e.kind()) => {}
                Err(e) => return Err(io_error("write socket", e)),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn escape_is_local_and_guest_control_keys_are_preserved() {
        assert_eq!(
            input_prefix(b"hello\x03\x04\x1a"),
            (&b"hello\x03\x04\x1a"[..], false)
        );
        assert_eq!(
            input_prefix(b"prefix\x1dnever-send"),
            (&b"prefix"[..], true)
        );
        assert_eq!(input_prefix(b"\x1d"), (&b""[..], true));
    }
    fn pty() -> (std::fs::File, std::fs::File) {
        use std::os::fd::FromRawFd;
        unsafe extern "C" {
            fn openpty(
                master: *mut i32,
                slave: *mut i32,
                name: *mut u8,
                termios: *const std::ffi::c_void,
                winsize: *const std::ffi::c_void,
            ) -> i32;
        }
        let (mut master, mut slave) = (-1, -1);
        // SAFETY: valid fd outputs; null pointers request default PTY settings.
        assert_eq!(
            unsafe {
                openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    std::ptr::null(),
                )
            },
            0
        );
        // SAFETY: these fresh descriptors transfer ownership exactly once.
        unsafe {
            (
                std::fs::File::from_raw_fd(master),
                std::fs::File::from_raw_fd(slave),
            )
        }
    }
    fn attributes(fd: i32) -> Termios {
        let mut result = Termios {
            input: 0,
            output: 0,
            control: 0,
            local: 0,
            line: 0,
            chars: [0; 32],
            input_speed: 0,
            output_speed: 0,
        };
        // SAFETY: correctly aligned output storage and live tty descriptor.
        assert_eq!(unsafe { tcgetattr(fd, &mut result) }, 0);
        result
    }
    #[test]
    fn terminal_raw_mode_restores_all_settings_on_success_and_error() {
        let (_master, slave) = pty();
        let fd = slave.as_raw_fd();
        let before = attributes(fd);
        {
            let _guard = Terminal::raw(fd).unwrap();
            let raw = attributes(fd);
            assert_eq!(raw.local & (1 | 2 | 8), 0); // ISIG, ICANON, ECHO
            assert_eq!(raw.output & 1, 0); // OPOST
        }
        assert_eq!(attributes(fd), before);
        let failure = (|| -> Result<()> {
            let _guard = Terminal::raw(fd)?;
            Err(error("simulated connection failure"))
        })();
        assert!(failure.is_err());
        assert_eq!(attributes(fd), before);
        let null = std::fs::File::open("/dev/null").unwrap();
        assert!(Terminal::raw(null.as_raw_fd()).unwrap().original.is_none());
    }
    #[test]
    #[ignore = "requires AF_UNIX sockets; sandbox blocks socket creation"]
    fn serial_relay_preserves_bytes_and_detaches_without_guest_escape() {
        let (mut master, slave) = pty();
        let before = attributes(slave.as_raw_fd());
        let (mut client, mut peer) = UnixStream::pair().unwrap();
        client.set_nonblocking(true).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        peer.write_all(b"guest-output\r\n").unwrap();
        let mut output = Vec::new();
        {
            let _terminal = Terminal::raw(slave.as_raw_fd()).unwrap();
            master.write_all(b"guest-input\x03\x1dtrailing").unwrap();
            relay(&mut client, slave.as_raw_fd(), &mut output).unwrap();
        }
        assert_eq!(attributes(slave.as_raw_fd()), before);
        assert_eq!(output, b"guest-output\r\n");
        let mut received = [0u8; 12];
        peer.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"guest-input\x03");
        drop(client);
        assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
    }
    #[test]
    fn terminal_restores_after_external_sigterm() {
        const CHILD: &str = "CHADM_TEST_TERMINAL_SIGNAL_CHILD";
        if std::env::var(CHILD).as_deref() == Ok("1") {
            let before = attributes(0);
            {
                let _signals = Signals::install().unwrap();
                let _terminal = Terminal::raw(0).unwrap();
                println!("TERMINAL_SIGNAL_READY");
                let deadline = Instant::now() + Duration::from_secs(5);
                while INTERRUPTED.load(Ordering::Relaxed) == 0 && Instant::now() < deadline {
                    let mut fd = PollFd {
                        fd: 0,
                        events: 1,
                        revents: 0,
                    };
                    // SAFETY: one initialized pollfd for the inherited live PTY.
                    unsafe {
                        poll(&mut fd, 1, 100);
                    }
                }
                assert_eq!(INTERRUPTED.load(Ordering::Relaxed), 15);
            }
            assert_eq!(attributes(0), before);
            return;
        }
        use std::io::BufRead;
        use std::process::{Command, Stdio};
        unsafe extern "C" {
            fn kill(pid: i32, signal: i32) -> i32;
        }
        let (_master, slave) = pty();
        let before = attributes(slave.as_raw_fd());
        // Ensure the test's private PTY is restored even if its child fails.
        let _restore = Terminal {
            fd: slave.as_raw_fd(),
            original: Some(before),
        };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "console::tests::terminal_restores_after_external_sigterm",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            assert_ne!(
                stdout.read_line(&mut line).unwrap(),
                0,
                "child exited before entering raw mode"
            );
            if line.contains("TERMINAL_SIGNAL_READY") {
                break;
            }
        }
        assert_eq!(attributes(slave.as_raw_fd()).local & (1 | 2 | 8), 0);
        // SAFETY: signal only this freshly spawned, still-live test subprocess.
        assert_eq!(unsafe { kill(child.id() as i32, 15) }, 0);
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("console signal cleanup timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(attributes(slave.as_raw_fd()), before);
    }
}
