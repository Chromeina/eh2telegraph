/// nhentai collector.
/// Host matching: nhentai.to or nhentai.net
///
/// Since nhentai.net always enable CloudFlare Firewall, so we will
/// use nhentai.xxx(but there is about 1~2 days syncing latency).
use again::RetryPolicy;
use ipnet::Ipv6Net;
use rand::seq::SliceRandom;
use regex::Regex;
use reqwest::Response;
use std::time::Duration;

use crate::{
    http_client::{GhostClient, GhostClientBuilder},
    stream::AsyncStream,
    util::get_bytes,
    util::match_first_group,
};

use super::{AlbumMeta, Collector, ImageData, ImageMeta};

lazy_static::lazy_static! {
    static ref TITLE_RE: Regex = Regex::new(r#"<span class="pretty">(.*?)</span>"#).unwrap();
    static ref PAGE_RE: Regex = Regex::new(r#"<noscript><img src="(https://cdn\.(?:nhentai\.xxx|hentaibomb\.com)/g/\d+/\d+t\.\w+)"#).unwrap();

    static ref RETRY_POLICY: RetryPolicy = RetryPolicy::fixed(Duration::from_millis(200))
        .with_max_retries(5)
        .with_jitter(true);
}

const DOMAIN_LIST: [&str; 12] = [
    "nhentai.net",
    "i.nhentai.net",
    "i2.nhentai.net",
    "i3.nhentai.net",
    "i4.nhentai.net",
    "i5.nhentai.net",
    "i6.nhentai.net",
    "i7.nhentai.net",
    "i8.nhentai.net",
    "i9.nhentai.net",
    "nhentai.xxx",
    "cdn.nhentai.xxx",
];

const NH_CDN_LIST: [&str; 5] = [
    "https://i.nhentai.net/galleries",
    "https://i2.nhentai.net/galleries",
    "https://i3.nhentai.net/galleries",
    "https://i5.nhentai.net/galleries",
    "https://i7.nhentai.net/galleries",
];

#[derive(Debug, Clone, Default)]
pub struct NHCollector {
    client: GhostClient,
}

impl NHCollector {
    pub fn new(prefix: Option<Ipv6Net>) -> Self {
        Self {
            client: GhostClientBuilder::default()
                .with_cf_resolve(&DOMAIN_LIST)
                .build(prefix),
        }
    }

    pub fn new_from_config() -> anyhow::Result<Self> {
        Ok(Self {
            client: GhostClientBuilder::default()
                .with_cf_resolve(&DOMAIN_LIST)
                .build_from_config()?,
        })
    }
}

impl Collector for NHCollector {
    type FetchError = anyhow::Error;
    type FetchFuture<'a> =
        impl std::future::Future<Output = anyhow::Result<(AlbumMeta, Self::ImageStream)>> + 'a;

    type StreamError = anyhow::Error;
    type ImageStream = NHImageStream;

    #[inline]
    fn name() -> &'static str {
        "nhentai"
    }

    fn fetch(&self, path: String) -> Self::FetchFuture<'_> {
        async move {
            // normalize url
            let mut parts = path.trim_matches(|c| c == '/').split('/');
            let g = parts.next();
            let album_id = parts.next();
            let album_id = match (g, album_id) {
                (Some("g"), Some(album_id)) => album_id,
                _ => {
                    return Err(anyhow::anyhow!("invalid input path({path}), gallery url is expected(like https://nhentai.net/g/333678)"));
                }
            };
            // Note: Since nh enables CF firewall, we use nhentai.xxx instead.
            // let url = format!("https://nhentai.net/g/{album_id}");
            let url = format!("https://nhentai.xxx/g/{album_id}");
            tracing::info!("[nhentai] process {url}");

            // clone client to force changing ip
            let client = self.client.clone();
            let index = client
                .get(&url)
                .send()
                .await
                .and_then(Response::error_for_status)?
                .text()
                .await?;

            let title = match_first_group(&TITLE_RE, &index)
                .unwrap_or("No Title")
                .to_string();
            let image_urls = PAGE_RE
                .captures_iter(&index)
                .map(|c| {
                    let thumb_url = c
                        .get(1)
                        .expect("regexp is matched but no group 1 found")
                        .as_str();
                    ImageURL(
                        thumb_url
                            .replace("https://t", "https://i")
                            .replace("t.", "."),
                    )
                })
                .collect::<Vec<_>>()
                .into_iter();

            Ok((
                AlbumMeta {
                    link: url,
                    name: title,
                    class: None,
                    description: None,
                    authors: None,
                    tags: None,
                },
                NHImageStream { client, image_urls },
            ))
        }
    }
}

#[derive(Debug)]
struct ImageURL(String);

impl ImageURL {
    fn raw(&self) -> &str {
        &self.0
    }

    fn fallback(&self) -> String {
        let cdn = NH_CDN_LIST
            .choose(&mut rand::thread_rng())
            .expect("empty CDN list");
        self.0.replace("https://cdn.nhentai.xxx/g", cdn)
    }
}

#[derive(Debug)]
pub struct NHImageStream {
    client: GhostClient,
    image_urls: std::vec::IntoIter<ImageURL>,
}

impl NHImageStream {
    async fn load_image(client: GhostClient, link: &str) -> anyhow::Result<(ImageMeta, ImageData)> {
        let image_data = RETRY_POLICY
            .retry(|| async { get_bytes(&client, link).await })
            .await?;

        tracing::trace!(
            "download nhentai image with size {}, link: {link}",
            image_data.len()
        );
        let meta = ImageMeta {
            id: link.to_string(),
            url: link.to_string(),
            description: None,
        };
        Ok((meta, image_data))
    }
}

impl AsyncStream for NHImageStream {
    type Item = anyhow::Result<(ImageMeta, ImageData)>;

    type Future = impl std::future::Future<Output = Self::Item>;

    fn next(&mut self) -> Option<Self::Future> {
        let link = self.image_urls.next()?;
        let client = self.client.clone();
        Some(async move {
            match Self::load_image(client.clone(), link.raw()).await {
                Ok(r) => Ok(r),
                Err(e) => {
                    tracing::error!("fallback for nh image {link:?}: {e}");
                    Self::load_image(client, &link.fallback()).await
                }
            }
        })
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.image_urls.size_hint()
    }
}
