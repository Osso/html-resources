#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

use anyhow::{Context, Result};
use clap::Parser;
use futures::stream::{self, StreamExt};
use reqwest::Client;
use scraper::{Html, Selector};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

#[derive(Parser)]
#[command(name = "html-resources", about = "Find missing resources in HTML")]
struct Args {
    /// URL or file path to check
    source: String,

    /// Concurrency limit for HTTP requests
    #[arg(short, long, default_value = "10")]
    concurrency: usize,

    /// Request timeout in seconds
    #[arg(short, long, default_value = "10")]
    timeout: u64,

    /// Only show missing resources
    #[arg(short, long)]
    missing_only: bool,

    /// Output as JSON
    #[arg(long)]
    json: bool,
}

#[derive(Debug)]
struct Resource {
    url: String,
    resource_type: &'static str,
    status: ResourceStatus,
}

#[derive(Debug)]
enum ResourceStatus {
    Ok(u16),
    Failed(String),
}

#[tokio::main]
#[cfg_attr(coverage_nightly, coverage(off))]
async fn main() -> Result<()> {
    let args = Args::parse();

    let (html, base_url) = fetch_html(&args.source).await?;
    let resources = extract_resources(&html, &base_url)?;

    let client = Client::builder()
        .timeout(Duration::from_secs(args.timeout))
        .build()?;

    let results: Vec<Resource> = stream::iter(resources)
        .map(|(url, resource_type)| {
            let client = client.clone();
            async move { check_resource(&client, url, resource_type).await }
        })
        .buffer_unordered(args.concurrency)
        .collect()
        .await;

    print_results(&results, args.missing_only, args.json);

    let missing_count = results
        .iter()
        .filter(|r| matches!(r.status, ResourceStatus::Failed(_)))
        .count();

    if missing_count > 0 {
        std::process::exit(1);
    }

    Ok(())
}

#[cfg_attr(coverage_nightly, coverage(off))]
async fn fetch_html(source: &str) -> Result<(String, Url)> {
    if source.starts_with("http://") || source.starts_with("https://") {
        let url = Url::parse(source)?;
        let html = reqwest::get(source)
            .await
            .context("Failed to fetch URL")?
            .text()
            .await?;
        Ok((html, url))
    } else {
        let path = PathBuf::from(source);
        let html = std::fs::read_to_string(&path).context("Failed to read file")?;
        let base = Url::from_file_path(&path.canonicalize()?)
            .map_err(|_| anyhow::anyhow!("Invalid file path"))?;
        Ok((html, base))
    }
}

fn collect_srcset_urls<'a>(
    srcset: &'a str,
    base_url: &'a Url,
) -> impl Iterator<Item = String> + 'a {
    srcset.split(',').filter_map(move |part| {
        let url_part = part.trim().split_whitespace().next()?;
        resolve_url(base_url, url_part)
    })
}

