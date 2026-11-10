use anyhow::anyhow;
use lambda_runtime::{LambdaEvent, service_fn};
use walk_in::App;
use walk_in::importer::{ImportJob, run_import};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    walk_in::init_tracing();
    let app = App::from_env().await?;
    let app = &app;
    lambda_runtime::run(service_fn(move |event: LambdaEvent<ImportJob>| async move {
        run_import(app, &event.payload.slug).await;
        Ok::<(), lambda_runtime::Error>(())
    }))
    .await
    .map_err(|error| anyhow!(error))
}
