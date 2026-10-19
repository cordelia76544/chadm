//! Phase-6 host validation helper, not a new supported chadm CLI resource.
use chadm::{config::HostConfig, instance, network};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
struct Args {
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    #[command(subcommand)]
    action: Action,
}
#[derive(Subcommand)]
enum Action {
    Check {
        bridge: String,
    },
    Prepare {
        instance: String,
    },
    Cleanup {
        instance: String,
    },
    Observe {
        instance: String,
        #[arg(long, default_value_t = 0)]
        index: usize,
    },
}
fn run(args: Args) -> chadm::error::Result<()> {
    match args.action {
        Action::Check { bridge } => {
            println!("{bridge}: {:?}", network::bridge_status(&bridge)?);
        }
        action => {
            let host = HostConfig::load(args.config.as_deref())?;
            match action {
                Action::Prepare { instance } => network::prepare(&host, &instance)?,
                Action::Cleanup { instance } => network::cleanup(&host, &instance)?,
                Action::Observe {
                    instance: name,
                    index,
                } => {
                    let info = instance::show(&host, &name)?;
                    println!("{:?}", network::observe(&info.config, index)?);
                }
                Action::Check { .. } => (),
            }
        }
    }
    Ok(())
}
fn main() -> std::process::ExitCode {
    match run(Args::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
