//! Source-specific request headers for media fetched by the local DLNA proxy.
pub fn referer(url: &str) -> &'static str {
    match url::Url::parse(url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
    {
        Some(host) if host == "googlevideo.com" || host.ends_with(".googlevideo.com") => {
            "https://www.youtube.com/"
        }
        _ => "https://www.bilibili.com/",
    }
}
