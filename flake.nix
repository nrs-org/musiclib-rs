{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default";
    git-hooks.url = "github:cachix/git-hooks.nix";
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
        in
        {
          default = pkgs.mkShell {
            inherit shellHook;
            nativeBuildInputs = with pkgs; [
              pkg-config
              openssl
              # autotools: only needed to build `--features ffi` (the generic FFI
              # module), whose `libffi` crate vendors libffi and builds it from
              # source via autoreconf. Harmless for default builds.
              autoconf
              automake
              libtool
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
              ]);
          };
        }
      );
    };
}
