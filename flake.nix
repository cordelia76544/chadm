{
  description = "chadm: Cloud Hypervisor host management with local VM configuration";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
    in {
      nixosModules.default = import ./nix/module.nix;
      nixosModules.chadm = self.nixosModules.default;
      packages = forAllSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          default = pkgs.callPackage ./nix/package.nix { };
          chadm = self.packages.${system}.default;
        });
      checks = forAllSystems (system:
        let pkgs = import nixpkgs { inherit system; };
        in {
          package = self.packages.${system}.default;
          module = assert import ./nix/eval-tests.nix { inherit pkgs; };
            pkgs.runCommand "chadm-module-evaluation" { } "touch $out";
          # Host setup touches raw ownership xattrs: exercise it in the VM too.
          host-setup = self.checks.${system}.deployment;
          deployment = import ./nix/tests.nix { inherit pkgs; };
        });
    };
}
