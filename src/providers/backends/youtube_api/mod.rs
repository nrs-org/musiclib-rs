mod canonicalize;
mod channel;
mod client;
mod playlist;
mod types;
mod video;

pub use types::EntryFetchOptions;

use crate::providers::std_values::StandardProviderKeys;

pub const SOURCE: &'static str = StandardProviderKeys::YOUTUBE;
