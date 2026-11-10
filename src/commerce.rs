//! What shoppers can do in a shop. MCP and the hosted chat both come through here.

use std::collections::HashSet;

use rand::Rng;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::App;
use crate::error::{AppResult, user_error};
use crate::model::{Channel, Fulfillment, Order, OrderItem, OrderStatus, Product, Shop, ShopStatus, Tracking, now};
use crate::money::{display_money, format_minor, to_minor};
use crate::paypal::{CheckoutRequest, CreatedOrder, PaymentResult};

const MAX_QUANTITY: u32 = 99;
const MAX_LINES: usize = 20;
const MAX_REQUEST_CHARS: usize = 1000;
const MAX_PRICE: f64 = 1_000_000.0;
const STOP_WORDS: &[&str] = &[
    "a", "an", "and", "any", "are", "buy", "can", "do", "does", "find", "for", "from", "get", "have", "i", "in", "is",
    "it", "looking", "me", "my", "need", "of", "on", "or", "please", "show", "some", "something", "that", "the", "to",
    "under", "want", "with", "you", "your",
];

fn stem(token: &str) -> String {
    let count = token.chars().count();
    if count > 4 && token.ends_with("ies") {
        format!("{}y", &token[..token.len() - 3])
    } else if count > 3 && token.ends_with('s') && !token.ends_with("ss") {
        token[..token.len() - 1].to_owned()
    } else {
        token.to_owned()
    }
}

pub fn tokenize(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|token| token.chars().count() > 1 && !STOP_WORDS.contains(token))
        .map(stem)
        .collect()
}

fn token_set<'a>(texts: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
    texts.into_iter().flat_map(tokenize).collect()
}

/// Returns (score, number of query terms the product matched).
fn score_product(product: &Product, tokens: &[String]) -> (u32, usize) {
    let name = token_set([product.name.as_str()]);
    let tags = token_set(product.tags.iter().map(String::as_str));
    let category = token_set([product.category.as_str()]);
    let description = token_set([product.description.as_str()]);
    let mut score = 0;
    let mut matched = 0;
    for token in tokens {
        let points = u32::from(name.contains(token)) * 3
            + u32::from(tags.contains(token)) * 3
            + u32::from(category.contains(token)) * 2
            + u32::from(description.contains(token));
        matched += usize::from(points > 0);
        score += points;
    }
    (score, matched)
}

pub fn agent_product(shop: &Shop, product: &Product) -> Value {
    json!({
        "id": product.id,
        "name": product.name,
        "description": product.description,
        "price": (!product.by_quote).then(|| format_minor(product.price_minor, &shop.currency)),
        "pricing": if product.by_quote { "quote" } else { "fixed" },
        "currency": shop.currency,
        "category": product.category,
        "tags": product.tags,
        "options": product.options,
        "in_stock": product.in_stock,
        "image_url": product.image_url,
        "url": product.source_url,
    })
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Keywords describing what the shopper wants
    pub query: Option<String>,
    /// Highest acceptable unit price, in the shop currency
    pub max_price: Option<f64>,
    /// Maximum results, default 10
    pub limit: Option<u32>,
}

async fn note_unmet_need(app: &App, shop: &Shop, need: &str, tokens: &[String]) -> AppResult<()> {
    let mut sorted = tokens.to_vec();
    sorted.sort();
    let normalized = if sorted.is_empty() { need.to_lowercase() } else { sorted.join(" ") };
    app.store.record_unmet_need(&shop.slug, need, &normalized).await?;
    Ok(())
}

