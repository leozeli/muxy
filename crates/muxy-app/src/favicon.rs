use std::io::{Cursor, Read};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use muxy_core::worker::WorkerPool;

const MAX_BYTES: u64 = 1024 * 1024;
const SIZE: u32 = 64;
const MAX_SIDE: u32 = 1024;
const MAX_DECODED_BYTES: u64 = 16 * 1024 * 1024;
const MAX_REDIRECTS: usize = 3;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

impl Origin {
    fn from_url(url: &reqwest::Url) -> Option<Self> {
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return None;
        }
        let host = url.host_str()?.trim_end_matches('.').to_ascii_lowercase();
        if host.is_empty() {
            return None;
        }
        Some(Self {
            scheme: url.scheme().to_owned(),
            host,
            port: url.port_or_known_default()?,
        })
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CacheKey {
    page_origin: Option<Origin>,
    url: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Target {
    Public(reqwest::Url),
    SameOrigin { url: reqwest::Url, origin: Origin },
}

pub(crate) fn cache_key(url: &str, page_url: Option<&str>) -> Option<CacheKey> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    Origin::from_url(&parsed)?;
    let page_origin = page_url
        .and_then(|page| reqwest::Url::parse(page.trim()).ok())
        .and_then(|page| Origin::from_url(&page));
    Some(CacheKey {
        page_origin,
        url: parsed.into(),
    })
}

fn target(key: &CacheKey) -> Option<Target> {
    let url = reqwest::Url::parse(&key.url).ok()?;
    let origin = Origin::from_url(&url)?;
    if !crate::extensions::http::blocked(&origin.host) {
        return Some(Target::Public(url));
    }
    (key.page_origin.as_ref() == Some(&origin)).then_some(Target::SameOrigin { url, origin })
}

fn follows(target: &Target, next: &reqwest::Url) -> bool {
    let Some(next_origin) = Origin::from_url(next) else {
        return false;
    };
    match target {
        Target::Public(_) => !crate::extensions::http::blocked(&next_origin.host),
        Target::SameOrigin { origin, .. } => next_origin == *origin,
    }
}

pub(crate) fn submit(key: CacheKey) -> Option<async_channel::Receiver<Option<gpui::RenderImage>>> {
    static WORKERS: OnceLock<Option<WorkerPool>> = OnceLock::new();
    let pool = WORKERS
        .get_or_init(|| WorkerPool::new("favicons", 2, 16).ok())
        .as_ref()?;
    let (send, receive) = async_channel::bounded(1);
    pool.try_spawn(move || {
        if !send.is_closed() {
            let _ = send.try_send(fetch(&key));
        }
    })
    .ok()?;
    Some(receive)
}

fn fetch(key: &CacheKey) -> Option<gpui::RenderImage> {
    let target = target(key)?;
    let policy_target = target.clone();
    let mut builder = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .redirect(reqwest::redirect::Policy::custom(move |attempt| {
            if attempt.previous().len() >= MAX_REDIRECTS {
                attempt.stop()
            } else if follows(&policy_target, attempt.url()) {
                attempt.follow()
            } else {
                attempt.stop()
            }
        }));
    if matches!(target, Target::Public(_)) {
        builder = builder.dns_resolver(Arc::new(crate::extensions::http::PublicOnly));
    }
    let client = crate::proxy::configure(builder).ok()?.build().ok()?;
    let url = match target {
        Target::Public(url) | Target::SameOrigin { url, .. } => url,
    };
    let response = client.get(url).send().ok()?.error_for_status().ok()?;
    let mut bytes = Vec::new();
    response.take(MAX_BYTES).read_to_end(&mut bytes).ok()?;
    decode(&bytes)
}

fn decode(bytes: &[u8]) -> Option<gpui::RenderImage> {
    let mut reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    reader.limits(limits);
    let mut bgra = reader.decode().ok()?.thumbnail(SIZE, SIZE).into_rgba8();
    for pixel in bgra.pixels_mut() {
        pixel.0.swap(0, 2);
    }
    Some(gpui::RenderImage::new([image::Frame::new(bgra)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        image::RgbaImage::new(width, height)
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();
        bytes
    }

    #[test]
    fn icons_decode_only_up_to_a_small_size() {
        assert!(decode(&png(32, 32)).is_some());
        assert!(decode(&png(MAX_SIDE, 16)).is_some());
        let huge = png(MAX_SIDE + 1, 1);
        assert!(decode(&huge).is_none());
        let tall = png(1, 20_000);
        assert!(tall.len() < 64 * 1024, "{}", tall.len());
        assert!(decode(&tall).is_none());
        assert!(decode(b"not an image").is_none());
    }

    #[test]
    fn favicon_targets_block_private_hosts_unless_same_origin_and_limit_redirects() {
        for rejected in [
            "file:///etc/passwd",
            "data:image/png;base64,AA==",
            "javascript:alert(1)",
            "http://user:pass@8.8.8.8/favicon.ico",
            "https://user@8.8.8.8/favicon.ico",
        ] {
            assert!(
                cache_key(rejected, Some("https://8.8.8.8/")).is_none(),
                "{rejected}"
            );
        }

        for private in [
            "http://127.0.0.1:8080/favicon.ico",
            "http://localhost:3000/favicon.ico",
            "http://169.254.169.254/latest/meta-data/",
            "http://10.0.0.1/favicon.ico",
            "http://printer.local/favicon.ico",
            "http://[::1]:3000/favicon.ico",
        ] {
            let public_page = cache_key(private, Some("https://8.8.8.8/")).unwrap();
            assert!(target(&public_page).is_none(), "{private}");
            let no_page = cache_key(private, None).unwrap();
            assert!(target(&no_page).is_none(), "{private}");
        }

        let cross_port = cache_key(
            "http://localhost:8080/favicon.ico",
            Some("http://localhost:3000/app"),
        )
        .unwrap();
        assert!(target(&cross_port).is_none());

        let local_same_origin = cache_key(
            "http://localhost:3000/favicon.ico",
            Some("http://localhost:3000/app"),
        )
        .unwrap();
        let local_target = target(&local_same_origin).unwrap();
        assert!(matches!(local_target, Target::SameOrigin { .. }));
        assert!(follows(
            &local_target,
            &reqwest::Url::parse("http://localhost:3000/assets/icon.png").unwrap()
        ));
        assert!(!follows(
            &local_target,
            &reqwest::Url::parse("http://localhost:8080/icon.png").unwrap()
        ));
        assert!(!follows(
            &local_target,
            &reqwest::Url::parse("https://8.8.8.8/icon.png").unwrap()
        ));

        let public_key =
            cache_key("https://8.8.8.8/favicon.ico", Some("https://1.1.1.1/")).unwrap();
        let public_target = target(&public_key).unwrap();
        assert!(matches!(public_target, Target::Public(_)));
        assert!(follows(
            &public_target,
            &reqwest::Url::parse("https://1.1.1.1/icon.png").unwrap()
        ));
        assert!(!follows(
            &public_target,
            &reqwest::Url::parse("http://127.0.0.1/secret").unwrap()
        ));
        assert!(!follows(
            &public_target,
            &reqwest::Url::parse("http://localhost/secret").unwrap()
        ));
        assert!(!follows(
            &public_target,
            &reqwest::Url::parse("file:///etc/passwd").unwrap()
        ));
        assert!(!follows(
            &public_target,
            &reqwest::Url::parse("https://user:pass@1.1.1.1/icon.png").unwrap()
        ));

        let public_for_local_icon = cache_key(
            "http://localhost:3000/favicon.ico",
            Some("https://8.8.8.8/"),
        )
        .unwrap();
        assert_ne!(local_same_origin, public_for_local_icon);
    }
}
