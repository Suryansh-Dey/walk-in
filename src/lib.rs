pub mod chat;
pub mod commerce;
pub mod config;
pub mod crawl;
pub mod error;
pub mod extract;
pub mod importer;
pub mod llm;
pub mod mcp;
pub mod model;
pub mod money;
pub mod paypal;
pub mod safe_fetch;
pub mod store;
pub mod tools;
pub mod web;

use std::sync::Arc;

use anyhow::Context;
use aws_sdk_lambda::primitives::Blob;
use aws_sdk_lambda::types::InvocationType;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::llm::Llm;
use crate::paypal::PayPal;
use crate::safe_fetch::SafeFetcher;
use crate::store::Store;

/// Everything a request or background job needs. Cheap to clone.
#[derive(Clone)]
pub struct App {
    pub config: Arc<Config>,
    pub store: Store,
    pub paypal: PayPal,
    pub llm: Llm,
    pub fetcher: SafeFetcher,
    lambda: Option<aws_sdk_lambda::Client>,
}

impl App {
    pub async fn from_env() -> anyhow::Result<Self> {
        Self::new(Config::from_env()).await
    }

    pub async fn new(config: Config) -> anyhow::Result<Self> {
        let lambda = match config.importer_function {
            Some(_) => Some(aws_sdk_lambda::Client::new(&aws_config::load_from_env().await)),
            None => None,
        };
        Ok(Self {
            store: Store::connect(&config).await,
            paypal: PayPal::new(&config)?,
            llm: Llm::new(&config)?,
            fetcher: SafeFetcher::new(config.allow_private_fetch)?,
            config: Arc::new(config),
            lambda,
        })
    }

    /// Runs an import after the request returns: a separate Lambda when deployed, a task locally.
    pub async fn dispatch_import(&self, slug: &str) -> anyhow::Result<()> {
        match (&self.lambda, &self.config.importer_function) {
            (Some(client), Some(function)) => {
                client
                    .invoke()
                    .function_name(function)
                    .invocation_type(InvocationType::Event)
                    .payload(Blob::new(serde_json::to_vec(&importer::ImportJob { slug: slug.to_owned() })?))
                    .send()
                    .await
                    .context("start importer")?;
            }
            _ => {
                let app = self.clone();
                let slug = slug.to_owned();
                tokio::spawn(async move { importer::run_import(&app, &slug).await });
            }
        }
        Ok(())
    }

    /// Per-visitor limit plus a site-wide cap, so AI spend stays bounded even if IPs rotate.
    pub async fn rate_limit(&self, name: &str, ip: &str, per_ip: u64, global: u64, window_secs: i64) -> AppResult<()> {
        let visitor = self.store.hit_rate_limit(&format!("{name}:{ip}"), window_secs).await?;
        let everyone = self.store.hit_rate_limit(&format!("{name}:all"), window_secs).await?;
        if visitor > per_ip || everyone > global {
            return Err(AppError::RateLimited);
        }
        Ok(())
    }
}

pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with_target(false)
        .without_time()
        .init();
}
