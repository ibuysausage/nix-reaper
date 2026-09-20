{
  description = "nix-reaper: deep-clean a NixOS system (generations, boot entries, GC roots, non-Nix bloat)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "nix-reaper";
          version = "0.1.0";
          src = ./.;

          # Requires Cargo.lock to exist. If you just cloned this and there's
          # no Cargo.lock yet, run `nix develop -c cargo generate-lockfile` once.
          cargoLock.lockFile = ./Cargo.lock;

          # nix-reaper shells out to nix/journalctl/du/df at runtime; wrap it so
          # those are found even from a minimal PATH (cron, a systemd unit, etc).
          nativeBuildInputs = [ pkgs.makeWrapper ];
          postInstall = ''
            wrapProgram $out/bin/nix-reaper \
              --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.nix pkgs.systemd pkgs.coreutils ]}
          '';

          meta = with pkgs.lib; {
            description = "Deep-clean a NixOS system: generations, boot entries, GC roots, and non-Nix bloat";
            license = licenses.mit;
            mainProgram = "nix-reaper";
          };
        };

        apps.default = flake-utils.lib.mkApp {
          drv = self.packages.${system}.default;
        };

        devShells.default = pkgs.mkShell {
          packages = [ pkgs.cargo pkgs.rustc pkgs.rust-analyzer pkgs.clippy ];
        };
      });
}
