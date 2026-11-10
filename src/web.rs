use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::header::{CONTENT_TYPE, HeaderName};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{any, delete, get, post, put};
use axum::{Extension, Json, Router};
use rust_embed::{Embed, RustEmbed};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tower_http::set_header::SetResponseHeaderLayer;

use crate::App;
use crate::chat::{ChatMessage, run_chat};
use crate::commerce::{decline_quote, pay_quote, send_quote, sync_order, track_order};
use crate::error::{AppError, AppResult, user_error};
use crate::importer::{check_ownership, start_import};
use crate::model::{Channel, Order, OrderStatus, Product, Shop, ShopStatus, now};
use crate::money::{convert_minor, format_minor, is_supported_currency, to_minor};

#[derive(Embed)]
#[folder = "public/"]
struct Assets;

#[derive(Embed)]
#[folder = "demo-sites/"]
struct DemoSites;

const HOUR: i64 = 3600;
const MINUTE: i64 = 60;
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; img-src * data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'";

#[derive(Clone)]
struct ClientIp(String);

/// Uses the address Lambda observed, which a caller cannot forge the way it can a header.
async fn client_ip(mut request: Request, next: Next) -> Response {
    use lambda_http::request::RequestContext;
    let ip = match request.extensions().get::<RequestContext>() {
        Some(RequestContext::ApiGatewayV2(context)) => context.http.source_ip.clone(),
        _ => None,
    };
    request.extensions_mut().insert(ClientIp(ip.unwrap_or_else(|| "local".to_owned())));
    next.run(request).await
}

/// The public origin. Lambda Function URLs route by Host, so a forged Host can't reach this code.
fn origin(app: &App, headers: &HeaderMap) -> String {
    if let Some(base) = &app.config.public_base_url {
        return base.clone();
    }
    let host = headers.get("host").and_then(|value| value.to_str().ok()).unwrap_or("localhost");
    let forwarded = headers.get("x-forwarded-proto").and_then(|value| value.to_str().ok());
    let scheme = forwarded.unwrap_or(if host.starts_with("localhost") || host.starts_with("127.") { "http" } else { "https" });
    format!("{scheme}://{host}")
}

fn parse_json<T: DeserializeOwned>(bytes: &Bytes) -> AppResult<T> {
    serde_json::from_slice(bytes).map_err(|_| user_error("That request was not valid."))
}

async fn load_shop(app: &App, slug: &str) -> AppResult<Shop> {
    app.store.get_shop(slug).await?.ok_or_else(|| AppError::NotFound("Shop not found.".to_owned()))
}

async fn owner_shop(app: &App, slug: &str, headers: &HeaderMap) -> AppResult<Shop> {
    let shop = load_shop(app, slug).await?;
    let given = headers.get("x-owner-key").map_or(&b""[..], HeaderValue::as_bytes);
    if !bool::from(shop.owner_key.as_bytes().ct_eq(given)) {
        return Err(AppError::Forbidden);
    }
    Ok(shop)
}

async fn live_shop(app: &App, slug: &str) -> AppResult<Shop> {
    let shop = load_shop(app, slug).await?;
    if shop.status != ShopStatus::Live {
        return Err(AppError::NotFound("This shop is not open yet.".to_owned()));
    }
    Ok(shop)
}

fn product_view(shop: &Shop, product: &Product) -> Value {
    let mut view = json!(product);
    view["price"] = json!(format_minor(product.price_minor, &shop.currency));
    view
}

fn order_view(app: &App, order: &Order) -> Value {
    let money = |minor: i64| format_minor(minor, &order.currency);
    let mut view = json!(order);
    if let Some(capture_id) = &order.paypal_capture_id {
        view["paypalUrl"] = json!(format!("{}/unifiedtransactions/details/payment/{capture_id}", app.config.paypal_web_base()));
    }
    view["total"] = json!(money(order.total_minor));
    view["itemsTotal"] = json!(money(order.items_minor));
    view["shipping"] = json!(money(order.shipping_minor));
    view["items"] = order
        .items
        .iter()
        .map(|item| {
            let mut entry = json!(item);
            entry["unit"] = json!(money(item.unit_minor));
            entry
        })
        .collect();
    view
}

