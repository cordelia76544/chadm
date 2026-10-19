{ pkgs ? import <nixpkgs> { } }:
pkgs.testers.runNixOSTest {
  name = "chadm-deployment";
  nodes.machine = { ... }: {
    imports = [ ./module.nix ];
    # The host supplies the bridge independently of chadm.
    networking.bridges.chbr0.interfaces = [ ];
    virtualisation.chadm = {
      enable = true;
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
    machine.succeed("chadm instance create --name test01 --image base --boot firmware --volume root,size=2M --net bridge=chbr0 --no-cloud-init")
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
