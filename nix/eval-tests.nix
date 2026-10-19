{ pkgs ? import <nixpkgs> { } }:
let
  lib = pkgs.lib;
  evaluateWith = hostModules: settings: (import (pkgs.path + "/nixos/lib/eval-config.nix") {
    inherit pkgs;
    system = pkgs.stdenv.hostPlatform.system;
    modules = hostModules ++ [ ./module.nix {
      boot.isContainer = true;
      system.stateVersion = "26.05";
      virtualisation.chadm = { enable = true; } // settings;
    } ];
  }).config;
  valid = cfg: lib.all (item: item.assertion) cfg.assertions;
  evaluate = evaluateWith [ ];
  base = evaluate { };
  existingBridge = evaluateWith [ { networking.bridges.br0.interfaces = [ "eno1" ]; } ] { };
  custom = evaluate {
    defaultBridge = "br0";
    dataDir = "/srv/chadm";
    runtimeDir = "/run/chadm-custom";
    repos = {
      local.path = "/srv/chadm-disks";
      cloudinit.path = "/srv/chadm-seeds";
      fast = { driver = "zfs"; pool = "tank/chadm"; };
    };
  };
  firmware = evaluate {
    firmware = {
      enable = true;
      github.rev = "test-release";
      github.hash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
    };
  };
  source = evaluate { firmware = { enable = true; source = ./firmware-test.fd; }; };
  disabled = evaluate { enable = false; };
in
assert valid base;
assert base.virtualisation.chadm.defaultBridge == null;
assert custom.virtualisation.chadm.defaultBridge == "br0";
assert base.virtualisation.chadm.enableZfs == false;
assert base.systemd.services.chadm-host-setup.serviceConfig.Type == "oneshot";
assert !(builtins.hasAttr "chadm-instance@" base.systemd.services);
assert !(builtins.hasAttr "chadm-host-setup" disabled.systemd.services);
assert valid custom;
assert custom.virtualisation.chadm.enableZfs;
assert base.networking.bridges == { };
assert custom.networking.bridges == { };
assert builtins.attrNames existingBridge.networking.bridges == [ "br0" ];
assert existingBridge.networking.bridges.br0.interfaces == [ "eno1" ];
assert lib.elem "d /srv/chadm 0700 root root -" custom.systemd.tmpfiles.rules;
assert lib.hasInfix "zfs" custom.virtualisation.chadm.package.CHADM_TOOL_PATH;
assert valid firmware;
assert firmware.virtualisation.chadm.firmware.github.asset == (
  if pkgs.stdenv.hostPlatform.isAarch64 then "CLOUDHV_EFI.fd" else "CLOUDHV.fd");
assert valid source;
assert !(valid (evaluate { firmware.enable = true; }));
assert !(valid (evaluate { firmware = { enable = true; github.rev = "latest"; }; }));
assert !(valid (evaluate { repos.fast = { driver = "zfs"; pool = "tank/chadm"; }; enableZfs = false; }));
assert !(valid (evaluate { repos.local = { path = "/srv/disks"; pool = "tank/chadm"; }; }));
assert !(valid (evaluate { repos.cloudinit = { driver = "zfs"; pool = "tank/chadm"; }; }));
assert !(valid (evaluate { repos."../escape".path = "/srv/disks"; }));
assert !(valid (evaluate { dataDir = "/run/chadm/data"; }));
assert !(valid (evaluate { runtimeDir = "/tmp/chadm"; }));
assert !(valid (evaluate { dataDir = "/srv/../chadm"; }));
true
