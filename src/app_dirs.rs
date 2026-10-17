//! Per-user config, cache, and data directories.
//!
//! Each directory resolves to, in order: the binary's `--config-dir`,
//! `--cache-dir`, or `--data-dir` flag (installed by [`DirArgs::apply`]); the
//! `MUSICLIB_CONFIG_DIR`, `MUSICLIB_CACHE_DIR`, or `MUSICLIB_DATA_DIR`
//! environment variable; then the platform default from [`ProjectDirs`].
//! Relative overrides are made absolute against the working directory, so a
//! later directory change does not move them.

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    sync::OnceLock,
};

use clap::Args;
use directories::ProjectDirs;

/// Directory override flags shared by the binaries. Flatten into a binary's
/// arguments and call [`DirArgs::apply`] before anything reads a directory.
#[derive(Args, Debug, Clone, Default)]
pub struct DirArgs {
    /// Config directory. Overrides $MUSICLIB_CONFIG_DIR and the platform
    /// default (~/.config/musiclib-rs on Linux).
    #[arg(long, value_name = "DIR")]
    pub config_dir: Option<PathBuf>,
    /// Cache directory. Overrides $MUSICLIB_CACHE_DIR and the platform
    /// default (~/.cache/musiclib-rs on Linux).
    #[arg(long, value_name = "DIR")]
    pub cache_dir: Option<PathBuf>,
    /// Data directory. Overrides $MUSICLIB_DATA_DIR and the platform default
    /// (~/.local/share/musiclib-rs on Linux).
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
}

static OVERRIDES: OnceLock<DirArgs> = OnceLock::new();

impl DirArgs {
    /// Installs these flags for the rest of the process. Only the first call
    /// takes effect.
    pub fn apply(&self) {
        let _ = OVERRIDES.set(self.clone());
    }
}

fn project_dirs() -> ProjectDirs {
    ProjectDirs::from("", "nrs-org", "musiclib-rs").expect("could not determine home directory")
}

/// The first of `flag` and `env` that is set and non-empty, made absolute;
/// otherwise `default()`.
fn resolve(
    flag: Option<&Path>,
    env: Option<OsString>,
    default: impl FnOnce() -> PathBuf,
) -> PathBuf {
    let explicit = flag
        .map(Path::to_owned)
        .or_else(|| env.filter(|v| !v.is_empty()).map(PathBuf::from));
    match explicit {
        Some(path) => std::path::absolute(&path).unwrap_or(path),
        None => default(),
    }
}

pub fn config_dir() -> PathBuf {
    resolve(
        OVERRIDES.get().and_then(|o| o.config_dir.as_deref()),
        std::env::var_os("MUSICLIB_CONFIG_DIR"),
        || project_dirs().config_dir().to_owned(),
    )
}

pub fn cache_dir() -> PathBuf {
    resolve(
        OVERRIDES.get().and_then(|o| o.cache_dir.as_deref()),
        std::env::var_os("MUSICLIB_CACHE_DIR"),
        || project_dirs().cache_dir().to_owned(),
    )
}

pub fn data_dir() -> PathBuf {
    resolve(
        OVERRIDES.get().and_then(|o| o.data_dir.as_deref()),
        std::env::var_os("MUSICLIB_DATA_DIR"),
        || project_dirs().data_local_dir().to_owned(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default() -> PathBuf {
        PathBuf::from("/default")
    }

    #[test]
    fn flag_wins_over_env() {
        let got = resolve(Some(Path::new("/flag")), Some("/env".into()), default);
        assert_eq!(got, PathBuf::from("/flag"));
    }

    #[test]
    fn env_wins_over_default() {
        assert_eq!(
            resolve(None, Some("/env".into()), default),
            PathBuf::from("/env")
        );
    }

    #[test]
    fn empty_env_falls_back_to_default() {
        assert_eq!(resolve(None, Some("".into()), default), default());
        assert_eq!(resolve(None, None, default), default());
    }

    #[test]
    fn relative_override_is_made_absolute() {
        let got = resolve(Some(Path::new("state/config")), None, default);
        assert_eq!(got, std::env::current_dir().unwrap().join("state/config"));
    }
}
