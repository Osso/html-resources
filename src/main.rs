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
        let Ok(selector) = Selector::parse(selector_str) else {
            continue;
        };
        for element in document.select(&selector) {
            let Some(value) = element.value().attr(attr) else {
                continue;
            };
            if let Some(url) = resolve_url(base_url, value) {
                resources.insert((url, *resource_type));
            }
        }
    }

    if let Ok(selector) = Selector::parse("[srcset]") {
        for element in document.select(&selector) {
            let Some(srcset) = element.value().attr("srcset") else {
                continue;
            };
            for url in collect_srcset_urls(srcset, base_url) {
                resources.insert((url, "srcset"));
            }
        }
    }

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

fn print_results(results: &[Resource], missing_only: bool, json: bool) {
    if json {
        print_json(results, missing_only);
    } else {
        print_text(results, missing_only);
    }
}

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

fn print_text(results: &[Resource], missing_only: bool) {
    let mut ok_count = 0;
    let mut failed_count = 0;

    for r in results {
        match &r.status {
            ResourceStatus::Ok(_) => {
                ok_count += 1;
                if !missing_only {
                    println!("✓ [{}] {}", r.resource_type, r.url);
                }
            }
            ResourceStatus::Failed(err) => {
                failed_count += 1;
                println!("✗ [{}] {} - {}", r.resource_type, r.url, err);
            }
        }
    }

    eprintln!("\n{} ok, {} missing", ok_count, failed_count);
}
