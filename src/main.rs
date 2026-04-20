use clap::Parser;
use libertai_cli::cli;

fn main() {
    if let Err(e) = cli::dispatch(cli::Cli::parse()) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