fn collect_attr_resources(
    document: &Html,
    base_url: &Url,
    selector_str: &str,
    attr: &str,
    resource_type: &'static str,
    resources: &mut HashSet<(String, &'static str)>,
) {
    let Ok(selector) = Selector::parse(selector_str) else {
        return;
    };
    for element in document.select(&selector) {
        let Some(value) = element.value().attr(attr) else {
            continue;
        };
        if let Some(url) = resolve_url(base_url, value) {
            resources.insert((url, resource_type));
        }
    }
}

fn collect_srcset_resources(
    document: &Html,
    base_url: &Url,
    resources: &mut HashSet<(String, &'static str)>,
) {
    let Ok(selector) = Selector::parse("[srcset]") else {
        return;
    };
    for element in document.select(&selector) {
        let Some(srcset) = element.value().attr("srcset") else {
            continue;
        };
        for url in collect_srcset_urls(srcset, base_url) {
            resources.insert((url, "srcset"));
        }
    }
}

fn extract_resources(html: &str, base_url: &Url) -> Result<Vec<(String, &'static str)>> {
    let document = Html::parse_document(html);
    let mut resources = HashSet::new();

    let extractors: &[(&str, &str, &'static str)] = &[
        ("img[src]", "src", "image"),
        ("script[src]", "src", "script"),
        ("link[href][rel=stylesheet]", "href", "stylesheet"),
        ("link[href][rel=icon]", "href", "icon"),
        ("link[href][rel='shortcut icon']", "href", "icon"),
        ("link[href][rel=preload]", "href", "preload"),
        ("video[src]", "src", "video"),
        ("audio[src]", "src", "audio"),
        ("source[src]", "src", "media"),
        ("iframe[src]", "src", "iframe"),
        ("embed[src]", "src", "embed"),
        ("object[data]", "data", "object"),
    ];

    for (selector_str, attr, resource_type) in extractors {
        collect_attr_resources(
            &document,
            base_url,
            selector_str,
            attr,
            resource_type,
            &mut resources,
        );
    }

    collect_srcset_resources(&document, base_url, &mut resources);

    Ok(resources.into_iter().collect())
}

fn resolve_url(base: &Url, href: &str) -> Option<String> {
    let trimmed = href.trim();

    // Skip data URLs, javascript, and anchors
    if trimmed.starts_with("data:")
        || trimmed.starts_with("javascript:")
        || trimmed.starts_with('#')
        || trimmed.is_empty()
    {
        return None;
    }

    base.join(trimmed).ok().map(|u| u.to_string())
}

#[cfg_attr(coverage_nightly, coverage(off))]
async fn check_resource(client: &Client, url: String, resource_type: &'static str) -> Resource {
    // Check file:// URLs by checking if file exists
    if url.starts_with("file://") {
        let path = url.strip_prefix("file://").unwrap();
        let status = if std::path::Path::new(path).exists() {
            ResourceStatus::Ok(200)
        } else {
            ResourceStatus::Failed("file not found".to_string())
        };
        return Resource {
            url,
            resource_type,
            status,
        };
    }

    let status = match client.head(&url).send().await {
        Ok(resp) => {
            let status_code = resp.status().as_u16();
            if resp.status().is_success() || resp.status().is_redirection() {
                ResourceStatus::Ok(status_code)
            } else {
                ResourceStatus::Failed(format!("HTTP {}", status_code))
            }
        }
        Err(e) => ResourceStatus::Failed(e.to_string()),
    };

    Resource {
        url,
        resource_type,
        status,
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn print_results(results: &[Resource], missing_only: bool, json: bool) {
    if json {
        print_json(results, missing_only);
    } else {
        print_text(results, missing_only);
    }
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn print_json(results: &[Resource], missing_only: bool) {
    let items: Vec<_> = results
        .iter()
        .filter(|r| !missing_only || matches!(r.status, ResourceStatus::Failed(_)))
        .map(|r| {
            let (status, error) = match &r.status {
                ResourceStatus::Ok(code) => (Some(*code), None),
                ResourceStatus::Failed(msg) => (None, Some(msg.as_str())),
            };
            serde_json::json!({
                "url": r.url,
                "type": r.resource_type,
                "status": status,
                "error": error,
            })
        })
        .collect();

    println!("{}", serde_json::to_string_pretty(&items).unwrap());
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn print_text(results: &[Resource], missing_only: bool) {
    let mut ok_count = 0;
    let mut failed_count = 0;

    for r in results {
        print_resource_status_line(r, missing_only, &mut ok_count, &mut failed_count);
    }

    eprintln!("\n{} ok, {} missing", ok_count, failed_count);
}

#[cfg_attr(coverage_nightly, coverage(off))]
fn print_resource_status_line(
    resource: &Resource,
    missing_only: bool,
    ok_count: &mut usize,
    failed_count: &mut usize,
) {
    match &resource.status {
        ResourceStatus::Ok(_) => {
            *ok_count += 1;
            if missing_only {
                return;
            }
            println!("✓ [{}] {}", resource.resource_type, resource.url);
        }
        ResourceStatus::Failed(err) => {
            *failed_count += 1;
            println!("✗ [{}] {} - {}", resource.resource_type, resource.url, err);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn base_url() -> Url {
        Url::parse("https://example.com/path/page.html").expect("base URL")
    }

    #[test]
    fn resolve_url_skips_non_fetchable_references() {
        let base = base_url();

        assert_eq!(resolve_url(&base, "data:image/png;base64,abc"), None);
        assert_eq!(resolve_url(&base, "javascript:alert(1)"), None);
        assert_eq!(resolve_url(&base, "#section"), None);
        assert_eq!(resolve_url(&base, "   "), None);
    }

    #[test]
    fn resolve_url_handles_absolute_relative_and_root_paths() {
        let base = base_url();

        assert_eq!(
            resolve_url(&base, "https://cdn.example.com/app.js").as_deref(),
            Some("https://cdn.example.com/app.js")
        );
        assert_eq!(
            resolve_url(&base, "image.png").as_deref(),
            Some("https://example.com/path/image.png")
        );
        assert_eq!(
            resolve_url(&base, "/assets/site.css").as_deref(),
            Some("https://example.com/assets/site.css")
        );
    }

    #[test]
    fn collect_srcset_urls_extracts_first_token_and_resolves_urls() {
        let base = base_url();
        let urls: Vec<_> = collect_srcset_urls(
            "small.png 1x, /large.png 2x, https://cdn.example.com/full.png 3x",
            &base,
        )
        .collect();

        assert_eq!(
            urls,
            vec![
                "https://example.com/path/small.png".to_string(),
                "https://example.com/large.png".to_string(),
                "https://cdn.example.com/full.png".to_string(),
            ]
        );
    }

    #[test]
    fn extract_resources_finds_supported_attributes_and_srcset() {
        let base = base_url();
        let html = r#"
            <html>
              <head>
                <link rel="stylesheet" href="/site.css">
                <link rel="icon" href="favicon.ico">
                <script src="app.js"></script>
              </head>
              <body>
                <img src="hero.png" srcset="hero-1x.png 1x, /hero-2x.png 2x">
                <video src="/movie.mp4"></video>
                <iframe src="frame.html"></iframe>
                <object data="/file.pdf"></object>
              </body>
            </html>
        "#;

        let mut resources = extract_resources(html, &base).expect("resources");
        resources.sort();

        assert!(resources.contains(&("https://example.com/site.css".to_string(), "stylesheet")));
        assert!(resources.contains(&("https://example.com/path/favicon.ico".to_string(), "icon")));
        assert!(resources.contains(&("https://example.com/path/app.js".to_string(), "script")));
        assert!(resources.contains(&("https://example.com/path/hero.png".to_string(), "image")));
        assert!(
            resources.contains(&("https://example.com/path/hero-1x.png".to_string(), "srcset"))
        );
        assert!(resources.contains(&("https://example.com/hero-2x.png".to_string(), "srcset")));
        assert!(resources.contains(&("https://example.com/movie.mp4".to_string(), "video")));
        assert!(resources.contains(&("https://example.com/path/frame.html".to_string(), "iframe")));
        assert!(resources.contains(&("https://example.com/file.pdf".to_string(), "object")));
    }

    #[test]
    fn extract_resources_deduplicates_urls_by_type() {
        let base = base_url();
        let html = r#"<img src="same.png"><img src="same.png"><script src="same.png"></script>"#;

        let resources = extract_resources(html, &base).expect("resources");

        assert_eq!(resources.len(), 2);
        assert!(resources.contains(&("https://example.com/path/same.png".to_string(), "image")));
        assert!(resources.contains(&("https://example.com/path/same.png".to_string(), "script")));
    }

    #[test]
    fn clap_definition_is_valid() {
        Args::command().debug_assert();
    }
}
