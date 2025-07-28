mod args;
mod clean;
mod download;
mod fmt;
mod persist;
mod progress;
mod update;

use args::Args;
use color_eyre::Result;
use mimalloc::MiMalloc;

#[macro_use]
extern crate rust_i18n;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

i18n!("../../locales");

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    println!("fast-down v{}", env!("CARGO_PKG_VERSION"));
    let args = Args::parse()?;
    match args {
        Args::Download(download_args) => download::download(download_args).await,
        Args::Update => update::update().await,
        Args::Clean => clean::clean().await,
    }
}
