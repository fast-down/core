mod args_parse;
mod clean;
mod download;
mod draw_progress;
mod fmt_progress;
mod fmt_size;
mod fmt_time;
mod persist;
mod reverse_progress;
mod str_to_progress;
mod update;

use args_parse::Args;
use color_eyre::eyre::Result;
use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn main() -> Result<()> {
    color_eyre::install()?;
    let args = Args::parse()?;
    match args {
        Args::Download(download_args) => download::download(download_args),
        Args::Update => update::update(),
        Args::Clean => clean::clean(),
    }
}
