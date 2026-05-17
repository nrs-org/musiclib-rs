{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    systems.url = "github:nix-systems/default";
    git-hooks.url = "github:cachix/git-hooks.nix";
  };

  outputs =
    {
      self,
      nixpkgs,
      systems,
      git-hooks,
      ...
    }:
    let
      forEachSystem = nixpkgs.lib.genAttrs (import systems);
    in
    {
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
