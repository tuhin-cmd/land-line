//! `landline`: drives the core headlessly, so protocol work can be tested
//! against the real phone without touching Swift.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use clap::{Parser, Subcommand};
use landline_core::{Config, Core, CoreDelegate};

#[derive(Parser)]
#[command(
    name = "landline",
    version,
    about = "Headless driver for the Landline core"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the core version.
    Version,
    /// Start the core and print its heartbeats (M0 smoke test).
    Heartbeat {
        /// Stop after this many heartbeats.
        #[arg(long, default_value_t = 3)]
        count: u64,
        /// Milliseconds between heartbeats.
        #[arg(long, default_value_t = 1000)]
        interval_ms: u64,
    },
}

struct Printer(mpsc::Sender<u64>);

impl CoreDelegate for Printer {
    fn heartbeat(&self, count: u64) {
        let _ = self.0.send(count);
    }
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("landline: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::Version => println!("{}", landline_core::version()),
        Command::Heartbeat { count, interval_ms } => {
            let (tx, rx) = mpsc::channel();
            let config = Config {
                heartbeat_interval: Duration::from_millis(interval_ms.max(1)),
            };
            let core = Core::new(config, Arc::new(Printer(tx)))?;
            core.start();
            for beat in rx.iter().take(usize::try_from(count)?) {
                println!("heartbeat {beat}");
            }
            core.stop();
        }
    }
    Ok(())
}
