{
  description = "raven - A Wayland compositor in Rust.";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };
  outputs = {
    self,
    nixpkgs,
  }: let
    systems = ["x86_64-linux" "aarch64-linux"];

    forAllSystems = fn: nixpkgs.lib.genAttrs systems (system: fn nixpkgs.legacyPackages.${system});
  in {
    packages = forAllSystems (pkgs: rec {
      default = pkgs.callPackage ./default.nix {
        gitRev = self.rev or self.dirtyRev or null;
      };
      raven = default;
    });

    devShells = forAllSystems (pkgs: {
      default = pkgs.mkShell {
        inputsFrom = [self.packages.${pkgs.stdenv.hostPlatform.system}.raven];
        packages = [
          pkgs.rustc
          pkgs.cargo
          pkgs.clippy
          pkgs.rustfmt
          pkgs.foot
          pkgs.westonLite # weston-terminal
          pkgs.lua
          pkgs.just
          pkgs.pkg-config
        ];
        shellHook = ''
          export PS1="(raven-dev) $PS1"
        '';

        env = {
          RUST_SRC_PATH = pkgs.rustPlatform.rustLibSrc;
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
            pkgs.wayland
            pkgs.libGL
            pkgs.libglvnd
            pkgs.libinput
            pkgs.seatd
            pkgs.systemdMinimal
            pkgs.libgbm
            pkgs.mesa
          ];
        };
      };
    });

    formatter = forAllSystems (pkgs: pkgs.alejandra);
  };
}
