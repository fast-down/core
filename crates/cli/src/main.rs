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

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Args::parse()?;
    match args {
        Args::Download(download_args) => download::download(download_args).await,
        Args::Update => update::update(),
        Args::Clean => clean::clean().await,
    }
}
