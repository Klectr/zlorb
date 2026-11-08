mod utils;
use crate::utils::{daemon::DaemonManager, repo, setup, status};
use clap::{Parser, Subcommand};
use zlorb_lib::error::ZlorbError;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    #[command(subcommand)]
    cmd: Commands,
}

#[derive(Subcommand, Debug, Clone)]
enum Commands {
    /// remove a repo from the configs watched by the daemon @ ~/.config/zlorb/configs
    Remove {
        #[arg(short, long)]
        repo_name: String,
    },
    /// adds a repo to the configs watched by the daemon @ ~/.config/zlorb/configs
    Add,
    /// list repos currently watched by the daemon @ ~/.config/zlorb/configs
    List,
    /// report daemon and per-repo state; --json prints it as one JSON document
    Status {
        #[arg(long)]
        json: bool,
    },
    /// starts the repo watch daemon
    Start,
    /// sets up the daemon: config dir + systemd unit, then enables and starts it
    Setup {
        /// only write the config and unit files, and print the systemctl commands instead of running them
        #[arg(long)]
        print_only: bool,
    },
    /// runs a journalctl watcher to see realtime logs
    Watch,
    /// runs the zlorb-web binary to start a web server;
    ///
    /// This is being compiled as a dependency to ctl because the expectation
    /// is that if youre going to use the web ui you dont need to use the ctl
    Web,
}

fn main() -> Result<(), ZlorbError> {
    let args = Args::parse();

    let res = match args.cmd {
        Commands::Add => repo::add(),
        Commands::List => repo::list(),
        Commands::Status { json } => status::status(json),
        Commands::Start => DaemonManager::start(),
        Commands::Setup { print_only } => setup::setup(print_only),
        Commands::Remove { repo_name } => repo::remove(repo_name),
        Commands::Watch => repo::watch(),
        Commands::Web => {
            zlorb_web::run();
            Ok(())
        }
    };

    if let Err(e) = res {
        e.print();
        std::process::exit(1);
    }
    Ok(())
}
