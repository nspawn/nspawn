pub mod modules;

use {
    clap::{ArgMatches, CommandFactory},
    clap_complete::{generate, Shell},
    std::io,
};

use {
    clap::Parser,
    modules::{args::Arguments, manager::handle_arguments},
};

fn generate_completions(matches: &ArgMatches) {
    if let Some(generator) = matches.get_one::<Shell>("autocomplete").copied() {
        let cmd = Arguments::command();
        generate(
            generator,
            &mut cmd.clone(),
            &cmd.get_name().to_string(),
            &mut io::stdout(),
        );
    } else {
        eprintln!("Invalid generator specified");
    }
    std::process::exit(0);
}

fn try_main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Arguments::parse();

    handle_arguments(args)?;

    Ok(())
}

fn main() {
    let matches = Arguments::command().get_matches();

    if matches.contains_id("autocomplete") {
        generate_completions(&matches);
    }

    if let Err(e) = try_main() {
        eprintln!("Error: {e}");
        std::process::exit(1);
    }
}
