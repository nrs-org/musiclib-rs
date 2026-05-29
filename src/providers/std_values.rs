// provider keys
pub struct StandardProviderKeys;

impl StandardProviderKeys {
    pub const YOUTUBE: &'static str = "youtube";
    /// Source key used for unresolved URLs that have not yet been canonicalized.
    pub const UNKNOWN_URL: &'static str = "unknown_url";
}

pub struct StandardRoleNames;

impl StandardRoleNames {
    pub const UPLOADER: &'static str = "uploader";
    pub const LISTED_ARTIST: &'static str = "listed_artist";
}
