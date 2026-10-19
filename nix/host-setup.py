"""One-shot host provisioning. Never write VM configs or remove storage."""
import contextlib
import fcntl
import json
import os
from pathlib import Path
import re
import stat
import subprocess
import sys
import tempfile
import tomllib


class SetupError(Exception):
    pass


def safe_path(value):
    path = Path(value)
    if not path.is_absolute() or str(path) != str(value) or ".." in path.parts or path == Path("/"):
        raise SetupError(f"unsafe path: {value!r}")
    return path


def ensure_dir(path):
    path = safe_path(path)
    current = Path("/")
    root_uid = current.stat().st_uid
    for part in path.parts[1:]:
        current /= part
        try:
            info = current.lstat()
        except FileNotFoundError:
            current.mkdir(mode=0o700)
            info = current.lstat()
        sticky = current != path and info.st_uid == root_uid and info.st_mode & stat.S_ISVTX
        if (not stat.S_ISDIR(info.st_mode) or info.st_uid not in (root_uid, os.geteuid())
                or (info.st_mode & 0o022 and not sticky)):
            raise SetupError(f"unsafe directory: {current}")
    return path


def open_managed(path, create=False):
    flags = os.O_RDWR if create else os.O_RDONLY
    fd = os.open(path, flags | os.O_NOFOLLOW | os.O_NONBLOCK | (os.O_CREAT if create else 0), 0o600)
    info = os.fstat(fd)
    if (not stat.S_ISREG(info.st_mode) or info.st_uid != os.geteuid()
            or info.st_nlink != 1 or info.st_mode & 0o022):
        os.close(fd)
        raise SetupError(f"unsafe managed file: {path}")
    return os.fdopen(fd, "r+b" if create else "rb")


def existing_bytes(path):
    try:
        with open_managed(path) as stream:
            return stream.read()
    except FileNotFoundError:
        return None


def publish(path, contents, mode):
    # Parent and old file must already have passed ownership/alias checks.
    fd, temporary = tempfile.mkstemp(prefix=".chadm-setup-", dir=path.parent)
    try:
        with os.fdopen(fd, "wb") as stream:
            os.fchmod(stream.fileno(), mode)
            stream.write(contents)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(temporary, path)
        directory = os.open(path.parent, os.O_RDONLY | os.O_DIRECTORY)
        try:
            os.fsync(directory)
        finally:
            os.close(directory)
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


