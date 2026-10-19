{ lib, stdenv, rustPlatform, systemd, iproute2, cloud-hypervisor, qemu-utils, coreutils, python3
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
  outputs = [ "out" "tests" ];
  # Nix build sandboxes reject ownership xattrs. Run the pure tests here;
  # preserve ALL test harnesses for checks.deployment on a real VM filesystem.
  cargoTestFlags = [ "--lib" "--test=cli" "--test=repo" "--test=lifecycle" "--test=zfs" ];
  checkFlags = [
    "--skip=checkpoint::tests::" "--skip=hotplug::tests::"
    "--skip=instance::tests::" "--skip=lifecycle::tests::"
    "--skip=legacy_file_metadata_omits_native_identity_and_rejects_injected_zfs_fields"
  ];
  nativeBuildInputs = [ python3 ];
  postCheck = ''
    cargo test --offline --release --target ${stdenv.hostPlatform.rust.rustcTarget} --no-run --message-format=json > test-artifacts.json
    mkdir -p "$tests/bin"
    python3 - <<'PYCODE'
import json, os, shutil
for line in open("test-artifacts.json"):
    artifact = json.loads(line)
    if artifact.get("reason") == "compiler-artifact" and artifact.get("profile", {}).get("test") and artifact.get("executable"):
        source = artifact["executable"]
        shutil.copy2(source, os.path.join(os.environ["tests"], "bin", os.path.basename(source)))
PYCODE
  '';
  cargoLock.lockFile = ../Cargo.lock;
  CHADM_TOOL_PATH = lib.makeBinPath (
    [ systemd iproute2 cloud-hypervisor qemu-utils coreutils ]
    ++ lib.optional withZfs zfs
  );
  postPatch = ''
    substituteInPlace packaging/systemd/chadm-instance@.service \
      --replace-fail /usr/local/bin/chadm "$out/bin/chadm"
    substituteInPlace tests/*.rs \
      --replace 'env!("CARGO_BIN_EXE_chadm")' 'std::env::var("CHADM_TEST_BINARY").unwrap_or_else(|_| env!("CARGO_BIN_EXE_chadm").to_string())'
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
