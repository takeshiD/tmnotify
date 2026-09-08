use clap::Parser;

#[tokio::main]
async fn main() -> std::process::ExitCode {
    let cli = tmnotify::cli::Cli::parse();
    match tmnotify::app::run(cli).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("tmnotify: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
