use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShopStatus {
    Importing,
    Review,
    Live,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderStatus {
    AwaitingPayment,
    Paid,
    Refunded,
    Cancelled,
    Failed,
}

impl OrderStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            OrderStatus::AwaitingPayment => "awaiting_payment",
            OrderStatus::Paid => "paid",
            OrderStatus::Refunded => "refunded",
            OrderStatus::Cancelled => "cancelled",
            OrderStatus::Failed => "failed",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Mcp,
    Chat,
    Owner,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Fulfillment {
    Ship,
    Pickup,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProgressStep {
    pub label: String,
    pub done: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Shop {
    pub slug: String,
    pub owner_key: String,
    pub source_url: String,
    pub status: ShopStatus,
    pub progress: Vec<ProgressStep>,
    pub error: Option<String>,
    pub name: String,
    pub description: String,
    pub location: String,
    pub contact: String,
    pub hours: String,
    pub currency: String,
    pub ships: bool,
    pub pickup: bool,
    pub shipping_minor: i64,
    pub shipping_policy: String,
    pub returns_policy: String,
    pub paypal_email: String,
    pub verified: bool,
    pub verify_token: String,
    pub warnings: Vec<String>,
    pub created_at: String,
    pub published_at: Option<String>,
}

#[open_client::openai_schema]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProductOption {
    pub name: String,
    pub values: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Product {
    pub id: String,
    pub name: String,
    pub description: String,
    pub price_minor: i64,
    pub category: String,
    pub tags: Vec<String>,
    pub options: Vec<ProductOption>,
    pub image_url: Option<String>,
    pub source_url: String,
    pub in_stock: bool,
    pub price_text: String,
    pub price_verified: bool,
    pub position: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderItem {
    pub product_id: String,
    pub name: String,
    pub options: Vec<(String, String)>,
    pub quantity: u32,
    pub unit_minor: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShipTo {
    pub name: String,
    pub address: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tracking {
    pub carrier: String,
    pub number: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub id: String,
    pub shop_slug: String,
    pub status: OrderStatus,
    pub paypal_order_id: Option<String>,
    pub paypal_capture_id: Option<String>,
    pub items: Vec<OrderItem>,
    pub currency: String,
    pub items_minor: i64,
    pub shipping_minor: i64,
    pub total_minor: i64,
    pub fulfillment: Fulfillment,
    pub note: String,
    pub channel: Channel,
    pub approval_url: Option<String>,
    pub buyer_name: Option<String>,
    pub buyer_email: Option<String>,
    pub ship_to: Option<ShipTo>,
    pub tracking: Option<Tracking>,
    pub created_at: String,
    pub paid_at: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShopEvent {
    pub kind: String,
    pub summary: String,
    pub channel: Channel,
    pub created_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UnmetNeed {
    pub need: String,
    pub count: u64,
}

pub fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
