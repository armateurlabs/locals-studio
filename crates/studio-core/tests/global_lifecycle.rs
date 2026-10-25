//! Global API lifecycle without inference workers or an installed Python runtime.
use anyhow::Result;
use reqwest::Client;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, time::Duration};
use studio_core::{paths::AppPaths, service::Studio, store::Store, types::StudioConfig};

async fn isolated_studio(root: &Path) -> Result<Arc<Studio>> {
    let paths = AppPaths {
        data_dir: root.join("data"),
        database_path: root.join("data/studio.sqlite3"),
        socket_path: root.join("data/studio.sock"),
        models_dir: root.join("models"),
        locals_source: root.join("source"),
    };
    paths.prepare()?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut config = StudioConfig::default();
    config.api.port = listener.local_addr()?.port();
    drop(listener);
    Store::open(&paths.database_path)?.set("settings", "config", &config)?;
    Studio::new(paths).await
}

async fn health_url(studio: &Studio) -> String {
    format!(
        "http://127.0.0.1:{}/health",
        studio.config.read().await.api.port
    )
}

async fn assert_http_stopped(client: &Client, url: &str) {
    assert!(
        client.get(url).send().await.is_err(),
        "global stop must close the API listener"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_stop_closes_http_and_start_all_can_run_without_selected_models() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let studio = isolated_studio(directory.path()).await?;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let url = health_url(&studio).await;
    let stale_config = studio.config.read().await.clone();
    let health: Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(health["status"], "ok");

    let stopped = tokio::time::timeout(
        Duration::from_secs(5),
        studio.handle("server.stop", json!({"all":true})),
    )
    .await??;
    assert_eq!(stopped["api"]["enabled"], false);
    assert_eq!(stopped["api"]["running"], false);
    assert!(stopped["servers"].as_array().unwrap().is_empty());
    assert_http_stopped(&client, &url).await;
    assert_eq!(
        studio.handle("ping", json!({})).await?["service"],
        "locals-studio"
    );
    let saved: StudioConfig = studio.store.get("settings", "config")?.unwrap();
    assert!(!saved.api.enabled);
    let settings = studio
        .handle("config.set", json!({"config":stale_config}))
        .await?;
    assert_eq!(
        settings["api"]["enabled"], false,
        "Saving old Settings must not undo global OFF"
    );
    assert_eq!(settings["api"]["running"], false);
    assert!(
        studio
            .handle("config.set", json!({"key":"api.enabled","value":true}))
            .await
            .is_err()
    );

    let started = tokio::time::timeout(
        Duration::from_secs(5),
        studio.handle("server.start", json!({"all":true})),
    )
    .await??;
    assert_eq!(started["api"]["enabled"], true);
    assert_eq!(started["api"]["running"], true);
    assert!(started["servers"].as_array().unwrap().is_empty());
    let health: Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(health["status"], "ok");
    let saved: StudioConfig = studio.store.get("settings", "config")?.unwrap();
    assert!(saved.api.enabled);
    studio.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn global_off_survives_reload_and_configuration_changes_until_explicit_start() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let studio = isolated_studio(directory.path()).await?;
    let client = Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let url = health_url(&studio).await;
    studio.handle("server.stop", json!({"all":true})).await?;
    let paths = studio.paths.clone();
    studio.shutdown().await?;
    drop(studio);

    let studio = Studio::new(paths).await?;
    assert!(!studio.config.read().await.api.enabled);
    assert_eq!(studio.api_state.read().await["running"], false);
    assert_http_stopped(&client, &url).await;
    assert_eq!(
        studio.handle("ping", json!({})).await?["service"],
        "locals-studio"
    );
    let changed = studio
        .handle(
            "config.set",
            json!({
                "key":"api.allowed_hosts","value":["configured.example"]
            }),
        )
        .await?;
    assert_eq!(changed["api"]["enabled"], false);
    assert_eq!(changed["api"]["running"], false);
    assert_http_stopped(&client, &url).await;

    let started = studio.handle("server.start", json!({"all":true})).await?;
    assert_eq!(started["api"]["running"], true);
    let health: Value = client
        .get(&url)
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    assert_eq!(health["status"], "ok");
    studio.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn menu_bar_preference_survives_reload_without_starting_inference() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let studio = isolated_studio(directory.path()).await?;
    assert_eq!(
        studio.handle("config.get", json!({})).await?["desktop"]["menu_bar_enabled"],
        false
    );
    studio.handle("server.stop", json!({"all":true})).await?;
    let changed = studio
        .handle(
            "config.set",
            json!({"key":"desktop.menu_bar_enabled","value":true}),
        )
        .await?;
    assert_eq!(changed["config"]["desktop"]["menu_bar_enabled"], true);
    assert_eq!(changed["api"]["enabled"], false);
    assert_eq!(changed["api"]["running"], false);
    assert!(
        studio
            .handle("server.status", json!({}))
            .await?
            .as_array()
            .unwrap()
            .is_empty()
    );
    let paths = studio.paths.clone();
    studio.shutdown().await?;
    drop(studio);

    let studio = Studio::new(paths).await?;
    assert_eq!(
        studio
            .handle("config.get", json!({"key":"desktop.menu_bar_enabled"}))
            .await?,
        true
    );
    assert_eq!(studio.api_state.read().await["running"], false);
    let changed = studio
        .handle(
            "config.set",
            json!({"key":"desktop.menu_bar_enabled","value":false}),
        )
        .await?;
    assert_eq!(changed["config"]["desktop"]["menu_bar_enabled"], false);
    assert_eq!(changed["api"]["enabled"], false);
    assert_eq!(changed["api"]["running"], false);
    studio.shutdown().await?;
    Ok(())
}
