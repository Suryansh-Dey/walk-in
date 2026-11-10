//! DynamoDB single-table storage. Everything a shop owns lives under its own partition.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::error::SdkError;
use aws_sdk_dynamodb::operation::update_item::UpdateItemError;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, KeySchemaElement, KeyType, PutRequest, ReturnValue,
    ScalarAttributeType, TimeToLiveSpecification, WriteRequest,
};
use rand::Rng;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_dynamo::{from_item, to_attribute_value, to_item};

use crate::config::Config;
use crate::model::{Channel, Order, OrderStatus, Product, ShipTo, Shop, ShopEvent, Tracking, UnmetNeed, now};

type Item = HashMap<String, AttributeValue>;

const META: &str = "META";
const STATS: &str = "STATS";
const PRODUCT: &str = "PRODUCT#";
const ORDER: &str = "ORDER#";
const EVENT: &str = "EVENT#";
const UNMET: &str = "UNMET#";
const EVENT_RETENTION_SECS: i64 = 90 * 24 * 3600;

fn pk(slug: &str) -> String {
    format!("SHOP#{slug}")
}

fn s(value: impl Into<String>) -> AttributeValue {
    AttributeValue::S(value.into())
}

fn n(value: impl ToString) -> AttributeValue {
    AttributeValue::N(value.to_string())
}

fn unix_now() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Clone)]
pub struct Store {
    client: Client,
    table: String,
}

impl Store {
    pub async fn connect(config: &Config) -> Self {
        let shared = aws_config::load_from_env().await;
        let mut builder = aws_sdk_dynamodb::config::Builder::from(&shared);
        if let Some(endpoint) = &config.dynamodb_endpoint {
            builder = builder.endpoint_url(endpoint);
        }
        Self { client: Client::from_conf(builder.build()), table: config.table_name.clone() }
    }

