use crate::interruptor::Interruptor;
use clap::Parser;
use probe_rs::probe::list::Lister;
use std::{
    net::{SocketAddr, TcpListener},
    sync::mpsc,
};
use tracing::{debug, error, info};

mod interruptor;
mod manager;
mod rtt_session;
mod server;
mod trc_command;

/// Proxy debug-probe operations and RTT data over the network
#[derive(Parser, Debug, Clone)]
#[clap(version)]
#[command(name = "trc-rtt-proxy")]
struct Opts {
    /// The proxy service will be available at this socket
    #[clap(
        long,
        name = "address",
        env = "TRC_RTT_PROXY_ADDRESS",
        default_value = "0.0.0.0:8888"
    )]
    pub address: SocketAddr,

    /// Log periodic RTT metrics
    #[clap(long, name = "metrics", env = "TRC_RTT_PROXY_METRICS")]
    pub metrics: bool,

    /// List available probes and exit
    #[clap(long, name = "list-probes")]
    pub list_probes: bool,
}

fn main() {
    match do_main() {
        Ok(()) => (),
        Err(e) => {
            error!("{e}");
            let mut cause = e.source();
            while let Some(err) = cause {
                error!("Caused by: {err}");
                cause = err.source();
            }
            std::process::exit(exitcode::SOFTWARE);
        }
    }
}

fn do_main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = Opts::parse();

    tracing_subscriber::fmt()
        .with_thread_ids(false)
        .with_thread_names(true)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .compact()
        .init();

    let (op_tx, op_rx) = mpsc::sync_channel(manager::Operation::OPERATION_CHANNEL_SIZE);

    let intr = Interruptor::new();
    let handler_op_tx = op_tx.clone();
    ctrlc::set_handler(move || {
        if intr.is_set() {
            let exit_code = if cfg!(target_family = "unix") {
                // 128 (fatal error signal "n") + 2 (control-c is fatal error signal 2)
                130
            } else {
                // Windows code 3221225786
                // -1073741510 == C000013A
                -1073741510
            };
            std::process::exit(exit_code);
        }

        info!("Requesting shutdown");
        handler_op_tx.try_send(manager::Operation::Shutdown).ok();
        intr.set();
    })?;

    // Log available probes on startup
    let lister = Lister::new();
    let probes = lister.list_all();
    if opts.list_probes {
        println!("Available probes ({}):", probes.len());
    }
    for probe in probes.into_iter() {
        debug!(%probe, "Found probe");
        if opts.list_probes {
            println!("  {}", probe);
        }
    }
    if opts.list_probes {
        return Ok(());
    }

    info!(address = %opts.address, "Starting server");
    let listener = TcpListener::bind(opts.address)?;

    let _server_handle = server::spawn(listener, op_tx.clone())?;

    let manager_handle = manager::spawn(opts.metrics, op_rx, op_tx)?;

    // Wait for the manager thread to finish
    manager_handle.join().ok();

    Ok(())
}
