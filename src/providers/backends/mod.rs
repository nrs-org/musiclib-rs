pub mod youtube_api;

pub fn build_url(url: String, params: &[(&str, &str)]) -> String {
    let mut url = url;
    let mut first = true;
    for (k, v) in params {
        if first {
            url.push('?');
            first = false;
        } else {
            url.push('&');
        }
        url.push_str(&format!("{}={}", k, urlencoding::encode(v)));
    }
    url
}

pub type ExtraJSON = serde_json::Map<String, serde_json::Value>;