@contextlib.contextmanager
def lock(path):
    with open_managed(path, create=True) as stream:
        try:
            fcntl.flock(stream, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as error:
            raise SetupError("another chadm command is active; retry host setup later") from error
        yield


def run(args):
    result = subprocess.run(args, text=True, capture_output=True, timeout=120, check=False)
    if result.returncode:
        raise SetupError(f"command failed: {args[0]}: {result.stderr.strip()}")
    return result.stdout


def chadm(settings, config_path, *args):
    return json.loads(run([settings["chadm"], "--config", str(config_path), *args]))


def instance_names(data):
    directory = data / "instances"
    if not os.path.lexists(directory):
        return []
    ensure_dir(directory)
    names = sorted(entry.name for entry in directory.iterdir())
    if any(re.fullmatch(r"[a-z][a-z0-9-]{0,62}", name) is None for name in names):
        raise SetupError("invalid instance metadata; refusing host changes")
    return names


@contextlib.contextmanager
def stopped_guard(settings, config_path, host):
    data = ensure_dir(host["data_dir"])
    names = instance_names(data)
    # Follow the CLI's operation-lock -> global-lock order. Keep the operation
    # locks across state queries, so suspend/resume cannot invalidate the query.
    with contextlib.ExitStack() as stack:
        for name in names:
            for base in ("operations", "console-sessions"):
                directory = ensure_dir(data / base / name)
                stack.enter_context(lock(directory / ".lock"))
        if names:
            # The CLI acquires the global lock itself.
            instances = chadm(settings, config_path, "instance", "list", "--json")
            if sorted(vm["name"] for vm in instances) != names or any(vm["state"] != "stopped" for vm in instances):
                raise SetupError("stop/resume all instances before changing host settings or firmware")
        with lock(data / ".lock"):
            if instance_names(data) != names:
                raise SetupError("instance inventory changed; retry host setup")
            for name in names:
                text = run([settings["systemctl"], "--system", "show",
                            f"chadm-instance@{name}.service",
                            "--property=ActiveState,MainPID,ControlPID,Job"])
                properties = dict(line.split("=", 1) for line in text.splitlines())
                if (properties.get("ActiveState") not in ("inactive", "failed")
                        or properties.get("MainPID") != "0" or properties.get("ControlPID") != "0"
                        or properties.get("Job") not in ("", "0")):
                    raise SetupError(f"instance {name!r} has an active systemd unit or job")
            yield


def populated(host):
    data = ensure_dir(host["data_dir"])
    runtime = ensure_dir(host["runtime_dir"])
    if any(runtime.iterdir()):
        return True
    for name in ("instances", "repos", "volumes", "images", "snapshots"):
        directory = data / name
        if os.path.lexists(directory):
            ensure_dir(directory)
            if any(directory.iterdir()):
                return True
    return False


def setup(settings, config_path=Path("/etc/chadm/config.toml")):
    config_path = safe_path(config_path)
    ensure_dir(config_path.parent)
    desired = {"data_dir": str(safe_path(settings["dataDir"])),
               "runtime_dir": str(safe_path(settings["runtimeDir"])),
               "cloud_hypervisor": "cloud-hypervisor"}
    ensure_dir(Path(desired["data_dir"]))
    ensure_dir(Path(desired["runtime_dir"]))
    with lock(config_path.parent / ".setup.lock"):
        contents = existing_bytes(config_path)
        current = tomllib.loads(contents.decode()) if contents is not None else None
        if current is not None:
            if set(current) - {"data_dir", "runtime_dir", "cloud_hypervisor", "firmware"}:
                raise SetupError("unknown host config fields; refusing replacement")
            current = {"data_dir": "/var/lib/chadm", "runtime_dir": "/run/chadm",
                       "cloud_hypervisor": "cloud-hypervisor", **current}
            if settings["manageHostConfig"]:
                changed_directories = any(current[key] != desired[key] for key in ("data_dir", "runtime_dir"))
                if changed_directories and (populated(current) or populated(desired)):
                    raise SetupError("data/runtime directories contain resources; automatic migration is not supported")
                if any(current[key] != desired[key] for key in desired):
                    with stopped_guard(settings, config_path, current):
                        publish(config_path, encode_host(desired), 0o600)
            elif any(current[key] != desired[key] for key in ("data_dir", "runtime_dir")):
                raise SetupError("unmanaged host config directories differ from Nix options")
        else:
            publish(config_path, encode_host(desired), 0o600)
        # Firmware is copied, not linked. Identical bytes retain inode/mtime so
        # an ordinary rebuild cannot invalidate saved-state boot-file identity.
        if settings["firmware"] is not None:
            firmware = Path(desired["data_dir"]) / "CLOUDHV.fd"
            new_bytes = Path(settings["firmware"]).read_bytes()
            if not new_bytes:
                raise SetupError("firmware source is empty")
            previous = existing_bytes(firmware)
            if previous != new_bytes:
                effective = desired if settings["manageHostConfig"] or current is None else current
                with stopped_guard(settings, config_path, effective):
                    publish(firmware, new_bytes, 0o644)
        # Initialize a valid chadm global lock before list on a new empty data dir.
        with lock(Path(desired["data_dir"]) / ".lock"):
            pass
        existing = {repo["name"]: repo for repo in chadm(settings, config_path, "repo", "list", "--json")}
        # Check all existing declarations before creating anything.
        for repo in settings["repos"]:
            previous = existing.get(repo["name"])
            if previous is not None:
                key = "path" if repo["driver"] == "file" else "pool"
                if previous["driver"] != repo["driver"] or previous[key] != repo[key] or not previous["available"]:
                    raise SetupError(f"repo {repo['name']!r} differs from the declaration or is unavailable; no migration/adoption is performed")
        for repo in settings["repos"]:
            if repo["name"] not in existing:
                key = "path" if repo["driver"] == "file" else "pool"
                run([settings["chadm"], "--config", str(config_path), "repo", "create",
                     "--name", repo["name"], "--driver", repo["driver"], f"--{key}", repo[key]])


def encode_host(host):
    # JSON quoted strings are valid TOML basic strings for these ASCII paths.
    return "".join(f"{key} = {json.dumps(value)}\n" for key, value in host.items()).encode()


if __name__ == "__main__":
    try:
        with open(sys.argv[1], encoding="utf-8") as stream:
            setup(json.load(stream))
    except (SetupError, OSError, ValueError, KeyError, subprocess.SubprocessError) as error:
        print(f"chadm host setup: {error}", file=sys.stderr)
        sys.exit(1)
