use clap::Parser;
use libertai_cli::cli;

fn main() {
    if let Err(e) = cli::dispatch(cli::Cli::parse()) {
        eprintln!("error: {e:#}");
        // Differentiated exit codes — see `client::exit_code` for the
        // contract (1 generic, 2 usage via clap, 3 auth, 4 network, 5 API).
        std::process::exit(libertai_cli::client::exit_code(&e));
    }
}
