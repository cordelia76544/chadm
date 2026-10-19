{ lib, rustPlatform, systemd, iproute2, cloud-hypervisor, qemu-utils, coreutils
, zfs ? null, withZfs ? false }:

rustPlatform.buildRustPackage {
  pname = "chadm";
  version = "0.1.0";
  # Explicit source allowlist: never copy local firmware, guest images or target/.
  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [
      ../Cargo.toml ../Cargo.lock ../src ../tests ../packaging
    ];
  };
  cargoLock.lockFile = ../Cargo.lock;
  CHADM_TOOL_PATH = lib.makeBinPath (
    [ systemd iproute2 cloud-hypervisor qemu-utils coreutils ]
    ++ lib.optional withZfs zfs
  );
  postPatch = ''
    substituteInPlace packaging/systemd/chadm-instance@.service \
      --replace-fail /usr/local/bin/chadm "$out/bin/chadm"
    # Unit tests use small coreutils subprocesses, not FHS paths in the sandbox.
    substituteInPlace src/command.rs \
      --replace-fail /bin/echo ${coreutils}/bin/echo \
      --replace-fail /bin/false ${coreutils}/bin/false \
      --replace-fail /bin/sleep ${coreutils}/bin/sleep \
      --replace-fail /bin/ls ${coreutils}/bin/ls
    substituteInPlace src/lifecycle.rs \
      --replace-fail /bin/true ${coreutils}/bin/true
  '';
  preBuild = ''
    export CHADM_HELPER="$out/bin/chadm"
  '';
  postInstall = ''
    install -Dm444 packaging/systemd/chadm-instance@.service \
      "$out/lib/systemd/system/chadm-instance@.service"
  '';
  meta = {
    description = "Linux-native Cloud Hypervisor manager using systemd and local TOML";
    license = lib.licenses.asl20;
    platforms = [ "x86_64-linux" "aarch64-linux" ];
    mainProgram = "chadm";
  };
}
