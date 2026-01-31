use std::{path::PathBuf, str::FromStr};

use anyhow::Context;
use tracing::instrument;

use convert_invert::internals::{
    context::context_manager::Managers,
    utils::{config::config_manager::Config, trace},
};

#[instrument]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = Config::try_from_env().context("Cannot read env vars for config")?;
    let attempt_num: usize = match std::env::args().nth(1) {
        Some(value) => value.parse().unwrap(),
        None => 1usize,
    };
    config.run_id = format!("{}_attempt_{}", config.run_id, attempt_num);
    println!("run_id: {}", config.run_id);
    tracing::info!(config = ?config, "read config");
    trace::otel_trace::init_tracing_with_otel("convert_invert".to_string(), config.run_id.clone())
        .context("Tracing")?;
    let download_path =
        PathBuf::from_str("/home/gonik/Music/widerSemaphores").context("Acquiring download dir")?;

    let (sender, receiver) = tokio::sync::mpsc::channel(6000);
    let managers = Managers::new(0.75, download_path, config);
    managers
        .run_cycle(sender, receiver)
        .await
        .context("Full shutdown")?;

    trace::otel_trace::shutdown_otel();
    Ok(())
}
