//! Import direct HTTP/HTTPS downloads from a user-supplied HTML page.
//!
//! This deliberately ignores magnet links and `.torrent` files. It is a small static-HTML
//! importer, not a browser automation engine: pages that render their catalog entirely with
//! JavaScript need a site-specific provider.

use scraper::{ElementRef, Html, Selector};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};
use tokio::io::AsyncWriteExt;
use url::Url;

use crate::CmdResult;

const MAX_HTML_BYTES: usize = 8 * 1024 * 1024;
const MAX_ITEMS: usize = 500;

#[derive(Debug, Clone, Serialize)]
pub struct WebLibraryItem {
    pub name: String,
    pub url: String,
    pub image: Option<String>,
    pub kind: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebLibrary {
    pub source_url: String,
    pub title: String,
    pub image: Option<String>,
    pub items: Vec<WebLibraryItem>,
    pub scanned_pages: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct WebSource {
    pub source_url: String,
    pub title: String,
    pub image: Option<String>,
}

struct ParsedPage {
    title: String,
    image: Option<String>,
    items: Vec<WebLibraryItem>,
    links: Vec<Url>,
}

#[derive(Clone, Serialize)]
struct DownloadProgress<'a> {
    url: &'a str,
    name: &'a str,
    downloaded: u64,
    total: Option<u64>,
    state: &'a str,
    path: Option<String>,
}

#[derive(Clone, Serialize)]
struct CrawlProgress<'a> {
    checked: usize,
    total: usize,
    current: &'a str,
}

fn http_url(raw: &str) -> CmdResult<Url> {
    let url = Url::parse(raw.trim()).map_err(|e| format!("That is not a valid URL: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Only http:// and https:// website links are supported.".into());
    }
    if url.host_str().is_none() || !url.username().is_empty() || url.password().is_some() {
        return Err(
            "The URL must be a normal website address without embedded credentials.".into(),
        );
    }
    Ok(url)
}

fn client() -> CmdResult<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent("Steam-Transfer/2.4 website-library")
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(45))
        .redirect(reqwest::redirect::Policy::limited(8))
        .build()
        .map_err(|e| format!("Could not create the web client: {e}"))
}

pub async fn scrape(
    raw_url: &str,
    crawl_linked_pages: bool,
    app: Option<&AppHandle>,
) -> CmdResult<WebLibrary> {
    let requested = http_url(raw_url)?;
    let client = client()?;
    let (final_url, html) = fetch_html(&client, requested).await?;
    let root = parse_page(&final_url, &html);
    let ParsedPage {
        title,
        image,
        mut items,
        links,
    } = root;
    let mut scanned_pages = 1;

    if crawl_linked_pages {
        let origin = final_url.origin();
        let mut seen_pages = HashSet::from([final_url.to_string()]);
        let pages: Vec<Url> = links
            .into_iter()
            .filter(|url| url.origin() == origin && seen_pages.insert(url.to_string()))
            .collect();
        let total = pages.len();
        let mut seen_items: HashSet<String> = items.iter().map(|item| item.url.clone()).collect();
        let mut title_indices: HashMap<String, usize> = items
            .iter()
            .enumerate()
            .map(|(index, item)| (title_key(&item.name), index))
            .collect();
        for (index, url) in pages.into_iter().enumerate() {
            if let Some(app) = app {
                let _ = app.emit(
                    "web-library-progress",
                    CrawlProgress {
                        checked: index,
                        total,
                        current: url.as_str(),
                    },
                );
            }
            let Ok((page_url, html)) = fetch_html(&client, url).await else {
                continue;
            };
            let page = parse_page(&page_url, &html);
            scanned_pages += 1;
            for item in page.items {
                if items.len() >= MAX_ITEMS {
                    break;
                }
                if !seen_items.insert(item.url.clone()) {
                    continue;
                }
                let key = title_key(&item.name);
                if let Some(&index) = title_indices.get(&key) {
                    if items[index].image.is_none() && item.image.is_some() {
                        items[index].image = item.image;
                    }
                    continue;
                }
                title_indices.insert(key, items.len());
                items.push(item);
            }
        }
        if let Some(app) = app {
            let _ = app.emit(
                "web-library-progress",
                CrawlProgress {
                    checked: total,
                    total,
                    current: "",
                },
            );
        }
    }

    keep_magnet_items(&mut items);

    Ok(WebLibrary {
        source_url: final_url.to_string(),
        title,
        image,
        items,
        scanned_pages,
    })
}

