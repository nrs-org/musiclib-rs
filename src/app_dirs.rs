use std::path::PathBuf;

use directories::ProjectDirs;

fn project_dirs() -> ProjectDirs {
    ProjectDirs::from("", "nrs-org", "musiclib-rs").expect("could not determine home directory")
}

pub fn config_dir() -> PathBuf {
    project_dirs().config_dir().to_owned()
}

pub fn cache_dir() -> PathBuf {
    project_dirs().cache_dir().to_owned()
}

pub fn data_dir() -> PathBuf {
    project_dirs().data_local_dir().to_owned()
}
