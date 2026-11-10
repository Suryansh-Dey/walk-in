use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::HeaderMap;
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::config::Config;
use crate::model::{Fulfillment, Order, OrderItem, ShipTo};
use crate::money::format_minor;

#[derive(Debug, thiserror::Error)]
#[error("PayPal error {status}: {message}")]
pub struct PayPalError {
    pub status: u16,
    pub message: String,
    pub issue: Option<String>,
    pub debug_id: Option<String>,
}

impl PayPalError {
    fn simple(message: impl Into<String>) -> Self {
        Self { status: 0, message: message.into(), issue: None, debug_id: None }
    }
}

impl From<reqwest::Error> for PayPalError {
    fn from(error: reqwest::Error) -> Self {
        Self::simple(error.to_string())
    }
}

struct CachedToken {
    value: String,
    expires_at: Instant,
}

#[derive(Clone)]
pub struct PayPal {
    http: reqwest::Client,
    base: String,
    client_id: String,
    client_secret: String,
    webhook_id: String,
    token: Arc<Mutex<Option<CachedToken>>>,
}

pub struct CheckoutRequest<'a> {
    pub reference: &'a str,
    /// Repeating a key returns the same PayPal order instead of creating another.
    pub idempotency_key: String,
    /// Lets webhooks find the order without a secondary index.
    pub custom_id: &'a str,
    pub brand_name: &'a str,
    pub payee_email: &'a str,
    pub order: &'a Order,
    pub fulfillment: Fulfillment,
    pub return_url: &'a str,
    pub cancel_url: &'a str,
}

pub struct CreatedOrder {
    pub id: String,
    pub approval_url: String,
}

#[derive(Debug)]
pub struct PaymentResult {
    pub order_status: String,
    pub capture_id: Option<String>,
    pub capture_status: Option<String>,
    pub buyer_name: Option<String>,
    pub buyer_email: Option<String>,
    pub ship_to: Option<ShipTo>,
}

