use clap::{Parser, Subcommand};
use fwsi::Reader;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "fwsi")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Info { path: PathBuf },
    Verify { path: PathBuf },
}

fn main() -> fwsi::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Info { path } => {
            let reader = Reader::open(path)?;
            let info = reader.info();
            println!(
                "width={} height={} tile_size={} jpeg_quality={} tiles={}x{}",
                info.width,
                info.height,
                info.tile_size,
                info.jpeg_quality,
                info.tiles_x,
                info.tiles_y
            );
            println!("image_hash={}", info.image_hash_hex());
            println!("content_id={}", info.content_id());
        }
        Command::Verify { path } => {
            let info = Reader::open(path)?.verify()?;
            println!("{}", info.image_hash_hex());
        }
    }
    Ok(())
}
