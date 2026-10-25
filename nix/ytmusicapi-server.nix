{ runCommand, python3 }:

let
  python = python3.withPackages (ps: [ ps.ytmusicapi ]);
in
runCommand "ytmusicapi-server"
  {
    meta = {
      description = "YouTube Music metadata helper for musiclib";
      mainProgram = "ytmusicapi-server";
    };
  }
  ''
    install -Dm755 ${../src/bin/ytmusicapi_server.py} $out/bin/ytmusicapi-server
    substituteInPlace $out/bin/ytmusicapi-server \
      --replace-fail '#!/usr/bin/env -S uv run' '#!${python.interpreter} -u'
  ''
