use clap::Parser;

fn main() {
    let cli = agent_usage_dashboard::cli::Cli::parse();
    if let Err(error) = agent_usage_dashboard::cli::run(cli) {
        if let Some(code) = error.exit_code() {
            std::process::exit(code);
        }
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}
