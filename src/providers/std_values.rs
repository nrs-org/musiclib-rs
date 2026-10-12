// provider keys
pub struct StandardProviderKeys;

impl StandardProviderKeys {
    pub const YOUTUBE: &'static str = "youtube";
    /// Source key used for unresolved URLs that have not yet been canonicalized.
    pub const UNKNOWN_URL: &'static str = "unknown_url";
    /// Source key for ISRC codes; normalise identifiers with [`normalize_isrc`].
    pub const ISRC: &'static str = "isrc";
}

/// The canonical form of an ISRC: upper case, without the hyphens or spaces
/// of the display form (`us-hm2-19-27082` -> `USHM21927082`). Providers don't
/// agree on case (Spotify sometimes returns lower case), and dedup matches
/// identifiers exactly.
pub fn normalize_isrc(isrc: &str) -> String {
    isrc.chars()
        .filter(|c| !c.is_whitespace() && *c != '-')
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

pub struct StandardRoleNames;

impl StandardRoleNames {
    pub const UPLOADER: &'static str = "uploader";
    pub const LISTED_ARTIST: &'static str = "listed_artist";
}

#[cfg(test)]
mod tests {
    use super::normalize_isrc;

    #[test]
    fn normalize_isrc_upper_cases_and_strips_separators() {
        assert_eq!(normalize_isrc("ushm21927082"), "USHM21927082");
        assert_eq!(normalize_isrc("US-HM2-19-27082"), "USHM21927082");
        assert_eq!(normalize_isrc(" JPV752307972 "), "JPV752307972");
    }
}
