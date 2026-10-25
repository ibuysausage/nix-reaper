{
  description = "nix-reaper - Nix/Docker disk cleanup tool";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    forAllSystems = nixpkgs.lib.genAttrs ["x86_64-linux" "aarch64-linux"];
  in {
    packages = forAllSystems (system: let
      pkgs = nixpkgs.legacyPackages.${system};
    in rec {
      nix-reaper = pkgs.stdenv.mkDerivation {
        pname = "nix-reaper";
        version = "0.1.0";
        src = self;

        buildPhase = "$CC -O2 -Wall -o nix-reaper nix-reaper.c";
        installPhase = "install -Dm755 nix-reaper $out/bin/nix-reaper";

        meta.mainProgram = "nix-reaper";
      };
      default = nix-reaper;
    });
  };
}
