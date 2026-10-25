{
  inputs = {
    # Same revision as ../nrs-next's flake, so the NRS dev stack and this
    # checkout use one Rust toolchain. Update both pins together.
    nixpkgs.url = "github:NixOS/nixpkgs/d407951447dcd00442e97087bf374aad70c04cea";
    systems.url = "github:nix-systems/default";
    git-hooks.url = "github:cachix/git-hooks.nix";
    # Its rustfmt hook puts cargo on the shell's PATH; keep that cargo on the
    # same nixpkgs as rustc.
    git-hooks.inputs.nixpkgs.follows = "nixpkgs";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      systems,
      git-hooks,
      crane,
      ...
    }:
    let
      forEachSystem = nixpkgs.lib.genAttrs (import systems);
    in
    {
      packages = forEachSystem (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
        in
        rec {
          musiclib = pkgs.callPackage ./nix/musiclib.nix { craneLib = crane.mkLib pkgs; };
          ytdlp-server = pkgs.callPackage ./nix/ytdlp-server.nix { inherit musiclib; };
          ytmusicapi-server = pkgs.callPackage ./nix/ytmusicapi-server.nix { };
          default = musiclib;
        }
      );

      checks = forEachSystem (system: {
        pre-commit-check = git-hooks.lib.${system}.run {
          src = ./.;
          hooks = {
            nixfmt.enable = true;
            statix.enable = true;
            end-of-file-fixer.enable = true;
            trim-trailing-whitespace.enable = true;
            rustfmt.enable = true;
          };
        };
      });

      devShells = forEachSystem (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
          };
          inherit (self.checks.${system}.pre-commit-check) shellHook enabledPackages;
          # The development environment without the git-hooks shell hook. The
          # NRS dev stack builds with it, so restarts do not reinstall hooks.
          build = pkgs.mkShell (
            {
              nativeBuildInputs = with pkgs; [
                pkg-config
                openssl
                # autotools: only needed to build `--features ffi` (the generic FFI
                # module), whose `libffi` crate vendors libffi and builds it from
                # source via autoreconf. Harmless for default builds.
                autoconf
                automake
                libtool
                # `--features ffi-system` (used by the NRS dev stack) links this
                # prebuilt libffi instead.
                libffi
              ];
              packages =
                enabledPackages
                ++ (with pkgs; [
                  sqlite
                  rustc
                  cargo
                  clippy
                  nixfmt
                  uv
                ])
                ++ pkgs.lib.optional pkgs.stdenv.isLinux pkgs.mold;
            }
            # Link with mold; GNU ld spent most of an incremental rebuild
            # linking debug info. ../nrs-next's dev shell sets the same flags.
            // pkgs.lib.optionalAttrs pkgs.stdenv.isLinux {
              "CARGO_TARGET_${pkgs.stdenv.hostPlatform.rust.cargoEnvVarTarget}_RUSTFLAGS" =
                "-C link-arg=-fuse-ld=mold";
            }
          );
        in
        {
          inherit build;
          default = build.overrideAttrs { inherit shellHook; };
        }
      );
    };
}