    /// Creates the table on a local DynamoDB. Deployed tables come from the CloudFormation stack.
    pub async fn ensure_local_table(&self) -> Result<()> {
        if self.client.describe_table().table_name(&self.table).send().await.is_ok() {
            return Ok(());
        }
        let attribute = |name: &str| {
            AttributeDefinition::builder().attribute_name(name).attribute_type(ScalarAttributeType::S).build()
        };
        let key = |name: &str, kind: KeyType| KeySchemaElement::builder().attribute_name(name).key_type(kind).build();
        self.client
            .create_table()
            .table_name(&self.table)
            .billing_mode(BillingMode::PayPerRequest)
            .attribute_definitions(attribute("pk")?)
            .attribute_definitions(attribute("sk")?)
            .key_schema(key("pk", KeyType::Hash)?)
            .key_schema(key("sk", KeyType::Range)?)
            .send()
            .await
            .context("create local table")?;
        for _ in 0..50 {
            let described = self.client.describe_table().table_name(&self.table).send().await?;
            if described.table().and_then(|t| t.table_status()).is_some_and(|status| status.as_str() == "ACTIVE") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Local emulators may not implement TTL, and nothing depends on it locally.
        let _ = self
            .client
            .update_time_to_live()
            .table_name(&self.table)
            .time_to_live_specification(TimeToLiveSpecification::builder().attribute_name("expiresAt").enabled(true).build()?)
            .send()
            .await;
        Ok(())
    }

    fn item<T: Serialize>(partition: &str, sort: &str, value: &T) -> Result<Item> {
        let mut item: Item = to_item(value).context("serialize item")?;
        item.insert("pk".into(), s(partition));
        item.insert("sk".into(), s(sort));
        Ok(item)
    }

    async fn put<T: Serialize>(&self, partition: &str, sort: &str, value: &T) -> Result<()> {
        self.client
            .put_item()
            .table_name(&self.table)
            .set_item(Some(Self::item(partition, sort, value)?))
            .send()
            .await
            .context("put item")?;
        Ok(())
    }

    async fn get<T: DeserializeOwned>(&self, partition: &str, sort: &str) -> Result<Option<T>> {
        let output = self
            .client
            .get_item()
            .table_name(&self.table)
            .key("pk", s(partition))
            .key("sk", s(sort))
            .consistent_read(true)
            .send()
            .await
            .context("get item")?;
        output.item.map(|item| from_item(item).context("deserialize item")).transpose()
    }

    async fn query<T: DeserializeOwned>(&self, partition: &str, prefix: &str, limit: Option<usize>, newest_first: bool) -> Result<Vec<T>> {
        let mut results = Vec::new();
        let mut cursor: Option<Item> = None;
        loop {
            let output = self
                .client
                .query()
                .table_name(&self.table)
                .key_condition_expression("pk = :pk AND begins_with(sk, :prefix)")
                .expression_attribute_values(":pk", s(partition))
                .expression_attribute_values(":prefix", s(prefix))
                .scan_index_forward(!newest_first)
                .consistent_read(true)
                .set_exclusive_start_key(cursor.take())
                .set_limit(limit.map(|value| value as i32))
                .send()
                .await
                .context("query items")?;
            for item in output.items.unwrap_or_default() {
                results.push(from_item(item).context("deserialize item")?);
            }
            match output.last_evaluated_key {
                Some(key) if limit.is_none_or(|value| results.len() < value) => cursor = Some(key),
                _ => return Ok(results),
            }
        }
    }

    /// Sets fields on an item. Returns false when the status condition did not hold.
    async fn set_fields(&self, partition: &str, sort: &str, fields: Vec<(&str, AttributeValue)>, required_status: Option<&str>) -> Result<bool> {
        let mut request = self.client.update_item().table_name(&self.table).key("pk", s(partition)).key("sk", s(sort));
        let mut assignments = Vec::with_capacity(fields.len());
        for (index, (field, value)) in fields.into_iter().enumerate() {
            assignments.push(format!("#f{index} = :v{index}"));
            request = request
                .expression_attribute_names(format!("#f{index}"), field)
                .expression_attribute_values(format!(":v{index}"), value);
        }
        request = request.update_expression(format!("SET {}", assignments.join(", ")));
        request = match required_status {
            Some(status) => request
                .condition_expression("#required = :required")
                .expression_attribute_names("#required", "status")
                .expression_attribute_values(":required", s(status)),
            None => request.condition_expression("attribute_exists(pk)"),
        };
        match request.send().await {
            Ok(_) => Ok(true),
            Err(SdkError::ServiceError(error)) if matches!(error.err(), UpdateItemError::ConditionalCheckFailedException(_)) => Ok(false),
            Err(error) => Err(error).context("update item"),
        }
    }

    pub async fn put_shop(&self, shop: &Shop) -> Result<()> {
        self.put(&pk(&shop.slug), META, shop).await
    }

    pub async fn get_shop(&self, slug: &str) -> Result<Option<Shop>> {
        self.get(&pk(slug), META).await
    }

    pub async fn list_products(&self, slug: &str) -> Result<Vec<Product>> {
        let mut products: Vec<Product> = self.query(&pk(slug), PRODUCT, None, false).await?;
        products.sort_by_key(|product| product.position);
        Ok(products)
    }

    pub async fn get_product(&self, slug: &str, id: &str) -> Result<Option<Product>> {
        self.get(&pk(slug), &format!("{PRODUCT}{id}")).await
    }

    pub async fn put_product(&self, slug: &str, product: &Product) -> Result<()> {
        self.put(&pk(slug), &format!("{PRODUCT}{}", product.id), product).await
    }

    pub async fn delete_product(&self, slug: &str, id: &str) -> Result<()> {
        self.client
            .delete_item()
            .table_name(&self.table)
            .key("pk", s(pk(slug)))
            .key("sk", s(format!("{PRODUCT}{id}")))
            .send()
            .await
            .context("delete product")?;
        Ok(())
    }

    pub async fn put_products(&self, slug: &str, products: &[Product]) -> Result<()> {
        let partition = pk(slug);
        for chunk in products.chunks(25) {
            let mut writes = chunk
                .iter()
                .map(|product| {
                    let item = Self::item(&partition, &format!("{PRODUCT}{}", product.id), product)?;
                    Ok(WriteRequest::builder().put_request(PutRequest::builder().set_item(Some(item)).build()?).build())
                })
                .collect::<Result<Vec<_>>>()?;
            let mut attempt = 0u32;
            while !writes.is_empty() {
                let output = self
                    .client
                    .batch_write_item()
                    .request_items(self.table.clone(), writes)
                    .send()
                    .await
                    .context("batch write products")?;
                writes = output.unprocessed_items.and_then(|mut map| map.remove(&self.table)).unwrap_or_default();
                if !writes.is_empty() {
                    attempt += 1;
                    anyhow::ensure!(attempt <= 8, "DynamoDB kept throttling the product write");
                    tokio::time::sleep(Duration::from_millis(50 * 2u64.pow(attempt))).await;
                }
            }
        }
        Ok(())
    }

    pub async fn put_order(&self, order: &Order) -> Result<()> {
        self.put(&pk(&order.shop_slug), &format!("{ORDER}{}", order.id), order).await
    }

    pub async fn get_order(&self, slug: &str, id: &str) -> Result<Option<Order>> {
        self.get(&pk(slug), &format!("{ORDER}{id}")).await
    }

    pub async fn list_orders(&self, slug: &str) -> Result<Vec<Order>> {
        let mut orders: Vec<Order> = self.query(&pk(slug), ORDER, None, false).await?;
        orders.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        Ok(orders)
    }

    fn order_key(order: &Order) -> (String, String) {
        (pk(&order.shop_slug), format!("{ORDER}{}", order.id))
    }

    pub async fn attach_paypal_order(&self, order: &Order, paypal_order_id: &str, approval_url: &str) -> Result<()> {
        let (partition, sort) = Self::order_key(order);
        self.set_fields(&partition, &sort, vec![("paypalOrderId", s(paypal_order_id)), ("approvalUrl", s(approval_url))], None).await?;
        Ok(())
    }

    pub async fn set_order_status(&self, order: &Order, status: OrderStatus) -> Result<()> {
        let (partition, sort) = Self::order_key(order);
        self.set_fields(&partition, &sort, vec![("status", s(status.as_str()))], None).await?;
        Ok(())
    }

    pub async fn set_order_tracking(&self, order: &Order, tracking: &Tracking) -> Result<()> {
        let (partition, sort) = Self::order_key(order);
        self.set_fields(&partition, &sort, vec![("tracking", to_attribute_value(tracking)?)], None).await?;
        Ok(())
    }

    /// Moves an order to paid exactly once. Returns true for the call that made the change.
    pub async fn mark_order_paid(&self, order: &Order, capture_id: &str, buyer_name: Option<String>, buyer_email: Option<String>, ship_to: Option<ShipTo>) -> Result<bool> {
        let (partition, sort) = Self::order_key(order);
        self.set_fields(
            &partition,
            &sort,
            vec![
                ("status", s(OrderStatus::Paid.as_str())),
                ("paypalCaptureId", s(capture_id)),
                ("buyerName", to_attribute_value(buyer_name)?),
                ("buyerEmail", to_attribute_value(buyer_email)?),
                ("shipTo", to_attribute_value(ship_to)?),
                ("paidAt", s(now())),
            ],
            Some(OrderStatus::AwaitingPayment.as_str()),
        )
        .await
    }

    pub async fn record_event(&self, slug: &str, kind: &str, summary: &str, channel: Channel) -> Result<()> {
        let event = ShopEvent { kind: kind.to_owned(), summary: summary.to_owned(), channel, created_at: now() };
        let sort = format!("{EVENT}{:013}#{:04x}", chrono::Utc::now().timestamp_millis(), rand::rng().random::<u16>());
        let mut item = Self::item(&pk(slug), &sort, &event)?;
        item.insert("expiresAt".into(), n(unix_now() + EVENT_RETENTION_SECS));
        self.client.put_item().table_name(&self.table).set_item(Some(item)).send().await.context("record event")?;
        Ok(())
    }

    pub async fn list_events(&self, slug: &str, limit: usize) -> Result<Vec<ShopEvent>> {
        self.query(&pk(slug), EVENT, Some(limit), true).await
    }

    pub async fn add_visit(&self, slug: &str) -> Result<()> {
        self.client
            .update_item()
            .table_name(&self.table)
            .key("pk", s(pk(slug)))
            .key("sk", s(STATS))
            .update_expression("ADD visits :one")
            .expression_attribute_values(":one", n(1))
            .send()
            .await
            .context("add visit")?;
        Ok(())
    }

    pub async fn visits(&self, slug: &str) -> Result<u64> {
        #[derive(serde::Deserialize)]
        struct Stats {
            visits: u64,
        }
        Ok(self.get::<Stats>(&pk(slug), STATS).await?.map_or(0, |stats| stats.visits))
    }

    pub async fn record_unmet_need(&self, slug: &str, need: &str, normalized: &str) -> Result<()> {
        let key: String = normalized.chars().take(200).collect();
        self.client
            .update_item()
            .table_name(&self.table)
            .key("pk", s(pk(slug)))
            .key("sk", s(format!("{UNMET}{key}")))
            .update_expression("ADD #count :one SET need = if_not_exists(need, :need)")
            .expression_attribute_names("#count", "count")
            .expression_attribute_values(":one", n(1))
            .expression_attribute_values(":need", s(need))
            .send()
            .await
            .context("record unmet need")?;
        Ok(())
    }

    pub async fn list_unmet_needs(&self, slug: &str) -> Result<Vec<UnmetNeed>> {
        let mut needs: Vec<UnmetNeed> = self.query(&pk(slug), UNMET, None, false).await?;
        needs.sort_by_key(|need| std::cmp::Reverse(need.count));
        needs.truncate(12);
        Ok(needs)
    }

    /// Counts one hit against a fixed window shared by every Lambda instance.
    pub async fn hit_rate_limit(&self, key: &str, window_secs: i64) -> Result<u64> {
        let window = unix_now() / window_secs;
        let output = self
            .client
            .update_item()
            .table_name(&self.table)
            .key("pk", s(format!("RATE#{key}")))
            .key("sk", s(window.to_string()))
            .update_expression("ADD hits :one SET expiresAt = :expires")
            .expression_attribute_values(":one", n(1))
            .expression_attribute_values(":expires", n(unix_now() + window_secs * 2))
            .return_values(ReturnValue::UpdatedNew)
            .send()
            .await
            .context("rate limit")?;
        let hits = output
            .attributes
            .as_ref()
            .and_then(|attributes| attributes.get("hits"))
            .and_then(|value| value.as_n().ok())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        Ok(hits)
    }
}
