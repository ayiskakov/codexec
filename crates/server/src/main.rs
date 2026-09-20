use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use codexec_judge::{IsolateSandbox, Judge, LanguageRegistry, ProblemSet};
use codexec_server::config::Config;
use codexec_server::store::Store;
use codexec_server::{http, worker, AppState};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,tower_http=warn".into()),
        )
        .init();

    let config_path: PathBuf = std::env::args()
        .nth(1)
        .or_else(|| std::env::var("CODEXEC_CONFIG").ok())
        .unwrap_or_else(|| "codexec.toml".into())
        .into();
    let config = Config::load(&config_path)?;

    let languages = Arc::new(LanguageRegistry::load(&config.server.languages_file)?);
    let problems = ProblemSet::load(&config.server.problems_dir)
        .with_context(|| format!("loading problems from {}", config.server.problems_dir.display()))?;
    if languages.is_empty() {
        bail!("{} defines no languages", config.server.languages_file.display());
    }

    let sandbox = Arc::new(IsolateSandbox::new(config.sandbox.clone())?);
    let isolate_version = sandbox.check().await?;
    let slots = sandbox.slots();
    if !config.sandbox.use_cgroups {
        tracing::warn!(
            "use_cgroups = false: memory limits fall back to rlimits and Go runs without a hard memory cap. Development only."
        );
    }

    let store = Store::open(&config.server.database)?;
    let requeued = store.requeue_inflight().await?;

    let judge = Judge::new(sandbox, languages.clone(), config.judge.clone());
    let listen = config.server.listen;
    let state = AppState::new(config.server, store, judge, problems);

    tracing::info!(
        %isolate_version,
        slots,
        languages = ?languages.ids().collect::<Vec<_>>(),
        problems = state.problems.len(),
        requeued,
        "codexec ready"
    );

    worker::spawn_workers(state.clone(), slots);
    worker::spawn_sweeper(state.clone());

    let listener =
        tokio::net::TcpListener::bind(listen).await.with_context(|| format!("binding {listen}"))?;
    tracing::info!("listening on http://{listen}");
    axum::serve(listener, http::router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down; in-flight jobs are re-queued on the next start");
        })
        .await?;
    Ok(())
}