pub async fn inspect_source(raw_url: &str) -> CmdResult<WebSource> {
    let requested = http_url(raw_url)?;
    let client = client()?;
    let (final_url, html) = fetch_html(&client, requested).await?;
    let page = parse_page(&final_url, &html);
    Ok(WebSource {
        source_url: final_url.to_string(),
        title: page.title,
        image: page.image,
    })
}

pub async fn search_source(raw_url: &str, query: &str) -> CmdResult<WebLibraryItem> {
    let query = clean_text(query);
    if query.is_empty() {
        return Err("Enter a title to search for.".into());
    }
    let base = http_url(raw_url)?;
    let mut search_url = base.clone();
    let existing: Vec<(String, String)> = search_url
        .query_pairs()
        .filter(|(key, _)| key != "s")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    search_url.set_query(None);
    {
        let mut pairs = search_url.query_pairs_mut();
        for (key, value) in existing {
            pairs.append_pair(&key, &value);
        }
        pairs.append_pair("s", &query);
    }

    let client = client()?;
    let (resolved_search_url, html) = fetch_html(&client, search_url).await?;
    let (result_url, result_title) = find_first_search_result(&resolved_search_url, &html, &query)
        .ok_or_else(|| format!("No result title containing ‘{query}’ was found."))?;
    let (page_url, result_html) = fetch_html(&client, result_url).await?;
    let page = parse_page(&page_url, &result_html);
    let magnet = page.items.into_iter().next().ok_or_else(|| {
        format!("The first matching result, ‘{result_title}’, did not contain a magnet link.")
    })?;
    Ok(WebLibraryItem {
        name: result_title,
        url: magnet.url,
        image: magnet.image.or(page.image),
        kind: "magnet".into(),
    })
}

fn find_first_search_result(base: &Url, source: &str, query: &str) -> Option<(Url, String)> {
    let doc = Html::parse_document(source);
    let anchor_sel = Selector::parse("a[href]").expect("constant selector");
    let image_sel =
        Selector::parse("img[src], img[data-src], img[alt]").expect("constant selector");
    let query_key = title_key(query);
    if query_key.is_empty() {
        return None;
    }
    for anchor in doc.select(&anchor_sel) {
        let label = element_label(&anchor, &image_sel);
        if label.is_empty() || !title_key(&label).contains(&query_key) {
            continue;
        }
        let href = anchor.value().attr("href")?;
        let Ok(mut url) = base.join(href) else {
            continue;
        };
        url.set_fragment(None);
        if !should_crawl(base, &url) || url == *base {
            continue;
        }
        return Some((url, label));
    }
    None
}

async fn fetch_html(client: &reqwest::Client, requested: Url) -> CmdResult<(Url, String)> {
    let response = client
        .get(requested)
        .send()
        .await
        .map_err(|e| format!("Could not open that website: {e}"))?
        .error_for_status()
        .map_err(|e| format!("The website returned an error: {e}"))?;

    let final_url = response.url().clone();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if !content_type.is_empty()
        && !content_type.contains("html")
        && !content_type.starts_with("text/")
    {
        return Err("That URL is a file, not an HTML library page.".into());
    }
    if response
        .content_length()
        .is_some_and(|n| n > MAX_HTML_BYTES as u64)
    {
        return Err("That page is too large to import safely (maximum 8 MB).".into());
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|e| format!("Could not read the page: {e}"))?;
    if bytes.len() > MAX_HTML_BYTES {
        return Err("That page is too large to import safely (maximum 8 MB).".into());
    }
    Ok((final_url, String::from_utf8_lossy(&bytes).into_owned()))
}

#[cfg(test)]
fn parse_html(base: &Url, source: &str) -> WebLibrary {
    let page = parse_page(base, source);
    WebLibrary {
        source_url: base.to_string(),
        title: page.title,
        image: page.image,
        items: page.items,
        scanned_pages: 1,
    }
}