async fn owner_view(app: &App, shop: &Shop) -> AppResult<Value> {
    let products = app.store.list_products(&shop.slug).await?;
    let mut view = json!(shop);
    if let Some(object) = view.as_object_mut() {
        object.remove("ownerKey");
    }
    view["shipping"] = json!(format_minor(shop.shipping_minor, &shop.currency));
    view["products"] = products.iter().map(|product| product_view(shop, product)).collect();
    view["publishNeedsVerification"] = json!(!app.config.allow_unverified_publish);
    view["demoMerchantEmail"] = json!(app.config.demo_merchant_email);
    view["paypalReady"] = json!(app.config.paypal_configured());
    Ok(view)
}

fn bounded(value: &str, limit: usize, field: &str) -> AppResult<()> {
    if value.chars().count() > limit {
        return Err(user_error(format!("{field} is too long.")));
    }
    Ok(())
}

async fn healthz(State(app): State<App>) -> Json<Value> {
    Json(json!({ "ok": true, "paypal": app.config.paypal_configured(), "ai": app.llm.configured() }))
}

async fn samples(State(app): State<App>, headers: HeaderMap) -> Json<Value> {
    let base = origin(&app, &headers);
    let mut list: Vec<Value> = DemoSites::iter()
        .filter(|path| path.ends_with("/sample.json"))
        .filter_map(|path| {
            let file = DemoSites::get(&path)?;
            let mut meta: Value = serde_json::from_slice(&file.data).ok()?;
            let folder = path.trim_end_matches("/sample.json");
            meta["url"] = json!(format!("{base}/demo/{folder}/index.html"));
            Some(meta)
        })
        .collect();
    list.sort_by_key(|sample| sample["name"].as_str().unwrap_or_default().to_owned());
    Json(json!({ "samples": list }))
}

#[derive(Deserialize)]
struct ImportBody {
    url: String,
}

async fn create_shop(State(app): State<App>, Extension(ClientIp(ip)): Extension<ClientIp>, headers: HeaderMap, body: Bytes) -> AppResult<Response> {
    app.rate_limit("import", &ip, 8, 60, HOUR).await?;
    let body: ImportBody = parse_json(&body)?;
    bounded(&body.url, 2000, "That address")?;
    if body.url.trim().len() < 3 {
        return Err(user_error("Enter your website address."));
    }
    let shop = start_import(&app, &body.url, &origin(&app, &headers)).await?;
    Ok((StatusCode::CREATED, Json(json!({ "slug": shop.slug, "key": shop.owner_key }))).into_response())
}

