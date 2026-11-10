use anyhow::anyhow;
use walk_in::App;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    walk_in::init_tracing();
    let app = App::from_env().await?;
    let port = app.config.port;
    let local_table = app.config.dynamodb_endpoint.is_some();
    if local_table {
        app.store.ensure_local_table().await?;
    }
    if !app.config.paypal_configured() {
        tracing::warn!("PayPal credentials are missing, so checkout will not work");
    }
    let router = walk_in::web::router(app);

    if std::env::var("AWS_LAMBDA_RUNTIME_API").is_ok() {
        return lambda_http::run(router).await.map_err(|error| anyhow!(error));
    }
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("Walk-In listening on http://localhost:{port}");
    axum::serve(listener, router).await?;
    Ok(())
}
