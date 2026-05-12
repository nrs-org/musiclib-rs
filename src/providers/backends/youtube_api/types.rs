pub enum StringFilterMode {
    Include,
    Exclude,
}

pub struct StringFilter {
    pattern: String,
    priority: i32,
    mode: StringFilterMode,
}

pub struct StringFilterSet {
    filters: Vec<StringFilter>,
}

pub struct DurationRange {
    min: Option<u64>,
    max: Option<u64>,
}

pub struct VideoDiscographyFetchOptions {
    // true -> only fetch videos with category 10 (Music)
    // (this is not really reliable)
    filter_music_category: bool,

    title_filters: StringFilterSet,
    description_filters: StringFilterSet,
    duration_range: Option<DurationRange>,
}

pub struct PlaylistDiscographyFetchOptions {
    title_filters: StringFilterSet,
    description_filters: StringFilterSet,
}

pub struct YtMusicDiscographyFetchOptions {}

pub struct DiscographyFetchOptions {
    videos: Option<VideoDiscographyFetchOptions>,
    playlists: Option<PlaylistDiscographyFetchOptions>,
    ytmusic: Option<YtMusicDiscographyFetchOptions>,
}

#[derive(Default)]
pub struct EntryFetchOptions {
    discography: Option<DiscographyFetchOptions>,
}

pub struct YoutubeProvider {}
