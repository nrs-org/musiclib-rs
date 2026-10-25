{
  lib,
  runCommand,
  makeWrapper,
  musiclib,
  yt-dlp,
}:

# YTDLP_PATH is a default only: yt-dlp needs updates more often than this
# flake's lock, so a newer yt-dlp can be selected without a rebuild.
runCommand "ytdlp-server-${musiclib.version}"
  {
    nativeBuildInputs = [ makeWrapper ];
    meta = {
      description = "yt-dlp metadata helper for musiclib";
      mainProgram = "ytdlp-server";
    };
  }
  ''
    makeWrapper ${musiclib}/bin/musiclib-ytdlp_server $out/bin/ytdlp-server \
      --set-default YTDLP_PATH ${lib.getExe yt-dlp}
  ''
