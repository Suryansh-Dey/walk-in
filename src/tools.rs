//! The single definition of what an assistant can do in a shop, shared by MCP and the hosted chat.

use schemars::JsonSchema;
use schemars::generate::SchemaSettings;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::App;
use crate::commerce::{self, CheckoutArgs, QuoteArgs, SearchArgs};
use crate::error::{AppResult, user_error};
use crate::model::{Channel, Shop};

/// Builds a JSON schema with every type inlined, which tool clients handle best.
/// Tools keep ordinary optional parameters, unlike strict-mode structured output.
fn schema_of<T: JsonSchema>() -> Value {
    let generator = SchemaSettings::draft2020_12().with(|settings| settings.inline_subschemas = true).into_generator();
    let mut schema = generator.into_root_schema_for::<T>().to_value();
    if let Some(object) = schema.as_object_mut() {
        object.remove("$schema");
        object.remove("title");
    }
    schema
}

pub struct ToolDef {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub schema: Value,
    pub read_only: bool,
}

#[derive(Deserialize, JsonSchema)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
struct ProductArgs {
    /// Product id from search_products
    product_id: String,
}

#[derive(Deserialize, JsonSchema)]
struct OrderArgs {
    /// The reference returned by create_checkout or request_quote, such as WI-AB23CD45
    order_reference: String,
}

#[derive(Deserialize, JsonSchema)]
struct UnmetArgs {
    /// What the shopper wanted, in a few words
    need: String,
}

pub fn definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "get_shop_info",
            title: "Shop details",
            description: "Get this shop's description, location, opening hours, categories, delivery options and policies.",
            schema: schema_of::<NoArgs>(),
            read_only: true,
        },
        ToolDef {
            name: "search_products",
            title: "Search products",
            description: "Search what this shop sells. Use a few keywords, such as \"chocolate birthday cake\". Leave query empty to list the catalog. When match is \"some_terms\", the results only partly fit the request. Results with pricing \"quote\" have no fixed price, so use request_quote for those and never state a price.",
            schema: schema_of::<SearchArgs>(),
            read_only: true,
        },
        ToolDef {
            name: "get_product",
            title: "Product details",
            description: "Get one product, including the options that must be chosen before checkout.",
            schema: schema_of::<ProductArgs>(),
            read_only: true,
        },
        ToolDef {
            name: "create_checkout",
            title: "Start PayPal checkout",
            description: "Create an order for products with a fixed price and get a PayPal approval link for the shopper. Prices come from the shop's catalog. Nothing is charged until the shopper approves the order in PayPal, so always show them the total and the approval_url.",
            schema: schema_of::<CheckoutArgs>(),
            read_only: false,
        },
        ToolDef {
            name: "request_quote",
            title: "Ask the shop for a price",
            description: "Ask the shop to price custom work or a product with no fixed price. Collect the details the shop needs first. The shop replies with a price, and the shopper pays it through PayPal at status_url, so always give them that link.",
            schema: schema_of::<QuoteArgs>(),
            read_only: false,
        },
        ToolDef {
            name: "get_order_status",
            title: "Check an order",
            description: "Check an order or a quote request. Call this after the shopper says they approved a payment in PayPal, or to see whether the shop has set a price.",
            schema: schema_of::<OrderArgs>(),
            read_only: false,
        },
        ToolDef {
            name: "report_unmet_need",
            title: "Tell the owner what was missing",
            description: "Tell the shop owner the shopper wanted something this shop does not sell, such as \"gluten-free birthday cake\". Call this whenever you could not find a good match.",
            schema: schema_of::<UnmetArgs>(),
            read_only: false,
        },
    ]
}

pub struct ToolContext<'a> {
    pub app: &'a App,
    pub shop: &'a Shop,
    pub channel: Channel,
    pub base_url: &'a str,
}

fn parse<T: DeserializeOwned>(args: Value) -> AppResult<T> {
    serde_json::from_value(args).map_err(|error| user_error(format!("Those tool arguments were not valid: {error}")))
}

pub async fn dispatch(context: &ToolContext<'_>, name: &str, args: Value) -> AppResult<Value> {
    let ToolContext { app, shop, channel, base_url } = *context;
    match name {
        "get_shop_info" => commerce::shop_info(app, shop, channel).await,
        "search_products" => commerce::search_products(app, shop, parse(args)?, channel).await,
        "get_product" => {
            let args: ProductArgs = parse(args)?;
            commerce::product_details(app, shop, &args.product_id, channel).await
        }
        "create_checkout" => commerce::create_checkout(app, shop, parse(args)?, channel, base_url).await,
        "get_order_status" => {
            let args: OrderArgs = parse(args)?;
            commerce::order_status(app, shop, &args.order_reference, base_url).await
        }
        "request_quote" => commerce::request_quote(app, shop, parse(args)?, channel, base_url).await,
        "report_unmet_need" => {
            let args: UnmetArgs = parse(args)?;
            commerce::report_unmet_need(app, shop, &args.need, channel).await
        }
        other => Err(user_error(format!("Unknown tool {other}."))),
    }
}