pub async fn search_products(app: &App, shop: &Shop, args: SearchArgs, channel: Channel) -> AppResult<Value> {
    let query = args.query.unwrap_or_default();
    let query = query.trim();
    let tokens = tokenize(query);
    let max_minor = args.max_price.map(|price| to_minor(price, &shop.currency));
    let limit = args.limit.unwrap_or(10).clamp(1, 30) as usize;

    let products = app.store.list_products(&shop.slug).await?;
    let mut scored: Vec<(u32, usize, &Product)> = products
        .iter()
        .filter(|product| product.in_stock && (product.by_quote || max_minor.is_none_or(|max| product.price_minor <= max)))
        .map(|product| {
            let (score, matched) = score_product(product, &tokens);
            (score, matched, product)
        })
        .filter(|(_, matched, _)| tokens.is_empty() || *matched > 0)
        .collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)).then(a.2.position.cmp(&b.2.position)));

    let label = if query.is_empty() { "everything" } else { query };
    let described = max_minor.map_or_else(|| label.to_owned(), |max| format!("{label} under {}", display_money(max, &shop.currency)));
    app.store.add_visit(&shop.slug).await?;
    app.store.record_event(&shop.slug, "search", &format!("Searched for \"{described}\""), channel).await?;

    let best = scored.first().map_or(0, |entry| entry.1);
    let mut view = json!({
        "shop": shop.name,
        "match": if scored.is_empty() { "none" } else if tokens.is_empty() || best == tokens.len() { "all_terms" } else { "some_terms" },
        "total_found": scored.len(),
        "results": scored.iter().take(limit).map(|(_, _, product)| agent_product(shop, product)).collect::<Vec<_>>(),
    });
    // Assistants rephrase a failed search several times, so only an explicit report counts as one ask.
    if scored.is_empty() || view["match"] == "some_terms" {
        view["next_step"] = json!("If this shop doesn't sell what the shopper asked for, tell them and call report_unmet_need once, without asking first.");
    } else if scored.iter().take(limit).any(|(_, _, product)| product.by_quote) {
        view["next_step"] = json!("Results with pricing \"quote\" have no fixed price, so never state one. Collect the details the shop needs and call request_quote.");
    }
    Ok(view)
}

pub async fn report_unmet_need(app: &App, shop: &Shop, need: &str, channel: Channel) -> AppResult<Value> {
    let trimmed: String = need.trim().chars().take(200).collect();
    if trimmed.is_empty() {
        return Err(user_error("Describe what the shopper wanted."));
    }
    note_unmet_need(app, shop, &trimmed, &tokenize(&trimmed)).await?;
    app.store.record_event(&shop.slug, "unmet", &format!("Asked for \"{trimmed}\", which isn't stocked"), channel).await?;
    Ok(json!({ "recorded": true }))
}

pub async fn product_details(app: &App, shop: &Shop, product_id: &str, channel: Channel) -> AppResult<Value> {
    let Some(product) = app.store.get_product(&shop.slug, product_id).await? else {
        return Err(user_error(format!("No product with id \"{product_id}\". Use search_products to find valid ids.")));
    };
    app.store.add_visit(&shop.slug).await?;
    app.store.record_event(&shop.slug, "view", &format!("Looked at {}", product.name), channel).await?;
    Ok(agent_product(shop, &product))
}