fn clip(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn describe_item(item: &OrderItem) -> String {
    if item.options.is_empty() {
        return clip(&item.name, 127);
    }
    let chosen: Vec<String> = item.options.iter().map(|(name, value)| format!("{name}: {value}")).collect();
    clip(&format!("{} ({})", item.name, chosen.join(", ")), 127)
}

fn read_payment(order: &Value) -> PaymentResult {
    let unit = &order["purchase_units"][0];
    let capture = &unit["payments"]["captures"][0];
    let payer = if order["payer"].is_object() { &order["payer"] } else { &order["payment_source"]["paypal"] };
    let name: Vec<&str> = ["given_name", "surname"].iter().filter_map(|key| payer["name"][key].as_str()).collect();
    let address = &unit["shipping"]["address"];
    let address_line: Vec<&str> = ["address_line_1", "address_line_2", "admin_area_2", "admin_area_1", "postal_code", "country_code"]
        .iter()
        .filter_map(|key| address[key].as_str())
        .collect();
    PaymentResult {
        order_status: order["status"].as_str().unwrap_or_default().to_owned(),
        capture_id: capture["id"].as_str().map(str::to_owned),
        capture_status: capture["status"].as_str().map(str::to_owned),
        buyer_name: (!name.is_empty()).then(|| name.join(" ")),
        buyer_email: payer["email_address"].as_str().map(str::to_owned),
        ship_to: (!address_line.is_empty()).then(|| ShipTo {
            name: unit["shipping"]["name"]["full_name"].as_str().unwrap_or_default().to_owned(),
            address: address_line.join(", "),
        }),
    }
}

impl PayPal {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder().timeout(Duration::from_secs(30)).build()?,
            base: config.paypal_api_base.clone(),
            client_id: config.paypal_client_id.clone(),
            client_secret: config.paypal_client_secret.clone(),
            webhook_id: config.paypal_webhook_id.clone(),
            token: Arc::new(Mutex::new(None)),
        })
    }

    async fn access_token(&self) -> Result<String, PayPalError> {
        let mut cached = self.token.lock().await;
        if let Some(token) = cached.as_ref().filter(|token| token.expires_at > Instant::now()) {
            return Ok(token.value.clone());
        }
        if self.client_id.is_empty() || self.client_secret.is_empty() {
            return Err(PayPalError::simple("PayPal credentials are not configured."));
        }
        let response = self
            .http
            .post(format!("{}/v1/oauth2/token", self.base))
            .basic_auth(&self.client_id, Some(&self.client_secret))
            .form(&[("grant_type", "client_credentials")])
            .send()
            .await?;
        if !response.status().is_success() {
            return Err(PayPalError { status: response.status().as_u16(), message: "PayPal rejected the app credentials.".into(), issue: None, debug_id: None });
        }
        #[derive(Deserialize)]
        struct Token {
            access_token: String,
            expires_in: u64,
        }
        let token: Token = response.json().await?;
        let value = token.access_token.clone();
        *cached = Some(CachedToken {
            value: token.access_token,
            expires_at: Instant::now() + Duration::from_secs(token.expires_in.saturating_sub(60)),
        });
        Ok(value)
    }

    async fn call(&self, method: Method, path: &str, body: Option<Value>, headers: &[(&str, String)]) -> Result<Value, PayPalError> {
        let mut request = self.http.request(method.clone(), format!("{}{path}", self.base)).bearer_auth(self.access_token().await?);
        for (name, value) in headers {
            request = request.header(*name, value);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await?;
        let status = response.status();
        let text = response.text().await?;
        let parsed: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if !status.is_success() {
            let issue = parsed["details"][0]["issue"].as_str().or(parsed["name"].as_str()).map(str::to_owned);
            let message = parsed["details"][0]["description"].as_str().or(parsed["message"].as_str()).unwrap_or("request failed").to_owned();
            let debug_id = parsed["debug_id"].as_str().map(str::to_owned);
            tracing::error!(%method, path, status = status.as_u16(), ?issue, ?debug_id, "paypal call failed");
            return Err(PayPalError { status: status.as_u16(), message, issue, debug_id });
        }
        Ok(parsed)
    }

    /// Creates an order whose funds go to the shop, not to this platform.
    pub async fn create_order(&self, request: CheckoutRequest<'_>) -> Result<CreatedOrder, PayPalError> {
        let order = request.order;
        let money = |minor: i64| json!({ "currency_code": order.currency, "value": format_minor(minor, &order.currency) });
        let mut amount = money(order.total_minor);
        amount["breakdown"] = json!({ "item_total": money(order.items_minor), "shipping": money(order.shipping_minor) });
        let items: Vec<Value> = order
            .items
            .iter()
            .map(|item| {
                json!({
                    "name": describe_item(item),
                    "quantity": item.quantity.to_string(),
                    "unit_amount": money(item.unit_minor),
                    "category": "PHYSICAL_GOODS",
                })
            })
            .collect();
        let body = json!({
            "intent": "CAPTURE",
            "purchase_units": [{
                "reference_id": request.reference,
                "custom_id": request.custom_id,
                "invoice_id": request.reference,
                "description": clip(&format!("Order {} from {}", request.reference, request.brand_name), 127),
                "payee": { "email_address": request.payee_email },
                "amount": amount,
                "items": items,
            }],
            "payment_source": { "paypal": { "experience_context": {
                "brand_name": clip(request.brand_name, 127),
                "landing_page": "NO_PREFERENCE",
                "shipping_preference": if request.fulfillment == Fulfillment::Ship { "GET_FROM_FILE" } else { "NO_SHIPPING" },
                "user_action": "PAY_NOW",
                "return_url": request.return_url,
                "cancel_url": request.cancel_url,
            }}},
        });
        let created = self
            .call(Method::POST, "/v2/checkout/orders", Some(body), &[
                ("PayPal-Request-Id", request.idempotency_key),
                ("Prefer", "return=representation".to_owned()),
            ])
            .await?;
        let approval = created["links"]
            .as_array()
            .and_then(|links| links.iter().find(|link| matches!(link["rel"].as_str(), Some("payer-action" | "approve"))))
            .and_then(|link| link["href"].as_str())
            .ok_or_else(|| PayPalError::simple("PayPal did not return an approval link."))?;
        Ok(CreatedOrder { id: created["id"].as_str().unwrap_or_default().to_owned(), approval_url: approval.to_owned() })
    }

    pub async fn get_order(&self, id: &str) -> Result<PaymentResult, PayPalError> {
        Ok(read_payment(&self.call(Method::GET, &format!("/v2/checkout/orders/{id}"), None, &[]).await?))
    }

    /// Captures an approved order. Safe to call repeatedly for the same order.
    pub async fn capture_order(&self, id: &str) -> Result<PaymentResult, PayPalError> {
        let result = self
            .call(Method::POST, &format!("/v2/checkout/orders/{id}/capture"), Some(json!({})), &[
                ("PayPal-Request-Id", format!("capture-{id}")),
                ("Prefer", "return=representation".to_owned()),
            ])
            .await;
        match result {
            Ok(captured) => Ok(read_payment(&captured)),
            Err(error) if error.issue.as_deref() == Some("ORDER_ALREADY_CAPTURED") => self.get_order(id).await,
            Err(error) => Err(error),
        }
    }

    /// Verifies a webhook with PayPal. The raw body is spliced in unchanged,
    /// because re-serializing the event can alter it and make the signature check fail.
    pub async fn verify_webhook(&self, headers: &HeaderMap, raw_body: &str) -> Result<bool, PayPalError> {
        if self.webhook_id.is_empty() {
            return Ok(false);
        }
        let header = |name: &str| headers.get(name).and_then(|value| value.to_str().ok()).map(str::to_owned);
        let (Some(algo), Some(cert), Some(id), Some(sig), Some(time)) = (
            header("paypal-auth-algo"),
            header("paypal-cert-url"),
            header("paypal-transmission-id"),
            header("paypal-transmission-sig"),
            header("paypal-transmission-time"),
        ) else {
            return Ok(false);
        };
        let envelope = json!({
            "auth_algo": algo,
            "cert_url": cert,
            "transmission_id": id,
            "transmission_sig": sig,
            "transmission_time": time,
            "webhook_id": self.webhook_id,
            "webhook_event": "__EVENT__",
        })
        .to_string()
        .replacen("\"__EVENT__\"", raw_body, 1);
        let response = self
            .http
            .post(format!("{}/v1/notifications/verify-webhook-signature", self.base))
            .bearer_auth(self.access_token().await?)
            .header("content-type", "application/json")
            .body(envelope)
            .send()
            .await?;
        if !response.status().is_success() {
            return Ok(false);
        }
        let result: Value = response.json().await?;
        Ok(result["verification_status"].as_str() == Some("SUCCESS"))
    }
}
