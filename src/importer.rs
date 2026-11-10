use anyhow::anyhow;
use rand::Rng;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::App;
use crate::crawl::{CrawlError, crawl_site, normalize_url};
use crate::error::{AppResult, user_error};
use crate::extract::{extract_catalog, verify_catalog};
use crate::model::{Product, ProgressStep, Shop, ShopStatus, now};
use crate::money::to_minor;
use crate::safe_fetch::FetchError;

#[derive(Debug, Serialize, Deserialize)]
pub struct ImportJob {
    pub slug: String,
}

/// An import failure whose message is safe to show to the owner.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct VisibleFailure(String);

fn random_hex(bytes: usize) -> String {
    let mut rng = rand::rng();
    (0..bytes).map(|_| format!("{:02x}", rng.random::<u8>())).collect()
}

fn random_key() -> String {
    random_hex(24)
}

fn slug_base(url: &Url, is_sample: bool) -> String {
    let source = if is_sample {
        url.path_segments().and_then(|mut segments| segments.nth(1)).unwrap_or("shop").to_owned()
    } else {
        url.host_str().unwrap_or("shop").trim_start_matches("www.").split('.').next().unwrap_or("shop").to_owned()
    };
    let cleaned: String = source.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() { "shop".to_owned() } else { trimmed.chars().take(30).collect() }
}

/// Creates the shop and hands the slow work to a background job.
pub async fn start_import(app: &App, input: &str, base_url: &str) -> AppResult<Shop> {
    let url = app.fetcher.check_url(&normalize_url(input)).map_err(|error| user_error(error.to_string()))?;
    // Sample shops are hosted by this app, so their ownership is already known.
    let is_sample = url.as_str().starts_with(&format!("{base_url}/demo/"));
    let base = slug_base(&url, is_sample);
    let mut slug = format!("{base}-{}", random_hex(2));
    while app.store.get_shop(&slug).await?.is_some() {
        slug = format!("{base}-{}", random_hex(3));
    }
    let shop = Shop {
        slug,
        owner_key: random_key(),
        source_url: url.to_string(),
        status: ShopStatus::Importing,
        progress: vec![],
        error: None,
        name: String::new(),
        description: String::new(),
        location: String::new(),
        contact: String::new(),
        hours: String::new(),
        currency: "USD".to_owned(),
        ships: false,
        pickup: true,
        shipping_minor: 0,
        shipping_policy: String::new(),
        returns_policy: String::new(),
        paypal_email: String::new(),
        verified: is_sample,
        verify_token: random_hex(12),
        warnings: vec![],
        created_at: now(),
        published_at: None,
    };
    app.store.put_shop(&shop).await?;
    app.dispatch_import(&shop.slug).await?;
    Ok(shop)
}

fn friendly_error(error: &anyhow::Error) -> String {
    if let Some(visible) = error.downcast_ref::<VisibleFailure>() {
        return visible.0.clone();
    }
    match error.downcast_ref::<CrawlError>() {
        Some(CrawlError::RobotsDisallowed) => CrawlError::RobotsDisallowed.to_string(),
        Some(CrawlError::Fetch(FetchError::Unsafe(message))) => message.clone(),
        Some(CrawlError::Fetch(FetchError::Failed { .. })) => "We couldn't open that website. Check the address and try again.".to_owned(),
        None => "Something went wrong while reading that website. Please try again.".to_owned(),
    }
}

/// Runs one import to completion. Failures are saved on the shop rather than returned,
/// so Lambda does not retry a job the owner has already been told about.
pub async fn run_import(app: &App, slug: &str) {
    let mut shop = match app.store.get_shop(slug).await {
        Ok(Some(shop)) if shop.status == ShopStatus::Importing => shop,
        Ok(_) => return,
        Err(error) => {
            tracing::error!(?error, slug, "could not load shop for import");
            return;
        }
    };
    if let Err(error) = import(app, &mut shop).await {
        tracing::error!(?error, slug, "import failed");
        shop.status = ShopStatus::Failed;
        shop.error = Some(friendly_error(&error));
        if let Err(error) = app.store.put_shop(&shop).await {
            tracing::error!(?error, slug, "could not save failed import");
        }
    }
}