pub async fn shop_info(app: &App, shop: &Shop, channel: Channel) -> AppResult<Value> {
    let products = app.store.list_products(&shop.slug).await?;
    let mut categories: Vec<&str> = products.iter().map(|product| product.category.as_str()).filter(|c| !c.is_empty()).collect();
    categories.sort_unstable();
    categories.dedup();
    app.store.add_visit(&shop.slug).await?;
    app.store.record_event(&shop.slug, "visit", "Opened the shop", channel).await?;
    Ok(json!({
        "name": shop.name,
        "description": shop.description,
        "website": shop.source_url,
        "location": shop.location,
        "hours": shop.hours,
        "contact": shop.contact,
        "currency": shop.currency,
        "ownership_verified": shop.verified,
        "categories": categories,
        "product_count": products.len(),
        "fulfillment": {
            "ships": shop.ships,
            "pickup": shop.pickup,
            "shipping_fee": shop.ships.then(|| format_minor(shop.shipping_minor, &shop.currency)),
        },
        "shipping_policy": shop.shipping_policy,
        "returns_policy": shop.returns_policy,
        "payment": "PayPal. The shopper approves each order in PayPal, and funds go to the shop.",
        "custom_requests": "For custom work or items with no fixed price, use request_quote. The shop replies with a price, and the shopper pays it through PayPal.",
    }))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct OptionChoice {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckoutLine {
    pub product_id: String,
    pub quantity: u32,
    /// Required when the product has options
    pub options: Option<Vec<OptionChoice>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckoutArgs {
    pub items: Vec<CheckoutLine>,
    /// How the shopper receives the order
    pub fulfillment: Option<Fulfillment>,
    /// Message for the shop, such as cake writing
    pub note: Option<String>,
}

fn same(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

fn resolve_options(product: &Product, provided: &[OptionChoice]) -> AppResult<Vec<(String, String)>> {
    product
        .options
        .iter()
        .map(|option| {
            let available = option.values.join(", ");
            let Some(choice) = provided.iter().find(|entry| same(&entry.name, &option.name)) else {
                return Err(user_error(format!("Choose a {} for {}. Available: {available}.", option.name, product.name)));
            };
            let Some(value) = option.values.iter().find(|value| same(value, &choice.value)) else {
                return Err(user_error(format!("\"{}\" is not available for {}. Available {}: {available}.", choice.value, product.name, option.name)));
            };
            Ok((option.name.clone(), value.clone()))
        })
        .collect()
}

fn choose_fulfillment(shop: &Shop, requested: Option<Fulfillment>) -> AppResult<Fulfillment> {
    match requested {
        Some(Fulfillment::Ship) if !shop.ships => Err(user_error(format!("{} does not ship. Use pickup.", shop.name))),
        Some(Fulfillment::Pickup) if !shop.pickup => Err(user_error(format!("{} does not offer pickup. Use ship.", shop.name))),
        Some(choice) => Ok(choice),
        None => Ok(if shop.ships { Fulfillment::Ship } else { Fulfillment::Pickup }),
    }
}

fn new_reference() -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::rng();
    let code: String = (0..8).map(|_| ALPHABET[rng.random_range(0..ALPHABET.len())] as char).collect();
    format!("WI-{code}")
}

pub fn agent_order(shop: &Shop, order: &Order, base_url: &str) -> Value {
    let priced = !matches!(order.status, OrderStatus::AwaitingQuote | OrderStatus::Declined);
    let money = |minor: i64| priced.then(|| format_minor(minor, &order.currency));
    let mut view = json!({
        "order_reference": order.id,
        "status": order.status,
        "shop": shop.name,
        "items": order.items.iter().map(|item| json!({
            "name": item.name,
            "options": item.options,
            "quantity": item.quantity,
            "unit_price": money(item.unit_minor),
        })).collect::<Vec<_>>(),
        "shipping": money(order.shipping_minor),
        "total": money(order.total_minor),
        "currency": order.currency,
        "fulfillment": order.fulfillment,
        "tracking": order.tracking,
    });
    if order.is_quote() {
        view["request"] = json!(order.request);
        view["message_from_shop"] = json!(order.quote_note);
        view["status_url"] = json!(format!("{base_url}/quote/{}/{}", shop.slug, order.id));
        let step = match order.status {
            OrderStatus::AwaitingQuote => Some("The shop hasn't set a price yet. Give the shopper status_url so they can follow this request."),
            OrderStatus::Quoted | OrderStatus::AwaitingPayment => Some("The shop has set a price. Tell the shopper the total and give them status_url, where they can pay with PayPal. Nothing is charged until they approve it there."),
            OrderStatus::Declined => Some("The shop can't take this request. Pass on message_from_shop if there is one."),
            _ => None,
        };
        if let Some(step) = step {
            view["next_step"] = json!(step);
        }
    } else if order.status == OrderStatus::AwaitingPayment {
        view["approval_url"] = json!(order.approval_url);
    } else if order.status == OrderStatus::Cancelled {
        view["next_step"] = json!("This checkout expired before it was approved. Create a new one if the shopper still wants it.");
    }
    view
}

fn ensure_open(shop: &Shop) -> AppResult<()> {
    if shop.status != ShopStatus::Live {
        return Err(user_error(format!("{} is not open for orders yet.", shop.name)));
    }
    if shop.paypal_email.is_empty() {
        return Err(user_error(format!("{} has not connected PayPal yet.", shop.name)));
    }
    Ok(())
}

async fn open_checkout(app: &App, shop: &Shop, order: &Order, idempotency_key: String, base_url: &str) -> AppResult<CreatedOrder> {
    let query = format!("shop={}&ref={}", shop.slug, order.id);
    app.paypal
        .create_order(CheckoutRequest {
            reference: &order.id,
            idempotency_key,
            custom_id: &format!("{}|{}", shop.slug, order.id),
            brand_name: &shop.name,
            payee_email: &shop.paypal_email,
            order,
            fulfillment: order.fulfillment,
            return_url: &format!("{base_url}/checkout/return?{query}"),
            cancel_url: &format!("{base_url}/checkout/cancel?{query}"),
        })
        .await
        .map_err(|error| {
            tracing::error!(?error, order = %order.id, "paypal checkout failed");
            user_error("PayPal could not start this checkout. Please try again shortly.")
        })
}

/// Builds an order from the shop's own catalog. Prices never come from the caller,
/// so an assistant cannot talk the shop into a different total.
pub async fn create_checkout(app: &App, shop: &Shop, args: CheckoutArgs, channel: Channel, base_url: &str) -> AppResult<Value> {
    ensure_open(shop)?;
    if args.items.is_empty() {
        return Err(user_error("Add at least one item."));
    }
    if args.items.len() > MAX_LINES {
        return Err(user_error(format!("An order can have at most {MAX_LINES} lines.")));
    }

    let mut items = Vec::with_capacity(args.items.len());
    for line in &args.items {
        let Some(product) = app.store.get_product(&shop.slug, &line.product_id).await? else {
            return Err(user_error(format!("No product with id \"{}\". Use search_products to find valid ids.", line.product_id)));
        };
        if !product.in_stock {
            return Err(user_error(format!("{} is sold out.", product.name)));
        }
        if product.by_quote {
            return Err(user_error(format!("{} has no fixed price. Use request_quote to ask the shop for one.", product.name)));
        }
        if line.quantity < 1 || line.quantity > MAX_QUANTITY {
            return Err(user_error(format!("Quantity for {} must be between 1 and {MAX_QUANTITY}.", product.name)));
        }
        items.push(OrderItem {
            options: resolve_options(&product, line.options.as_deref().unwrap_or_default())?,
            product_id: product.id,
            name: product.name,
            quantity: line.quantity,
            unit_minor: product.price_minor,
        });
    }

    let fulfillment = choose_fulfillment(shop, args.fulfillment)?;
    let items_minor: i64 = items.iter().map(|item| item.unit_minor * i64::from(item.quantity)).sum();
    let shipping_minor = if fulfillment == Fulfillment::Ship { shop.shipping_minor } else { 0 };
    let reference = new_reference();
    let mut order = Order {
        id: reference.clone(),
        shop_slug: shop.slug.clone(),
        status: OrderStatus::AwaitingPayment,
        paypal_order_id: None,
        paypal_capture_id: None,
        items,
        currency: shop.currency.clone(),
        items_minor,
        shipping_minor,
        total_minor: items_minor + shipping_minor,
        fulfillment,
        note: args.note.unwrap_or_default().chars().take(500).collect(),
        channel,
        approval_url: None,
        buyer_name: None,
        buyer_email: None,
        ship_to: None,
        tracking: None,
        created_at: now(),
        paid_at: None,
        request: None,
        contact: None,
        quote_note: None,
    };
    app.store.put_order(&order).await?;

    let created = match open_checkout(app, shop, &order, format!("create-{reference}"), base_url).await {
        Ok(created) => created,
        Err(error) => {
            app.store.set_order_status(&order, OrderStatus::Failed).await?;
            return Err(error);
        }
    };
    app.store.attach_paypal_order(&order, &created.id, &created.approval_url).await?;
    order.paypal_order_id = Some(created.id);
    order.approval_url = Some(created.approval_url);

    let summary = format!("Started checkout {reference} for {}", display_money(order.total_minor, &order.currency));
    app.store.record_event(&shop.slug, "checkout", &summary, channel).await?;

    let mut view = agent_order(shop, &order, base_url);
    view["next_step"] = json!("Give the shopper approval_url. Nothing is charged until they approve the order in PayPal.");
    Ok(view)
}

async fn record_payment(app: &App, shop: &Shop, order: &Order, payment: PaymentResult) -> AppResult<()> {
    let (Some(capture_id), Some("COMPLETED")) = (&payment.capture_id, payment.capture_status.as_deref()) else {
        return Ok(());
    };
    let changed = app
        .store
        .mark_order_paid(order, capture_id, payment.buyer_name.clone(), payment.buyer_email.clone(), payment.ship_to.clone())
        .await?;
    if changed {
        let summary = format!("Order {} paid: {}", order.id, display_money(order.total_minor, &order.currency));
        app.store.record_event(&shop.slug, "paid", &summary, order.channel).await?;
    }
    Ok(())
}

/// Brings an order up to date with PayPal, capturing it once the buyer has approved.
pub async fn sync_order(app: &App, shop: &Shop, order: Order) -> AppResult<Order> {
    let Some(paypal_id) = order.paypal_order_id.clone().filter(|_| order.status == OrderStatus::AwaitingPayment) else {
        return Ok(order);
    };
    let remote = match app.paypal.get_order(&paypal_id).await {
        Ok(remote) => remote,
        // PayPal forgets orders nobody approved. A quote stays payable, so it goes back to being priced.
        Err(error) if error.status == 404 => {
            let status = if order.is_quote() { OrderStatus::Quoted } else { OrderStatus::Cancelled };
            app.store.set_order_status(&order, status).await?;
            return Ok(Order { status, ..order });
        }
        Err(error) => return Err(error.into()),
    };
    match remote.order_status.as_str() {
        "APPROVED" => {
            let captured = app.paypal.capture_order(&paypal_id).await?;
            record_payment(app, shop, &order, captured).await?;
        }
        "COMPLETED" => record_payment(app, shop, &order, remote).await?,
        _ => {}
    }
    Ok(app.store.get_order(&shop.slug, &order.id).await?.unwrap_or(order))
}

pub async fn order_status(app: &App, shop: &Shop, reference: &str, base_url: &str) -> AppResult<Value> {
    let Some(order) = app.store.get_order(&shop.slug, reference.trim()).await? else {
        return Err(user_error(format!("No order with reference \"{reference}\" at {}.", shop.name)));
    };
    Ok(agent_order(shop, &sync_order(app, shop, order).await?, base_url))
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct QuoteArgs {
    /// Everything the shop needs to set a price, such as what it is, size, quantity, date and wording
    pub request: String,
    /// The product this is about, when it is in the catalog
    pub product_id: Option<String>,
    /// How the shopper wants to receive it
    pub fulfillment: Option<Fulfillment>,
    /// How the shop can reach the shopper with questions, such as an email address
    pub contact: Option<String>,
}

/// Asks the shop for a price. Starts as an order with no amount, then joins the normal path once priced.
pub async fn request_quote(app: &App, shop: &Shop, args: QuoteArgs, channel: Channel, base_url: &str) -> AppResult<Value> {
    ensure_open(shop)?;
    let request: String = args.request.trim().chars().take(MAX_REQUEST_CHARS).collect();
    if request.is_empty() {
        return Err(user_error("Describe what the shopper wants priced."));
    }
    let product = match args.product_id.as_deref().map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => Some(app.store.get_product(&shop.slug, id).await?.ok_or_else(|| user_error(format!("No product with id \"{id}\". Use search_products to find valid ids.")))?),
        None => None,
    };
    let fulfillment = choose_fulfillment(shop, args.fulfillment)?;
    let (product_id, name) = product.map_or_else(|| (String::new(), request.chars().take(80).collect()), |product| (product.id, product.name));
    let order = Order {
        id: new_reference(),
        shop_slug: shop.slug.clone(),
        status: OrderStatus::AwaitingQuote,
        paypal_order_id: None,
        paypal_capture_id: None,
        items: vec![OrderItem { product_id, name, options: vec![], quantity: 1, unit_minor: 0 }],
        currency: shop.currency.clone(),
        items_minor: 0,
        shipping_minor: 0,
        total_minor: 0,
        fulfillment,
        note: String::new(),
        channel,
        approval_url: None,
        buyer_name: None,
        buyer_email: None,
        ship_to: None,
        tracking: None,
        created_at: now(),
        paid_at: None,
        contact: args.contact.map(|contact| contact.trim().chars().take(200).collect::<String>()).filter(|contact| !contact.is_empty()),
        request: Some(request),
        quote_note: None,
    };
    app.store.put_order(&order).await?;
    app.store.record_event(&shop.slug, "quote", &format!("Asked for a price: {}", order.items[0].name), channel).await?;
    Ok(agent_order(shop, &order, base_url))
}

fn optional_note(note: &str) -> Option<&str> {
    Some(note.trim()).filter(|note| !note.is_empty())
}

/// Sets the full amount the shopper will pay. Delivery is part of the owner's price.
pub async fn send_quote(app: &App, shop: &Shop, order: Order, price: f64, note: &str) -> AppResult<Order> {
    if !price.is_finite() || price <= 0.0 || price > MAX_PRICE {
        return Err(user_error("Enter a price greater than zero."));
    }
    let moved_on = || user_error("This request has already moved on.");
    if !matches!(order.status, OrderStatus::AwaitingQuote | OrderStatus::Quoted) {
        return Err(moved_on());
    }
    let total_minor = to_minor(price, &order.currency);
    let mut items = order.items.clone();
    if let Some(item) = items.first_mut() {
        item.unit_minor = total_minor;
    }
    if !app.store.set_quote(&order, &items, total_minor, optional_note(note)).await? {
        return Err(moved_on());
    }
    let summary = format!("Quoted {} for {}", display_money(total_minor, &order.currency), order.id);
    app.store.record_event(&shop.slug, "quote", &summary, Channel::Owner).await?;
    Ok(Order { status: OrderStatus::Quoted, items, items_minor: total_minor, total_minor, quote_note: optional_note(note).map(str::to_owned), ..order })
}

pub async fn decline_quote(app: &App, shop: &Shop, order: Order, note: &str) -> AppResult<Order> {
    let moved_on = || user_error("This request has already moved on.");
    if !matches!(order.status, OrderStatus::AwaitingQuote | OrderStatus::Quoted) || !app.store.decline_quote(&order, optional_note(note)).await? {
        return Err(moved_on());
    }
    app.store.record_event(&shop.slug, "quote", &format!("Declined request {}", order.id), Channel::Owner).await?;
    Ok(Order { status: OrderStatus::Declined, quote_note: optional_note(note).map(str::to_owned), ..order })
}

/// Opens PayPal for a priced quote and returns the approval link. PayPal forgets unapproved
/// orders within hours and a shopper may come back days later, so the checkout starts here.
pub async fn pay_quote(app: &App, shop: &Shop, order: Order, base_url: &str) -> AppResult<String> {
    let order = sync_order(app, shop, order).await?;
    match (order.status, &order.approval_url) {
        (OrderStatus::AwaitingPayment, Some(url)) => return Ok(url.clone()),
        (OrderStatus::Quoted, _) => {}
        _ => return Err(user_error("This request isn't waiting for payment.")),
    }
    let key = format!("create-{}-{}", order.id, chrono::Utc::now().timestamp_millis());
    let created = open_checkout(app, shop, &order, key, base_url).await?;
    if !app.store.start_quote_checkout(&order, &created.id, &created.approval_url).await? {
        let current = app.store.get_order(&shop.slug, &order.id).await?;
        return current.and_then(|current| current.approval_url).ok_or_else(|| user_error("This request isn't waiting for payment."));
    }
    app.store.record_event(&shop.slug, "checkout", &format!("Started paying {} for {}", order.id, display_money(order.total_minor, &order.currency)), order.channel).await?;
    Ok(created.approval_url)
}

/// Tracking is shared with the shopper's assistant. It stays out of PayPal
/// because PayPal only lets the account that received a payment add it there.
pub async fn track_order(app: &App, shop: &Shop, order: Order, carrier: &str, number: &str) -> AppResult<Order> {
    if order.status != OrderStatus::Paid {
        return Err(user_error("Only paid orders can be given tracking."));
    }
    let tracking = Tracking { carrier: carrier.to_owned(), number: number.to_owned() };
    app.store.set_order_tracking(&order, &tracking).await?;
    app.store.record_event(&shop.slug, "tracking", &format!("Added {carrier} tracking to order {}", order.id), Channel::Owner).await?;
    Ok(Order { tracking: Some(tracking), ..order })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn product(name: &str, tags: &[&str]) -> Product {
        Product {
            id: "p".into(),
            name: name.into(),
            description: "A lovely thing".into(),
            price_minor: 100,
            category: "Cakes".into(),
            tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
            options: vec![],
            image_url: None,
            source_url: String::new(),
            in_stock: true,
            price_text: String::new(),
            price_verified: true,
            position: 0,
            by_quote: false,
        }
    }

    #[test]
    fn tokenizes_and_stems() {
        assert_eq!(tokenize("I want some Birthday Cakes!"), vec!["birthday", "cake"]);
        assert_eq!(tokenize("gluten-free berries"), vec!["gluten", "free", "berry"]);
        assert_eq!(tokenize("glass"), vec!["glass"]);
        assert_eq!(tokenize("pâtisserie 東京"), vec!["pâtisserie", "東京"]);
    }

    #[test]
    fn scores_names_and_tags_above_descriptions() {
        let tokens = tokenize("birthday cake");
        let (score, matched) = score_product(&product("Chocolate Cake", &["birthday"]), &tokens);
        assert_eq!((score, matched), (3 + 2 + 3, 2));
        let (score, matched) = score_product(&product("Lemon Loaf", &[]), &tokenize("lovely"));
        assert_eq!((score, matched), (1, 1));
        assert_eq!(score_product(&product("Lemon Loaf", &[]), &tokenize("vegan")).1, 0);
    }

    #[test]
    fn requires_a_choice_for_every_option() {
        let mut item = product("Cake", &[]);
        item.options = vec![crate::model::ProductOption { name: "Size".into(), values: vec!["6 inch".into(), "8 inch".into()] }];
        assert!(resolve_options(&item, &[]).is_err());
        let bad = [OptionChoice { name: "size".into(), value: "12 inch".into() }];
        assert!(resolve_options(&item, &bad).is_err());
        let good = [OptionChoice { name: "SIZE".into(), value: "8 INCH".into() }];
        assert_eq!(resolve_options(&item, &good).unwrap(), vec![("Size".to_owned(), "8 inch".to_owned())]);
    }
}
