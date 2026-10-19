{ pkgs ? import <nixpkgs> { } }:
let package = pkgs.callPackage ./package.nix { }; in
pkgs.testers.runNixOSTest {
  name = "chadm-deployment";
  nodes.machine = { ... }: {
    imports = [ ./module.nix ];
    environment.systemPackages = [ pkgs.python3 ];
    virtualisation.memorySize = 2048;
    # The host supplies the bridge independently of chadm.
    networking.bridges.chbr0.interfaces = [ ];
    virtualisation.chadm = {
      enable = true;
      defaultBridge = "chbr0";
      repos.local.path = "/srv/chadm-storage";
      repos.cloudinit.path = "/srv/chadm-seeds";
      firmware = { enable = true; source = ./firmware-test.fd; };
    };
  };
  testScript = ''
    import json
    start_all()
    machine.wait_for_unit("multi-user.target")
    machine.wait_for_unit("chadm-host-setup.service")
    machine.succeed("chadm version")
    # Run every Rust harness on the guest filesystem, where user xattrs work.
    # Keep access to root-owned service config, but make DAC failure injection
    # meaningful: root must not bypass chmod(0500) in the filesystem tests.
    machine.succeed("install -d -m 700 /tmp/chadm-tests")
    machine.succeed("${pkgs.libcap}/bin/capsh --drop=cap_dac_override,cap_dac_read_search -- -c 'for test in ${package.tests}/bin/*; do CHADM_TEST_BINARY=${package}/bin/chadm TMPDIR=/tmp/chadm-tests \"$test\" || exit 1; done'", timeout=300)
    machine.succeed("CHADM_TEST_BINARY=${package}/bin/chadm PYTHONDONTWRITEBYTECODE=1 python3 ${./.}/test_host_setup.py", timeout=300)
    machine.succeed("test -f /etc/chadm/config.toml && test ! -L /etc/chadm/config.toml")
    fragment = machine.succeed("systemctl show chadm-instance@probe.service -p FragmentPath --value").strip()
    assert fragment.endswith("chadm-instance@.service"), fragment
    machine.succeed("grep 'ExecStart=/nix/store/' " + fragment)
    machine.succeed("test -z \"$(systemctl show chadm-instance@probe.service -p DropInPaths --value)\"")
    # KVM/firmware may be absent in this nested VM; test the deployment checks.
    _, output = machine.execute("chadm doctor --json")
    report = json.loads(output)
    assert any(c["name"] == "service template" and c["status"] == "OK" for c in report["checks"]), report
    for name in ["cloud-hypervisor", "systemd", "iproute2", "qemu-img"]:
        assert any(c["name"] == name and c["status"] == "OK" for c in report["checks"]), report
    machine.succeed("ip link show chbr0")
    repo_before = json.loads(machine.succeed("chadm repo show local --json"))
    assert repo_before["path"] == "/srv/chadm-storage"
    assert json.loads(machine.succeed("chadm repo show cloudinit --json"))["kind"] == "cloudinit"
    firmware_inode = machine.succeed("stat -c %i /var/lib/chadm/CLOUDHV.fd")
    machine.succeed("truncate -s 1M /tmp/base.raw")
    machine.succeed("chadm image create --name base --repo local --path /tmp/base.raw")
    machine.succeed("chadm instance create --name test01 --image base --boot firmware --volume root,size=2M --no-cloud-init")
    assert json.loads(machine.succeed("chadm instance show test01 --json"))["network"][0]["bridge"] == "chbr0"
    machine.succeed("ip link delete chbr0")
    failure = machine.fail("chadm instance create --name missing --image base --boot firmware --volume root,size=2M --no-cloud-init 2>&1")
    assert 'does not exist' in failure, failure
    machine.succeed("test ! -e /var/lib/chadm/instances/missing && test ! -e /var/lib/chadm/volumes/missing-root")
    machine.succeed("ip link add chbr0 type bridge && ip link set chbr0 up")
    vm_before = machine.succeed("cat /var/lib/chadm/instances/test01/config.toml")
    machine.succeed("systemctl restart chadm-host-setup.service")
    assert json.loads(machine.succeed("chadm repo show local --json"))["id"] == repo_before["id"]
    assert machine.succeed("stat -c %i /var/lib/chadm/CLOUDHV.fd") == firmware_inode
    assert machine.succeed("cat /var/lib/chadm/instances/test01/config.toml") == vm_before
    machine.succeed("chadm volume create --name test --repo local --size 16M")
    machine.succeed("chadm volume show test --json")
    machine.succeed("chadm volume delete test")
    machine.succeed("chadm instance delete test01")
    machine.succeed("chadm volume delete test01-root")
    machine.succeed("chadm image delete base")
    machine.succeed("chadm repo delete local")
  '';
}
