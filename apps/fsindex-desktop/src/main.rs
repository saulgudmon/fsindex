mod app;
#[cfg(feature = "capture")]
mod capture;
mod table;
mod theme;
mod worker;

use clap::Parser;
use fsindex_core::{config::RootConfig, Config, Engine};
use std::{path::PathBuf, sync::Arc};

#[derive(Parser)]
#[command(
    version,
    about = "Live filename search with an embedded in-memory index"
)]
struct Args {
    /// Configuration file (defaults to ~/.config/fsindex-mk2/fsindex.toml).
    #[arg(long)]
    config: Option<PathBuf>,
    /// Index these roots instead of those in the configuration file.
    #[arg(long = "root")]
    roots: Vec<PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let explicit_config = args.config.is_some();
    let config_path = args
        .config
        .unwrap_or_else(fsindex_core::config::default_config_path);
    let mut config = if explicit_config || config_path.exists() {
        Config::load(&config_path)?
    } else {
        Config::default()
    };
    if !args.roots.is_empty() {
        config.roots = args
            .roots
            .iter()
            .map(|p| RootConfig::new(p.to_string_lossy()))
            .collect();
    }
    let engine = Engine::build(config, config_path)?;
    let background = Arc::clone(&engine);
    let indexer = std::thread::Builder::new()
        .name("fsindex-indexer".into())
        .spawn(move || {
            background.initial_scan();
            background.maintenance_loop();
        })?;
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 740.0])
            .with_min_inner_size([720.0, 400.0])
            .with_app_id("dev.fsindex.mk2")
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!(
                "../assets/icon.png"
            ))?),
        ..Default::default()
    };
    let gui_engine = Arc::clone(&engine);
    let result = eframe::run_native(
        "fsindex",
        options,
        Box::new(move |cc| Ok(Box::new(app::Desktop::new(cc, gui_engine)))),
    );
    engine.request_shutdown();
    indexer
        .join()
        .map_err(|_| anyhow::anyhow!("indexer thread panicked"))?;
    result.map_err(|e| anyhow::anyhow!(e.to_string()))
}
