{
  lib,
  craneLib,
  cacert,
  pkg-config,
  libffi,
  openssl,
}:

let
  commonArgs = {
    src = lib.fileset.toSource {
      root = ../.;
      fileset = lib.fileset.unions [
        ../Cargo.toml
        ../Cargo.lock
        ../src
        # Tests read the example match script.
        ../config
      ];
    };
    strictDeps = true;
    # match.rhai loads its matcher library through the ffi module.
    cargoExtraArgs = "--locked --features ffi-system";
    nativeBuildInputs = [ pkg-config ];
    buildInputs = [
      libffi
      openssl
    ];
  };

  # Dependencies only change with Cargo.lock, so source edits reuse this build.
  cargoArtifacts = craneLib.buildDepsOnly commonArgs;
in
craneLib.buildPackage (
  commonArgs
  // {
    inherit cargoArtifacts;

    # reqwest loads the system CA certificates when it builds a client, and
    # the sandbox has none. Tests only contact local mock servers.
    preCheck = ''
      export SSL_CERT_FILE=${cacert}/etc/ssl/certs/ca-bundle.crt
    '';

    # Binary names such as `server` and `import` are generic on a shared PATH.
    # update_fixtures records test fixtures and is only useful in a checkout.
    postInstall = ''
      rm $out/bin/update_fixtures
      for bin in $out/bin/*; do
        mv "$bin" "$out/bin/musiclib-$(basename "$bin")"
      done
    '';

    meta = {
      description = "Music library and NRS music store";
      mainProgram = "musiclib-server";
    };
  }
)