fn parse_page(base: &Url, source: &str) -> ParsedPage {
    let doc = Html::parse_document(source);
    let title_sel = Selector::parse("title").expect("constant selector");
    let meta_sel = Selector::parse("meta[property], meta[name]").expect("constant selector");
    let candidate_sel =
        Selector::parse("a[href], [data-magnet], [data-url], [data-href], [onclick]")
            .expect("constant selector");
    let image_sel = Selector::parse("img[src], img[data-src]").expect("constant selector");

    let mut og_title = None;
    let mut og_image = None;
    for meta in doc.select(&meta_sel) {
        let key = meta
            .value()
            .attr("property")
            .or_else(|| meta.value().attr("name"))
            .unwrap_or("")
            .to_ascii_lowercase();
        let value = meta.value().attr("content").unwrap_or("").trim();
        match key.as_str() {
            "og:title" if !value.is_empty() => og_title = Some(value.to_string()),
            "og:image" | "twitter:image" if og_image.is_none() && !value.is_empty() => {
                og_image = base.join(value).ok().map(|u| u.to_string())
            }
            _ => {}
        }
    }
    let title = og_title
        .or_else(|| {
            doc.select(&title_sel)
                .next()
                .map(|e| clean_text(e.text().collect::<Vec<_>>().join(" ")))
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| base.host_str().unwrap_or("Website library").to_string());

    let mut seen = HashSet::new();
    let mut title_indices: HashMap<String, usize> = HashMap::new();
    let mut items: Vec<WebLibraryItem> = Vec::new();
    let mut seen_links = HashSet::new();
    let mut links = Vec::new();
    for candidate in doc.select(&candidate_sel) {
        let magnet = magnet_from_element(&candidate, &image_sel);
        if magnet.is_none() {
            if let Some(href) = candidate.value().attr("href") {
                if let Ok(mut url) = base.join(href) {
                    url.set_fragment(None);
                    if should_crawl(base, &url) && seen_links.insert(url.to_string()) {
                        links.push(url);
                    }
                }
            }
            continue;
        }
        if items.len() >= MAX_ITEMS {
            continue;
        }
        let canonical = magnet.unwrap();
        if !seen.insert(canonical.clone()) {
            continue;
        }

        let image_node = (candidate.value().name() == "img")
            .then_some(candidate)
            .or_else(|| candidate.select(&image_sel).next());
        let image = image_node
            .and_then(|img| {
                img.value()
                    .attr("src")
                    .or_else(|| img.value().attr("data-src"))
            })
            .and_then(|src| base.join(src).ok())
            .map(|u| u.to_string())
            .or_else(|| og_image.clone());
        let image_alt = image_node
            .and_then(|img| img.value().attr("alt"))
            .map(clean_text)
            .filter(|s| !s.is_empty());
        let text = element_label(&candidate, &image_sel);
        let name = image_alt
            .filter(|value| !generic_magnet_label(value))
            .or_else(|| (!text.is_empty() && !generic_magnet_label(&text)).then_some(text))
            .or_else(|| magnet_display_name(&canonical))
            .unwrap_or_else(|| title.clone());
        let key = title_key(&name);
        if let Some(&index) = title_indices.get(&key) {
            if items[index].image.is_none() && image.is_some() {
                items[index].image = image;
            }
            continue;
        }
        title_indices.insert(key, items.len());
        items.push(WebLibraryItem {
            name,
            url: canonical,
            image,
            kind: "magnet".into(),
        });
    }

    ParsedPage {
        title,
        image: og_image,
        items,
        links,
    }
}

fn magnet_from_element(element: &ElementRef<'_>, image_sel: &Selector) -> Option<String> {
    const ATTRS: [&str; 7] = [
        "href",
        "data-magnet",
        "data-url",
        "data-href",
        "data-download",
        "onclick",
        "value",
    ];
    for name in ATTRS {
        if let Some(magnet) = element.value().attr(name).and_then(magnet_from_value) {
            return Some(magnet);
        }
    }
    for image in element.select(image_sel) {
        for name in ATTRS {
            if let Some(magnet) = image.value().attr(name).and_then(magnet_from_value) {
                return Some(magnet);
            }
        }
    }
    None
}

fn magnet_from_value(value: &str) -> Option<String> {
    let decoded = percent_decode_lossy(
        &value
            .replace("\\u003a", ":")
            .replace("\\u003A", ":")
            .replace("\\x3a", ":")
            .replace("\\x3A", ":")
            .replace("\\u003f", "?")
            .replace("\\u003F", "?")
            .replace("\\x3f", "?")
            .replace("\\x3F", "?")
            .replace("\\u0026", "&")
            .replace("\\/", "/"),
    );
    let lower = decoded.to_ascii_lowercase();
    let start = lower.find("magnet:?")?;
    let tail = &decoded[start..];
    let end = tail
        .char_indices()
        .find_map(|(index, c)| {
            (index > 0 && (c.is_whitespace() || matches!(c, '\'' | '"' | ')' | '<' | '>')))
                .then_some(index)
        })
        .unwrap_or(tail.len());
    let magnet = tail[..end]
        .trim_end_matches([',', ';'])
        .replace("&amp;", "&");
    Url::parse(&magnet)
        .ok()
        .filter(|url| url.scheme() == "magnet")
        .map(|url| url.to_string())
}

fn percent_decode_lossy(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn element_label(element: &ElementRef<'_>, image_sel: &Selector) -> String {
    let mut parts = Vec::new();
    for name in ["aria-label", "title"] {
        if let Some(value) = element.value().attr(name) {
            parts.push(value);
        }
    }
    let visible = clean_text(element.text().collect::<Vec<_>>().join(" "));
    if !visible.is_empty() {
        parts.push(&visible);
    }
    if element.value().name() == "img" {
        for name in ["alt", "title", "aria-label"] {
            if let Some(value) = element.value().attr(name) {
                parts.push(value);
            }
        }
    }
    for image in element.select(image_sel) {
        for name in ["alt", "title", "aria-label"] {
            if let Some(value) = image.value().attr(name) {
                parts.push(value);
            }
        }
    }
    clean_text(parts.join(" "))
}

fn generic_magnet_label(value: &str) -> bool {
    let mut key = title_key(value);
    for word in [
        "download", "magnet", "torrent", "link", "button", "click", "here",
    ] {
        key = key.replace(word, "");
    }
    key.is_empty()
}

fn magnet_display_name(magnet: &str) -> Option<String> {
    Url::parse(magnet)
        .ok()?
        .query_pairs()
        .find_map(|(key, value)| {
            (key == "dn" && !value.trim().is_empty()).then(|| value.into_owned())
        })
}

fn should_crawl(base: &Url, url: &Url) -> bool {
    if !matches!(url.scheme(), "http" | "https") || url.origin() != base.origin() {
        return false;
    }
    let path = url.path().to_ascii_lowercase();
    if [
        "logout", "signout", "login", "signin", "register", "account", "cart", "checkout",
    ]
    .iter()
    .any(|word| path.contains(word))
    {
        return false;
    }
    let ext = Path::new(url.path())
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    ext.is_empty() || matches!(ext.as_str(), "html" | "htm" | "php" | "asp" | "aspx")
}

fn clean_text(value: impl AsRef<str>) -> String {
    value
        .as_ref()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn title_key(value: &str) -> String {
    value
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn keep_magnet_items(items: &mut Vec<WebLibraryItem>) {
    items.retain(|item| item.kind == "magnet");
}

fn filename_from_url(url: &Url) -> String {
    url.path_segments()
        .and_then(|mut parts| parts.next_back())
        .filter(|s| !s.is_empty())
        .map(|s| s.replace("%20", " "))
        .unwrap_or_else(|| "download".into())
}

fn safe_filename(value: &str) -> String {
    let mut name: String = value
        .chars()
        .map(|c| {
            if c.is_control() || "<>:\"/\\|?*".contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    name = name.trim().trim_matches('.').to_string();
    if name.is_empty() {
        name = "download".into();
    }
    if name.len() > 180 {
        name.truncate(180);
    }
    name
}

fn unique_path(folder: &Path, name: &str) -> PathBuf {
    let first = folder.join(name);
    if !first.exists() {
        return first;
    }
    let path = Path::new(name);
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("download");
    let ext = path.extension().and_then(|s| s.to_str());
    for n in 2..10_000 {
        let candidate = match ext {
            Some(ext) => folder.join(format!("{stem} ({n}).{ext}")),
            None => folder.join(format!("{stem} ({n})")),
        };
        if !candidate.exists() {
            return candidate;
        }
    }
    folder.join(format!("{stem}-{}", std::process::id()))
}

pub async fn download(
    app: &AppHandle,
    raw_url: &str,
    folder: &Path,
    display_name: &str,
) -> CmdResult<String> {
    let url = http_url(raw_url)?;
    tokio::fs::create_dir_all(folder)
        .await
        .map_err(|e| format!("Could not create the download folder: {e}"))?;
    let fallback = filename_from_url(&url);
    let supplied = if Path::new(display_name).extension().is_some() {
        display_name
    } else {
        &fallback
    };
    let name = safe_filename(supplied);
    let target = unique_path(folder, &name);
    let part = target.with_file_name(format!(
        "{}.steamtransfer-part",
        target.file_name().unwrap_or_default().to_string_lossy()
    ));

    let result = download_inner(app, &url, &name, &target, &part).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&part).await;
    }
    result.map(|_| target.to_string_lossy().into_owned())
}

async fn download_inner(
    app: &AppHandle,
    url: &Url,
    name: &str,
    target: &Path,
    part: &Path,
) -> CmdResult<()> {
    let mut response = client()?
        .get(url.clone())
        .send()
        .await
        .map_err(|e| format!("Could not start the download: {e}"))?
        .error_for_status()
        .map_err(|e| format!("The server refused the download: {e}"))?;
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if content_type.contains("text/html") {
        return Err("That link opened another web page instead of a downloadable file.".into());
    }
    let total = response.content_length();
    let mut file = tokio::fs::File::create(part)
        .await
        .map_err(|e| format!("Could not create the download file: {e}"))?;
    let mut downloaded = 0u64;
    let mut last_event = Instant::now() - Duration::from_secs(1);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("The download was interrupted: {e}"))?
    {
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("Could not write the download: {e}"))?;
        downloaded += chunk.len() as u64;
        if last_event.elapsed() >= Duration::from_millis(150) {
            let _ = app.emit(
                "web-download-progress",
                DownloadProgress {
                    url: url.as_str(),
                    name,
                    downloaded,
                    total,
                    state: "downloading",
                    path: None,
                },
            );
            last_event = Instant::now();
        }
    }
    file.flush()
        .await
        .map_err(|e| format!("Could not finish writing the download: {e}"))?;
    drop(file);
    tokio::fs::rename(part, target)
        .await
        .map_err(|e| format!("Could not finish the download: {e}"))?;
    let path = target.to_string_lossy().into_owned();
    let _ = app.emit(
        "web-download-progress",
        DownloadProgress {
            url: url.as_str(),
            name,
            downloaded,
            total,
            state: "done",
            path: Some(path),
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imports_only_unique_magnets() {
        let base = Url::parse("https://example.test/releases/").unwrap();
        let library = parse_html(
            &base,
            r##"
            <html><head><title>Open releases</title><meta property="og:image" content="cover.jpg"></head>
            <body>
              <a href="files/system.iso"><img src="system.png" alt="System 1.0"></a>
              <a href="files/system-mirror.iso">system-1.0</a>
              <a href="files/readme.txt">Read me</a>
              <a href="files/image.torrent">Torrent file</a>
              <a href="magnet:?xt=urn:btih:0123456789abcdef&amp;dn=System+Live">Magnet</a>
              <a href="magnet:?xt=urn:btih:fedcba9876543210&amp;dn=System+Live">magnet link</a>
              <a href="#" onclick="start('magnet:?xt=urn:btih:1111111111111111&amp;dn=Image+Release')"><img src="magnet.png" alt="Magnet download"></a>
              <button data-url="magnet%3A%3Fxt%3Durn%3Abtih%3A2222222222222222%26dn%3DButton%2BRelease">Magnet</button>
              <a href="get?id=4" download>Portable build</a>
            </body></html>"##,
        );
        assert_eq!(library.title, "Open releases");
        assert_eq!(library.items.len(), 3);
        assert_eq!(library.items[0].name, "System Live");
        assert_eq!(library.items[0].kind, "magnet");
        assert_eq!(library.items[1].name, "Image Release");
        assert_eq!(library.items[2].name, "Button Release");
        let mut filtered = library.items.clone();
        keep_magnet_items(&mut filtered);
        assert_eq!(filtered.len(), 3);
        assert_eq!(filtered[0].kind, "magnet");
    }

    #[test]
    fn filenames_are_safe_on_windows() {
        assert_eq!(safe_filename("disk: final?.iso"), "disk_ final_.iso");
    }

    #[test]
    fn collects_only_same_site_html_pages_for_crawling() {
        let base = Url::parse("https://example.test/catalog/").unwrap();
        let page = parse_page(
            &base,
            r#"
            <a href="release-one/">Release one</a>
            <a href="/release-two.html">Release two</a>
            <a href="https://other.test/release/">Other site</a>
            <a href="assets/poster.jpg">Poster</a>
            <a href="account/login">Login</a>
            "#,
        );
        assert_eq!(page.links.len(), 2);
        assert_eq!(
            page.links[0].as_str(),
            "https://example.test/catalog/release-one/"
        );
        assert_eq!(
            page.links[1].as_str(),
            "https://example.test/release-two.html"
        );
    }

    #[test]
    fn search_uses_first_title_containing_the_query() {
        let base = Url::parse("https://example.test/?s=search+test+ha").unwrap();
        let result = find_first_search_result(
            &base,
            r#"
            <a href="/unrelated/">Something else</a>
            <a href="/search-test-ha-release/"><img alt="Search Test Ha Complete Release"></a>
            <a href="/search-test-ha-second/">Search Test Ha Second Result</a>
            "#,
            "Search Test Ha",
        )
        .unwrap();
        assert_eq!(
            result.0.as_str(),
            "https://example.test/search-test-ha-release/"
        );
        assert!(result.1.contains("Search Test Ha Complete Release"));
    }
}
