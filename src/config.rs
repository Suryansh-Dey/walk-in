use std::env;

fn var(name: &str) -> String {
    env::var(name).unwrap_or_default()
}

fn var_or(name: &str, fallback: &str) -> String {
    env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| fallback.to_owned())
}

fn flag(name: &str) -> bool {
    var(name) == "1"
}

#[derive(Clone, Debug)]
pub struct Config {
    pub port: u16,
    /// Overrides the origin taken from the request. Needed behind a CDN.
    pub public_base_url: Option<String>,
    pub table_name: String,
    /// Points at a local DynamoDB for development and tests.
    pub dynamodb_endpoint: Option<String>,
    /// When set, imports run in this Lambda function. Otherwise they run in-process.
    pub importer_function: Option<String>,
    pub openrouter_api_key: String,
    pub openrouter_model: String,
    pub openrouter_api_base: String,
    pub paypal_api_base: String,
    pub paypal_client_id: String,
    pub paypal_client_secret: String,
    pub paypal_webhook_id: String,
    pub demo_merchant_email: String,
    /// Local development only: lets the crawler read localhost.
    pub allow_private_fetch: bool,
    /// Lets owners publish without proving they control the site. Sandbox demos only.
    pub allow_unverified_publish: bool,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            port: var_or("PORT", "3000").parse().unwrap_or(3000),
            public_base_url: env::var("PUBLIC_BASE_URL")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| v.trim_end_matches('/').to_owned()),
            table_name: var_or("TABLE_NAME", "walk-in"),
            dynamodb_endpoint: env::var("DYNAMODB_ENDPOINT").ok().filter(|v| !v.is_empty()),
            importer_function: env::var("IMPORTER_FUNCTION").ok().filter(|v| !v.is_empty()),
            openrouter_api_key: var("OPENROUTER_API_KEY"),
            openrouter_model: var_or("OPENROUTER_MODEL", "deepseek/deepseek-v4.1-flash-1017"),
            openrouter_api_base: var_or("OPENROUTER_API_BASE", "https://openrouter.ai/api/v1")
                .trim_end_matches('/')
                .to_owned(),
            paypal_api_base: var_or("PAYPAL_API_BASE", "https://api-m.sandbox.paypal.com")
                .trim_end_matches('/')
                .to_owned(),
            paypal_client_id: var("PAYPAL_CLIENT_ID"),
            paypal_client_secret: var("PAYPAL_CLIENT_SECRET"),
            paypal_webhook_id: var("PAYPAL_WEBHOOK_ID"),
            demo_merchant_email: var("DEMO_MERCHANT_EMAIL"),
            allow_private_fetch: flag("ALLOW_PRIVATE_FETCH"),
            allow_unverified_publish: flag("ALLOW_UNVERIFIED_PUBLISH"),
        }
    }

    /// The PayPal website that matches the API in use.
    pub fn paypal_web_base(&self) -> &'static str {
        if self.paypal_api_base.contains("sandbox") { "https://www.sandbox.paypal.com" } else { "https://www.paypal.com" }
    }

    pub fn paypal_configured(&self) -> bool {
        !self.paypal_client_id.is_empty() && !self.paypal_client_secret.is_empty()
    }
}