fn label_current_step(shop: &mut Shop, pages: usize) {
    if let Some(step) = shop.progress.last_mut() {
        step.label = format!("Reading your website ({pages} {})", if pages == 1 { "page" } else { "pages" });
    }
}

async fn advance(app: &App, shop: &mut Shop, label: &str) -> anyhow::Result<()> {
    shop.progress.iter_mut().for_each(|step| step.done = true);
    shop.progress.push(ProgressStep { label: label.to_owned(), done: false });
    app.store.put_shop(shop).await
}

async fn import(app: &App, shop: &mut Shop) -> anyhow::Result<()> {
    if !app.llm.configured() {
        return Err(anyhow!(VisibleFailure("The AI reader is not configured on this server.".to_owned())));
    }
    advance(app, shop, "Opening your website").await?;
    let source = shop.source_url.clone();
    let pages = crawl_site(&app.fetcher, &source, |count| {
        let (app, mut snapshot) = (app.clone(), shop.clone());
        async move {
            label_current_step(&mut snapshot, count);
            if let Err(error) = app.store.put_shop(&snapshot).await {
                tracing::warn!(?error, "could not save crawl progress");
            }
        }
    })
    .await?;
    label_current_step(shop, pages.len());
    if pages.iter().map(|page| page.text.len()).sum::<usize>() < 80 {
        return Err(anyhow!(VisibleFailure("We couldn't find readable text on that page.".to_owned())));
    }

    advance(app, shop, "Finding your products and prices").await?;
    let raw = extract_catalog(&app.llm, &pages).await?;

    advance(app, shop, "Checking every price against your pages").await?;
    let catalog = verify_catalog(raw, &pages);

    shop.name = if catalog.shop.name.trim().is_empty() {
        Url::parse(&shop.source_url).ok().and_then(|url| url.host_str().map(str::to_owned)).unwrap_or_default()
    } else {
        catalog.shop.name.trim().to_owned()
    };
    shop.description = catalog.shop.description;
    shop.location = catalog.shop.location;
    shop.contact = catalog.shop.contact;
    shop.hours = catalog.shop.hours;
    shop.currency = catalog.shop.currency;
    shop.ships = catalog.shop.ships;
    shop.pickup = catalog.shop.pickup || !catalog.shop.ships;
    shop.shipping_minor = catalog.shop.shipping_fee.map_or(0, |fee| to_minor(fee, &shop.currency));
    shop.shipping_policy = catalog.shop.shipping_policy;
    shop.returns_policy = catalog.shop.returns_policy;
    shop.warnings = catalog.warnings;

    // Ids come from position, so a duplicate delivery of this job overwrites instead of doubling.
    let products: Vec<Product> = catalog
        .products
        .into_iter()
        .enumerate()
        .map(|(position, product)| Product {
            id: format!("p{position:03}"),
            name: product.name,
            description: product.description,
            price_minor: product.price_minor,
            category: product.category,
            tags: product.tags,
            options: product.options,
            image_url: product.image_url,
            source_url: product.source_url,
            in_stock: product.in_stock,
            price_text: product.price_text,
            price_verified: product.price_verified,
            position: position as u32,
        })
        .collect();
    app.store.put_products(&shop.slug, &products).await?;

    shop.progress.iter_mut().for_each(|step| step.done = true);
    shop.status = ShopStatus::Review;
    app.store.put_shop(shop).await
}

/// Confirms control of a site by looking for a per-shop tag on its home page.
pub async fn check_ownership(app: &App, shop: &Shop) -> AppResult<bool> {
    let home = Url::parse(&shop.source_url)
        .and_then(|url| url.join("/"))
        .map_err(|_| user_error("That website address is not valid."))?;
    let page = app.fetcher.fetch(home.as_str()).await.map_err(|_| user_error("We couldn't open your website to check it."))?;
    let document = scraper::Html::parse_document(&page.body);
    let selector = scraper::Selector::parse(r#"meta[name="walk-in-verify"]"#).expect("static selector");
    Ok(document.select(&selector).any(|meta| meta.value().attr("content") == Some(shop.verify_token.as_str())))
}
