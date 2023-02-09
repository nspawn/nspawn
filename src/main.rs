pub mod modules;

use {
    clap::Parser,
    modules::{args::Arguments, manager::handle_arguments},
};

fn try_main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Arguments::parse();

    handle_arguments(args)?;

    Ok(())
}

fn main() {
    if let Err(e) = try_main() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