async fn get_shop(State(app): State<App>, Path(slug): Path<String>, headers: HeaderMap) -> AppResult<Json<Value>> {
    let shop = owner_shop(&app, &slug, &headers).await?;
    Ok(Json(owner_view(&app, &shop).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShopPatch {
    name: Option<String>,
    description: Option<String>,
    location: Option<String>,
    contact: Option<String>,
    hours: Option<String>,
    currency: Option<String>,
    ships: Option<bool>,
    pickup: Option<bool>,
    shipping: Option<f64>,
    shipping_policy: Option<String>,
    returns_policy: Option<String>,
    paypal_email: Option<String>,
}

fn is_plausible_email(email: &str) -> bool {
    email.len() <= 254 && !email.contains(char::is_whitespace) && email.split_once('@').is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.'))
}

async fn patch_shop(State(app): State<App>, Path(slug): Path<String>, headers: HeaderMap, body: Bytes) -> AppResult<Json<Value>> {
    let mut shop = owner_shop(&app, &slug, &headers).await?;
    let patch: ShopPatch = parse_json(&body)?;

    fn apply(target: &mut String, value: Option<String>, limit: usize, field: &str) -> AppResult<()> {
        if let Some(value) = value {
            bounded(&value, limit, field)?;
            *target = value;
        }
        Ok(())
    }
    if patch.name.as_deref().is_some_and(|name| name.trim().is_empty()) {
        return Err(user_error("Your shop needs a name."));
    }
    apply(&mut shop.name, patch.name, 120, "The name")?;
    apply(&mut shop.description, patch.description, 2000, "The description")?;
    apply(&mut shop.location, patch.location, 300, "The location")?;
    apply(&mut shop.contact, patch.contact, 300, "The contact")?;
    apply(&mut shop.hours, patch.hours, 500, "The hours")?;
    apply(&mut shop.shipping_policy, patch.shipping_policy, 2000, "The shipping policy")?;
    apply(&mut shop.returns_policy, patch.returns_policy, 2000, "The returns policy")?;
    shop.ships = patch.ships.unwrap_or(shop.ships);
    shop.pickup = patch.pickup.unwrap_or(shop.pickup);

    if let Some(email) = patch.paypal_email {
        let email = email.trim().to_owned();
        if !email.is_empty() && !is_plausible_email(&email) {
            return Err(user_error("Enter a valid PayPal email address."));
        }
        shop.paypal_email = email;
    }
    if let Some(currency) = patch.currency.filter(|currency| *currency != shop.currency) {
        if !is_supported_currency(&currency) {
            return Err(user_error("PayPal does not support that currency."));
        }
        let mut products = app.store.list_products(&slug).await?;
        for product in &mut products {
            product.price_minor = convert_minor(product.price_minor, &shop.currency, &currency);
        }
        app.store.put_products(&slug, &products).await?;
        shop.shipping_minor = convert_minor(shop.shipping_minor, &shop.currency, &currency);
        shop.currency = currency;
    }
    if let Some(shipping) = patch.shipping {
        if !shipping.is_finite() || !(0.0..=10_000.0).contains(&shipping) {
            return Err(user_error("Enter a valid shipping fee."));
        }
        shop.shipping_minor = to_minor(shipping, &shop.currency);
    }
    app.store.put_shop(&shop).await?;
    Ok(Json(owner_view(&app, &shop).await?))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProductBody {
    name: String,
    #[serde(default)]
    description: String,
    /// Empty means the item is sold by quote.
    price: Option<f64>,
    #[serde(default)]
    category: String,
    #[serde(default = "yes")]
    in_stock: bool,
}

fn yes() -> bool {
    true
}

impl ProductBody {
    fn validate(&self) -> AppResult<()> {
        if self.name.trim().is_empty() {
            return Err(user_error("A product needs a name."));
        }
        bounded(&self.name, 200, "The product name")?;
        bounded(&self.description, 2000, "The description")?;
        bounded(&self.category, 100, "The category")?;
        if self.price.is_some_and(|price| !price.is_finite() || price <= 0.0 || price > 1_000_000.0) {
            return Err(user_error("Enter a price greater than zero."));
        }
        Ok(())
    }
}

async fn add_product(State(app): State<App>, Path(slug): Path<String>, headers: HeaderMap, body: Bytes) -> AppResult<Response> {
    let shop = owner_shop(&app, &slug, &headers).await?;
    let body: ProductBody = parse_json(&body)?;
    body.validate()?;
    let existing = app.store.list_products(&slug).await?;
    let position = existing.iter().map(|product| product.position + 1).max().unwrap_or(0);
    let product = Product {
        id: format!("m{position:03}"),
        name: body.name.trim().to_owned(),
        description: body.description,
        price_minor: body.price.map_or(0, |price| to_minor(price, &shop.currency)),
        category: body.category,
        tags: vec![],
        options: vec![],
        image_url: None,
        source_url: shop.source_url.clone(),
        in_stock: body.in_stock,
        price_text: String::new(),
        price_verified: true,
        position,
        by_quote: body.price.is_none(),
    };
    app.store.put_product(&slug, &product).await?;
    Ok((StatusCode::CREATED, Json(product_view(&shop, &product))).into_response())
}

async fn update_product(State(app): State<App>, Path((slug, id)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> AppResult<Json<Value>> {
    let shop = owner_shop(&app, &slug, &headers).await?;
    let Some(mut product) = app.store.get_product(&slug, &id).await? else {
        return Err(AppError::NotFound("Product not found.".to_owned()));
    };
    let body: ProductBody = parse_json(&body)?;
    body.validate()?;
    product.name = body.name.trim().to_owned();
    product.description = body.description;
    product.price_minor = body.price.map_or(0, |price| to_minor(price, &shop.currency));
    product.by_quote = body.price.is_none();
    product.category = body.category;
    product.in_stock = body.in_stock;
    // Saving a product is the owner confirming its price.
    product.price_verified = true;
    app.store.put_product(&slug, &product).await?;
    Ok(Json(product_view(&shop, &product)))
}

async fn remove_product(State(app): State<App>, Path((slug, id)): Path<(String, String)>, headers: HeaderMap) -> AppResult<Json<Value>> {
    owner_shop(&app, &slug, &headers).await?;
    app.store.delete_product(&slug, &id).await?;
    Ok(Json(json!({ "deleted": true })))
}

async fn verify_shop(State(app): State<App>, Path(slug): Path<String>, headers: HeaderMap) -> AppResult<Json<Value>> {
    let mut shop = owner_shop(&app, &slug, &headers).await?;
    if !shop.verified {
        shop.verified = check_ownership(&app, &shop).await?;
        app.store.put_shop(&shop).await?;
    }
    Ok(Json(json!({ "verified": shop.verified })))
}

async fn publish_shop(State(app): State<App>, Path(slug): Path<String>, headers: HeaderMap) -> AppResult<Json<Value>> {
    let mut shop = owner_shop(&app, &slug, &headers).await?;
    let products = app.store.list_products(&slug).await?;
    if products.is_empty() {
        return Err(user_error("Add at least one product before opening."));
    }
    let unconfirmed = products.iter().filter(|product| !product.price_verified).count();
    if unconfirmed > 0 {
        return Err(user_error(format!("Confirm {unconfirmed} flagged {} before opening.", if unconfirmed == 1 { "price" } else { "prices" })));
    }
    if shop.paypal_email.is_empty() {
        return Err(user_error("Connect a PayPal account before opening."));
    }
    if !shop.ships && !shop.pickup {
        return Err(user_error("Offer shipping, pickup, or both."));
    }
    if !shop.verified && !app.config.allow_unverified_publish {
        return Err(user_error("Prove you own this website before opening."));
    }
    shop.status = ShopStatus::Live;
    shop.published_at.get_or_insert_with(now);
    app.store.put_shop(&shop).await?;
    Ok(Json(json!({ "status": shop.status })))
}

async fn dashboard(State(app): State<App>, Path(slug): Path<String>, headers: HeaderMap) -> AppResult<Json<Value>> {
    let shop = owner_shop(&app, &slug, &headers).await?;
    let (orders, events, unmet, visits, view) = tokio::try_join!(
        async { app.store.list_orders(&slug).await.map_err(AppError::from) },
        async { app.store.list_events(&slug, 40).await.map_err(AppError::from) },
        async { app.store.list_unmet_needs(&slug).await.map_err(AppError::from) },
        async { app.store.visits(&slug).await.map_err(AppError::from) },
        owner_view(&app, &shop),
    )?;
    let base = origin(&app, &headers);
    let mcp_url = format!("{base}/mcp/{slug}");
    let paid: Vec<&Order> = orders.iter().filter(|order| order.status == OrderStatus::Paid).collect();
    let revenue: i64 = paid.iter().map(|order| order.total_minor).sum();
    Ok(Json(json!({
        "shop": view,
        "connection": {
            "mcpUrl": mcp_url,
            "shopperUrl": format!("{base}/shop/{slug}"),
            "claudeCode": format!("claude mcp add --transport http {slug} {mcp_url}"),
        },
        "stats": { "visits": visits, "orders": paid.len(), "revenue": format_minor(revenue, &shop.currency) },
        "orders": orders.iter().map(|order| order_view(&app, order)).collect::<Vec<_>>(),
        "events": events,
        "unmet": unmet,
    })))
}

async fn owner_order(app: &App, slug: &str, id: &str, headers: &HeaderMap) -> AppResult<(Shop, Order)> {
    let shop = owner_shop(app, slug, headers).await?;
    let order = app.store.get_order(slug, id).await?.ok_or_else(|| AppError::NotFound("Order not found.".to_owned()))?;
    Ok((shop, order))
}

#[derive(Deserialize)]
struct TrackBody {
    carrier: String,
    number: String,
}

async fn track(State(app): State<App>, Path((slug, id)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> AppResult<Json<Value>> {
    let (shop, order) = owner_order(&app, &slug, &id, &headers).await?;
    let body: TrackBody = parse_json(&body)?;
    if body.carrier.trim().is_empty() || body.number.trim().is_empty() {
        return Err(user_error("Enter the carrier and tracking number."));
    }
    bounded(&body.carrier, 60, "The carrier")?;
    bounded(&body.number, 64, "The tracking number")?;
    Ok(Json(order_view(&app, &track_order(&app, &shop, order, body.carrier.trim(), body.number.trim()).await?)))
}

#[derive(Deserialize)]
struct QuoteBody {
    price: f64,
    #[serde(default)]
    note: String,
}

async fn quote_order(State(app): State<App>, Path((slug, id)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> AppResult<Json<Value>> {
    let (shop, order) = owner_order(&app, &slug, &id, &headers).await?;
    let body: QuoteBody = parse_json(&body)?;
    bounded(&body.note, 500, "The message")?;
    Ok(Json(order_view(&app, &send_quote(&app, &shop, order, body.price, &body.note).await?)))
}

#[derive(Deserialize)]
struct DeclineBody {
    #[serde(default)]
    note: String,
}

async fn decline_order(State(app): State<App>, Path((slug, id)): Path<(String, String)>, headers: HeaderMap, body: Bytes) -> AppResult<Json<Value>> {
    let (shop, order) = owner_order(&app, &slug, &id, &headers).await?;
    let body: DeclineBody = parse_json(&body)?;
    bounded(&body.note, 500, "The message")?;
    Ok(Json(order_view(&app, &decline_quote(&app, &shop, order, &body.note).await?)))
}

/// Finds a quote for a shopper. Ordinary orders stay out of reach of this public route.
async fn shopper_quote(app: &App, slug: &str, id: &str) -> AppResult<(Shop, Order)> {
    let shop = live_shop(app, slug).await?;
    let order = app.store.get_order(slug, id).await?.filter(Order::is_quote).ok_or_else(|| AppError::NotFound("We can't find that request.".to_owned()))?;
    Ok((shop, order))
}

async fn quote_status(State(app): State<App>, Extension(ClientIp(ip)): Extension<ClientIp>, Path((slug, id)): Path<(String, String)>) -> AppResult<Json<Value>> {
    app.rate_limit("quote", &ip, 240, 2000, MINUTE).await?;
    let (shop, order) = shopper_quote(&app, &slug, &id).await?;
    let order = match sync_order(&app, &shop, order.clone()).await {
        Ok(current) => current,
        Err(error) => {
            tracing::error!(?error, order = %order.id, "could not refresh quote");
            order
        }
    };
    let priced = !matches!(order.status, OrderStatus::AwaitingQuote | OrderStatus::Declined);
    Ok(Json(json!({
        "shop": shop.name,
        "reference": order.id,
        "status": order.status,
        "request": order.request,
        "total": priced.then(|| format_minor(order.total_minor, &order.currency)),
        "currency": order.currency,
        "note": order.quote_note,
    })))
}

async fn quote_pay(State(app): State<App>, Extension(ClientIp(ip)): Extension<ClientIp>, Path((slug, id)): Path<(String, String)>, headers: HeaderMap) -> AppResult<Json<Value>> {
    app.rate_limit("quote-pay", &ip, 30, 300, HOUR).await?;
    let (shop, order) = shopper_quote(&app, &slug, &id).await?;
    let approval_url = pay_quote(&app, &shop, order, &origin(&app, &headers)).await?;
    Ok(Json(json!({ "approvalUrl": approval_url })))
}

async fn public_shop(State(app): State<App>, Path(slug): Path<String>) -> AppResult<Json<Value>> {
    let shop = live_shop(&app, &slug).await?;
    Ok(Json(json!({ "name": shop.name, "description": shop.description, "verified": shop.verified, "website": shop.source_url })))
}

#[derive(Deserialize)]
struct ChatBody {
    messages: Vec<ChatMessage>,
}

async fn chat(State(app): State<App>, Extension(ClientIp(ip)): Extension<ClientIp>, Path(slug): Path<String>, headers: HeaderMap, body: Bytes) -> AppResult<Json<Value>> {
    app.rate_limit("chat", &ip, 40, 600, HOUR).await?;
    let shop = live_shop(&app, &slug).await?;
    let body: ChatBody = parse_json(&body)?;
    if body.messages.is_empty() || body.messages.len() > 30 {
        return Err(user_error("That conversation is too long. Please start a new one."));
    }
    for message in &body.messages {
        bounded(&message.text, 2000, "That message")?;
    }
    let reply = run_chat(&app, &shop, body.messages, &origin(&app, &headers)).await?;
    Ok(Json(json!(reply)))
}

async fn paypal_webhook(State(app): State<App>, headers: HeaderMap, body: String) -> AppResult<Response> {
    if !app.paypal.verify_webhook(&headers, &body).await? {
        return Ok((StatusCode::BAD_REQUEST, Json(json!({ "error": "Invalid signature." }))).into_response());
    }
    let event: Value = serde_json::from_str(&body).map_err(|_| user_error("Invalid event."))?;
    let resource = &event["resource"];
    let custom_id = resource["custom_id"].as_str().or(resource["purchase_units"][0]["custom_id"].as_str());
    let received = Json(json!({ "received": true })).into_response();
    let Some((slug, reference)) = custom_id.and_then(|id| id.split_once('|')) else { return Ok(received) };
    let (Some(shop), Some(order)) = (app.store.get_shop(slug).await?, app.store.get_order(slug, reference).await?) else {
        return Ok(received);
    };
    match event["event_type"].as_str() {
        Some("CHECKOUT.ORDER.APPROVED" | "PAYMENT.CAPTURE.COMPLETED") => {
            sync_order(&app, &shop, order).await?;
        }
        Some("PAYMENT.CAPTURE.REFUNDED") if order.status == OrderStatus::Paid => {
            // Owners refund in PayPal, where a refund can cover part of the order.
            let refunded = resource["seller_payable_breakdown"]["total_refunded_amount"]["value"].as_str().and_then(|value| value.parse::<f64>().ok());
            if refunded.is_none_or(|value| to_minor(value, &order.currency) >= order.total_minor) {
                app.store.set_order_status(&order, OrderStatus::Refunded).await?;
                app.store.record_event(slug, "refund", &format!("Order {} was refunded in PayPal", order.id), Channel::Owner).await?;
            } else {
                app.store.record_event(slug, "refund", &format!("Order {} was partly refunded in PayPal", order.id), Channel::Owner).await?;
            }
        }
        _ => {}
    }
    Ok(received)
}

async fn mcp(State(app): State<App>, Extension(ClientIp(ip)): Extension<ClientIp>, Path(slug): Path<String>, request: Request) -> AppResult<Response> {
    app.rate_limit("mcp", &ip, 240, 2000, MINUTE).await?;
    let shop = live_shop(&app, &slug).await?;
    let base = origin(&app, request.headers());
    Ok(crate::mcp::handle(app, shop, base, request).await)
}

fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn result_page(title: &str, inner: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} · Walk-In</title>
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="stylesheet" href="https://fonts.googleapis.com/css2?family=Fraunces:opsz,wght@9..144,500;9..144,700&family=Instrument+Sans:wght@400;500;600&display=swap">
<link rel="stylesheet" href="/assets/app.css">
</head>
<body class="result-page"><main class="result-card">{inner}</main></body>
</html>"#,
        title = escape_html(title),
    ))
}

fn order_summary(order: &Order) -> String {
    let money = |minor: i64| format_minor(minor, &order.currency);
    let mut rows: String = order
        .items
        .iter()
        .map(|item| {
            let options: Vec<&str> = item.options.iter().map(|(_, value)| value.as_str()).collect();
            let detail = if options.is_empty() { String::new() } else { format!(" <small>{}</small>", escape_html(&options.join(", "))) };
            format!("<li><span>{} × {}{detail}</span><span>{}</span></li>", item.quantity, escape_html(&item.name), money(item.unit_minor * i64::from(item.quantity)))
        })
        .collect();
    if order.shipping_minor > 0 {
        rows.push_str(&format!("<li><span>Shipping</span><span>{}</span></li>", money(order.shipping_minor)));
    }
    format!(r#"<ul class="receipt">{rows}<li class="receipt-total"><span>Total</span><span>{} {}</span></li></ul>"#, money(order.total_minor), escape_html(&order.currency))
}

#[derive(Deserialize)]
struct CheckoutQuery {
    #[serde(default)]
    shop: String,
    #[serde(default, rename = "ref")]
    reference: String,
}

async fn find_order(app: &App, query: &CheckoutQuery) -> AppResult<Option<(Shop, Order)>> {
    let (Some(shop), Some(order)) = (app.store.get_shop(&query.shop).await?, app.store.get_order(&query.shop, &query.reference).await?) else {
        return Ok(None);
    };
    Ok(Some((shop, order)))
}

async fn checkout_return(State(app): State<App>, Query(query): Query<CheckoutQuery>) -> AppResult<Response> {
    let Some((shop, order)) = find_order(&app, &query).await? else {
        return Ok((StatusCode::NOT_FOUND, result_page("Order not found", "<h1>We can't find that order</h1><p class=\"muted\">Check the link from your assistant.</p>")).into_response());
    };
    let (current, failed) = match sync_order(&app, &shop, order.clone()).await {
        Ok(current) => (current, false),
        Err(error) => {
            tracing::error!(?error, order = %order.id, "could not confirm order");
            (order, true)
        }
    };
    let id = escape_html(&current.id);
    if current.status == OrderStatus::Paid {
        let name = escape_html(&shop.name);
        let inner = format!(
            r#"<p class="eyebrow">Order {id}</p><h1>Thank you. {name} has your order.</h1>{}<p class="muted">Your payment went directly to the shop through PayPal. You can close this tab and go back to your assistant.</p>"#,
            order_summary(&current)
        );
        return Ok(result_page("Order paid", &inner).into_response());
    }
    let heading = if failed { "We couldn't confirm that payment yet" } else { "This payment isn't finished" };
    let retry = current.approval_url.as_deref().filter(|_| current.status == OrderStatus::AwaitingPayment).map_or_else(String::new, |url| format!(r#"<a class="button button-paypal" href="{}">Return to PayPal</a>"#, escape_html(url)));
    let inner = format!(
        r#"<p class="eyebrow">Order {id}</p><h1>{heading}</h1><p class="muted">If PayPal showed a confirmation, your assistant can check this order again in a moment.</p>{}{retry}"#,
        order_summary(&current)
    );
    Ok(result_page("Payment not finished", &inner).into_response())
}

async fn checkout_cancel(State(app): State<App>, Query(query): Query<CheckoutQuery>) -> AppResult<Response> {
    let again = find_order(&app, &query)
        .await?
        .filter(|(_, order)| order.status == OrderStatus::AwaitingPayment)
        .and_then(|(_, order)| order.approval_url)
        .map_or_else(String::new, |url| format!(r#"<a class="button button-paypal" href="{}">Go back to PayPal</a>"#, escape_html(&url)));
    let inner = format!(r#"<h1>Nothing was charged</h1><p class="muted">You left PayPal before approving this order. You can close this tab.</p>{again}"#);
    Ok(result_page("Checkout cancelled", &inner).into_response())
}

fn serve_embedded<E: RustEmbed>(path: &str) -> Response {
    match E::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            ([(CONTENT_TYPE, mime.as_ref().to_owned())], file.data).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn asset(Path(path): Path<String>) -> Response {
    serve_embedded::<Assets>(&path)
}

async fn demo(Path(path): Path<String>) -> Response {
    serve_embedded::<DemoSites>(&path)
}

async fn demo_robots() -> &'static str {
    "User-agent: *\nAllow: /\n"
}

fn page(file: &'static str) -> axum::routing::MethodRouter<App> {
    get(move || async move { serve_embedded::<Assets>(file) })
}

pub fn router(app: App) -> Router {
    let header = |name: &'static str, value: &'static str| {
        SetResponseHeaderLayer::overriding(HeaderName::from_static(name), HeaderValue::from_static(value))
    };
    let browser = Router::new()
        .route("/", page("index.html"))
        .route("/setup/{slug}", page("setup.html"))
        .route("/dashboard/{slug}", page("dashboard.html"))
        .route("/shop/{slug}", page("shop.html"))
        .route("/quote/{slug}/{id}", page("quote.html"))
        .route("/assets/{*path}", get(asset))
        .route("/checkout/return", get(checkout_return))
        .route("/checkout/cancel", get(checkout_cancel))
        .route("/api/samples", get(samples))
        .route("/api/shops", post(create_shop))
        .route("/api/shops/{slug}", get(get_shop).patch(patch_shop))
        .route("/api/shops/{slug}/products", post(add_product))
        .route("/api/shops/{slug}/products/{id}", put(update_product))
        .route("/api/shops/{slug}/products/{id}", delete(remove_product))
        .route("/api/shops/{slug}/verify", post(verify_shop))
        .route("/api/shops/{slug}/publish", post(publish_shop))
        .route("/api/shops/{slug}/dashboard", get(dashboard))
        .route("/api/shops/{slug}/orders/{id}/track", post(track))
        .route("/api/shops/{slug}/orders/{id}/quote", post(quote_order))
        .route("/api/shops/{slug}/orders/{id}/decline", post(decline_order))
        .route("/api/shops/{slug}/quotes/{id}", get(quote_status))
        .route("/api/shops/{slug}/quotes/{id}/pay", post(quote_pay))
        .route("/api/shops/{slug}/public", get(public_shop))
        .route("/api/shops/{slug}/chat", post(chat))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .layer(header("content-security-policy", CSP))
        .layer(header("x-content-type-options", "nosniff"))
        .layer(header("referrer-policy", "no-referrer"));

    Router::new()
        .merge(browser)
        .route("/healthz", get(healthz))
        // Assistants are not browsers, and the sample shops stand in for third-party sites,
        // so neither gets the browser policy above.
        .route("/mcp/{slug}", any(mcp))
        .route("/api/webhooks/paypal", post(paypal_webhook))
        .route("/demo/{*path}", get(demo))
        .route("/robots.txt", get(demo_robots))
        .layer(middleware::from_fn(client_ip))
        .with_state(app)
}
