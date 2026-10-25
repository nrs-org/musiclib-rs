/// A Discogs response's picture: its `images` entry of type `primary`, else
/// its first, at the size `Image::pick` prefers between `uri` and the
/// 150 px `uri150`. Release, master and artist responses all keep `images`
/// in their flattened `extra`.
pub(crate) fn primary_image(extra: &serde_json::Value) -> Option<crate::providers::types::Image> {
    use crate::providers::types::Image;
    let images = extra.get("images")?.as_array()?;
    let image = images
        .iter()
        .find(|i| i.get("type").and_then(|t| t.as_str()) == Some("primary"))
        .or_else(|| images.first())?;
    let size = |k: &str| {
        image
            .get(k)
            .and_then(serde_json::Value::as_u64)
            .map(|n| n as u32)
    };
    let url = |k: &str| {
        image
            .get(k)
            .and_then(|u| u.as_str())
            .filter(|u| !u.is_empty())
    };
    Image::pick(
        [
            url("uri").map(|u| Image::new(u, size("width"), size("height"))),
            url("uri150").map(|u| Image::new(u, Some(150), Some(150))),
        ]
        .into_iter()
        .flatten(),
    )
}

pub const EXTERNAL_TYPE_RELEASE: &str = "discogs:release";
pub const EXTERNAL_TYPE_MASTER: &str = "discogs:master";
pub const EXTERNAL_TYPE_ARTIST: &str = "discogs:artist";
pub const EXTERNAL_TYPE_TRACK: &str = "discogs:track";

#[cfg(test)]
mod image_tests {
    #[test]
    fn prefers_the_primary_image_at_full_size() {
        let extra = serde_json::json!({ "images": [
            { "type": "secondary", "uri": "s", "uri150": "s150", "width": 600, "height": 600 },
            { "type": "primary", "uri": "p", "uri150": "p150", "width": 547, "height": 600 },
        ]});
        assert_eq!(super::primary_image(&extra).unwrap().url, "p");
        assert!(super::primary_image(&serde_json::json!({})).is_none());
    }
}
