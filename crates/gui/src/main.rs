// Prevent console window in addition to Slint window in Windows release builds when, e.g., starting the app via file manager. Ignored on other platforms.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod args;
mod clean;
mod fmt;
mod home_page;
mod manager;
mod path;
mod persist;
mod progress;
mod update;

use args::Args;
use color_eyre::Result;
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    println!("fast-down v{}", env!("CARGO_PKG_VERSION"));
    let args = Args::parse()?;
    match args {
        Args::Download(download_args) => home_page::home_page(download_args).await,
        Args::Update => update::update().await,
        Args::Clean => clean::clean().await,
    }
}
