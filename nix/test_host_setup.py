"""Host provisioning integration tests; use real chadm with isolated metadata."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import tomllib
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("host_setup", Path(__file__).with_name("host-setup.py"))
setup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(setup)
BINARY = os.environ.get("CHADM_TEST_BINARY", str(Path(__file__).resolve().parents[1] / "target/release/chadm"))


class HostSetupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="chadm-host-setup-")
        self.root = Path(self.temp.name)
        self.config = self.root / "etc/config.toml"
        self.source = self.root / "firmware.fd"
        self.source.write_bytes(b"test firmware")
        self.source.chmod(0o600)
        self.settings = {
            "dataDir": str(self.root / "data"), "runtimeDir": str(self.root / "run"),
            "manageHostConfig": True, "firmware": str(self.source),
            "chadm": BINARY, "systemctl": "/never-execute-systemctl-in-unit-test",
            "repos": [{"name": "local", "driver": "file", "path": str(self.root / "disks"), "pool": None}],
        }

    def tearDown(self):
        self.temp.cleanup()

    def apply(self):
        setup.setup(self.settings, self.config)

    def cli(self, *args):
        return subprocess.run([BINARY, "--config", str(self.config), *args],
                              capture_output=True, text=True, check=True).stdout

    def test_real_file_repo_idempotence_and_firmware_inode(self):
        self.apply()
        repo = json.loads(self.cli("repo", "show", "local", "--json"))
        self.assertTrue(repo["available"])
        config_inode = self.config.stat().st_ino
        firmware = self.root / "data/CLOUDHV.fd"
        first = firmware.stat()
        self.apply()
        self.assertEqual(json.loads(self.cli("repo", "show", "local", "--json"))["id"], repo["id"])
        self.assertEqual(self.config.stat().st_ino, config_inode)
        self.assertEqual((firmware.stat().st_ino, firmware.stat().st_mtime_ns), (first.st_ino, first.st_mtime_ns))
        self.assertFalse(firmware.is_symlink())
        self.assertEqual(firmware.read_bytes(), self.source.read_bytes())
        self.assertEqual(self.config.stat().st_mode & 0o777, 0o600)

    def test_removed_repo_declaration_retains_real_volume(self):
        self.apply()
        self.cli("volume", "create", "--name", "test", "--repo", "local", "--size", "2M")
        self.settings["repos"] = []
        self.apply()
        self.assertTrue(json.loads(self.cli("volume", "show", "test", "--json"))["available"])

    def test_rebuild_preserves_cli_created_vm_config_and_disks(self):
        self.apply()
        image = self.root / "base.raw"
        with image.open("wb") as stream:
            stream.truncate(1 << 20)
        self.cli("image", "create", "--name", "base", "--repo", "local", "--path", str(image))
        self.cli("instance", "create", "--name", "test01", "--image", "base",
                 "--boot", "firmware", "--volume", "root,size=2M", "--no-cloud-init")
        config = self.root / "data/instances/test01/config.toml"
        original = config.read_bytes()
        disk = Path(json.loads(self.cli("volume", "show", "test01-root", "--json"))["path"])
        before = disk.stat()
        self.apply()
        self.assertEqual(config.read_bytes(), original)
        self.assertEqual((disk.stat().st_ino, disk.stat().st_mtime_ns), (before.st_ino, before.st_mtime_ns))

    def test_existing_repo_mismatch_cannot_move_or_delete_storage(self):
        self.apply()
        self.settings["repos"][0]["path"] = str(self.root / "other")
        with self.assertRaisesRegex(setup.SetupError, "differs"):
            self.apply()
        self.assertFalse((self.root / "other").exists())
        self.assertTrue(json.loads(self.cli("repo", "show", "local", "--json"))["available"])

    def test_cloudinit_repo_and_host_paths_containing_shell_characters(self):
        self.settings["repos"].append({"name": "cloudinit", "driver": "file", "path": str(self.root / "seed '$(touch injected)'"), "pool": None})
        self.apply()
        self.assertEqual(json.loads(self.cli("repo", "show", "cloudinit", "--json"))["kind"], "cloudinit")
        self.assertFalse((self.root / "injected").exists())

    def test_directory_change_with_existing_resources_is_rejected(self):
        self.apply()
        original = self.config.read_bytes()
        self.settings["dataDir"] = str(self.root / "new-data")
        with self.assertRaisesRegex(setup.SetupError, "migration"):
            self.apply()
        self.assertEqual(self.config.read_bytes(), original)

    def test_unmanaged_config_keeps_custom_hypervisor(self):
        self.settings["manageHostConfig"] = False
        self.apply()
        self.config.write_text(self.config.read_text().replace('"cloud-hypervisor"', '"/opt/cloud-hypervisor"'))
        original = self.config.read_bytes()
        self.apply()
        self.assertEqual(self.config.read_bytes(), original)

    def test_symlink_and_hardlink_config_are_rejected(self):
        self.apply()
        target = self.root / "outside"
        self.config.rename(target)
        for link in (lambda: self.config.symlink_to(target), lambda: os.link(target, self.config)):
            link()
            with self.assertRaises((OSError, setup.SetupError)):
                self.apply()
            self.config.unlink()
        self.assertEqual(tomllib.loads(target.read_text())["data_dir"], self.settings["dataDir"])

    def test_symlink_parent_directory_and_firmware_are_rejected(self):
        self.apply()
        firmware = self.root / "data/CLOUDHV.fd"
        firmware.unlink()
        firmware.symlink_to(self.source)
        with self.assertRaises((OSError, setup.SetupError)):
            self.apply()
        self.assertEqual(self.source.read_bytes(), b"test firmware")
        new_root = self.root / "alias"
        new_root.symlink_to(self.root / "data", target_is_directory=True)
        self.settings["dataDir"] = str(new_root / "new")
        with self.assertRaisesRegex(setup.SetupError, "unsafe directory"):
            self.apply()

    def test_running_or_suspended_instances_block_firmware_change(self):
        self.apply()
        vm_dir = self.root / "data/instances/test01"
        vm_dir.mkdir(parents=True, mode=0o700)
        vm_dir.parent.chmod(0o700)
        self.source.write_bytes(b"new firmware")
        for state in ("running", "suspended", "restoring", "checkpoint-pending", "unknown"):
            with patch.object(setup, "chadm", return_value=[{"name": "test01", "state": state}]):
                with self.assertRaisesRegex(setup.SetupError, "stop/resume"):
                    self.apply()
        self.assertEqual((self.root / "data/CLOUDHV.fd").read_bytes(), b"test firmware")

    def test_global_lock_prevents_firmware_update(self):
        self.apply()
        self.source.write_bytes(b"new firmware")
        with setup.lock(self.root / "data/.lock"):
            with self.assertRaisesRegex(setup.SetupError, "another chadm"):
                self.apply()
        self.assertEqual((self.root / "data/CLOUDHV.fd").read_bytes(), b"test firmware")
        self.apply()
        self.assertEqual((self.root / "data/CLOUDHV.fd").read_bytes(), b"new firmware")

    def test_stopped_instance_with_pending_systemd_job_blocks_update(self):
        self.apply()
        vm_dir = self.root / "data/instances/test01"
        vm_dir.mkdir(parents=True, mode=0o700)
        vm_dir.parent.chmod(0o700)
        self.source.write_bytes(b"new firmware")
        with patch.object(setup, "chadm", return_value=[{"name": "test01", "state": "stopped"}]), patch.object(
                setup, "run", return_value="ActiveState=inactive\nMainPID=0\nControlPID=0\nJob=42\n"):
            with self.assertRaisesRegex(setup.SetupError, "active systemd unit or job"):
                self.apply()
        self.assertEqual((self.root / "data/CLOUDHV.fd").read_bytes(), b"test firmware")

    def test_active_instance_operation_blocks_update_without_deadlocking(self):
        self.apply()
        vm_dir = self.root / "data/instances/test01"
        vm_dir.mkdir(parents=True, mode=0o700)
        vm_dir.parent.chmod(0o700)
        operation = setup.ensure_dir(self.root / "data/operations/test01")
        self.source.write_bytes(b"new firmware")
        with setup.lock(operation / ".lock"), patch.object(
                setup, "chadm", return_value=[{"name": "test01", "state": "stopped"}]):
            with self.assertRaisesRegex(setup.SetupError, "another chadm"):
                self.apply()
        self.assertEqual((self.root / "data/CLOUDHV.fd").read_bytes(), b"test firmware")

    def test_instance_operation_is_locked_before_state_query(self):
        self.apply()
        vm_dir = self.root / "data/instances/test01"
        vm_dir.mkdir(parents=True, mode=0o700)
        vm_dir.parent.chmod(0o700)
        self.source.write_bytes(b"new firmware")
        def query(*args):
            with self.assertRaisesRegex(setup.SetupError, "another chadm"):
                with setup.lock(self.root / "data/operations/test01/.lock"):
                    self.fail("lifecycle operation could race the state query")
            return [{"name": "test01", "state": "suspended"}]
        with patch.object(setup, "chadm", side_effect=query):
            with self.assertRaisesRegex(setup.SetupError, "stop/resume"):
                self.apply()
        self.assertEqual((self.root / "data/CLOUDHV.fd").read_bytes(), b"test firmware")

    def test_empty_firmware_and_unsafe_paths_fail(self):
        self.source.write_bytes(b"")
        with self.assertRaisesRegex(setup.SetupError, "empty"):
            self.apply()
        for path in ("relative", "/", str(self.root / "../escape")):
            with self.assertRaises(setup.SetupError):
                setup.safe_path(path)

    def test_zfs_commands_use_repo_pool_not_arbitrary_device(self):
        self.settings["repos"] = [{"name": "fast", "driver": "zfs", "path": None, "pool": "tank/chadm"}]
        self.settings["firmware"] = None
        calls = []
        def fake_run(args):
            calls.append(args)
            return "[]"
        with patch.object(setup, "run", side_effect=fake_run):
            self.apply()
        self.assertEqual(calls[-1][-8:], ["repo", "create", "--name", "fast", "--driver", "zfs", "--pool", "tank/chadm"])
        self.assertNotIn("destroy", sum(calls, []))


if __name__ == "__main__":
    unittest.main(verbosity=2)
