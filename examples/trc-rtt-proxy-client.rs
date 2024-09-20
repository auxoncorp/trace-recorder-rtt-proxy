use clap::Parser;
use probe_rs::probe::{DebugProbeSelector, WireProtocol};
use rtt_proxy::{
    ProbeConfig, ProxySessionConfig, ProxySessionStatus, RttConfig, Target, TargetConfig,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{self, Read, Write},
    net::{SocketAddr, TcpStream},
    path::PathBuf,
};
use tracing::{debug, info, trace};
use url::Url;

/// Connects to an existing trace recorder RTT proxy server and writes RTT binary data to file
#[derive(Parser, Debug, Clone)]
#[clap(version)]
#[command(name = "trc-rtt-proxy-client")]
struct Opts {
    /// Specify a target attach timeout.
    /// When provided, the plugin will continually attempt to attach and search
    /// for a valid RTT control block anywhere in the target RAM.
    ///
    /// Accepts durations like "10ms" or "1minute 2seconds 22ms".
    #[clap(long, name = "attach-timeout")]
    pub attach_timeout: Option<humantime::Duration>,

    /// Use the provided RTT control block address instead of scanning the target memory for it.
    #[clap(long, name = "control-block-address", value_parser=clap_num::maybe_hex::<u64>)]
    pub control_block_address: Option<u64>,

    /// Select a specific probe instead of opening the first available one.
    ///
    /// Use '--probe VID:PID' or '--probe VID:PID:Serial' if you have more than one probe with the same VID:PID.
    #[structopt(long = "probe", name = "probe")]
    pub probe_selector: Option<DebugProbeSelector>,

    /// The target chip to attach to (e.g. STM32F407VE).
    ///
    /// Defaults to AUTO.
    #[clap(long, name = "target")]
    pub target: Option<String>,

    /// Protocol used to connect to chip.
    /// Possible options: [swd, jtag].
    ///
    /// The default value is swd.
    #[structopt(long, name = "protocol", default_value = "Swd")]
    pub protocol: WireProtocol,

    /// The protocol speed in kHz.
    ///
    /// The default value is 4000.
    #[clap(long, name = "speed", default_value = "4000")]
    pub speed: u32,

    /// The selected core to target.
    ///
    /// The default value is 0.
    #[clap(long, name = "core", default_value = "0")]
    pub core: u32,

    /// Reset the core on startup.
    #[clap(long, name = "reset")]
    pub reset: bool,

    /// Attach to the chip under hard-reset.
    #[clap(long, name = "attach-under-reset")]
    pub attach_under_reset: bool,

    /// Force exclusive access to the probe.
    /// Any existing sessions using this probe will be shut down.
    #[clap(long, name = "force-exclusive")]
    pub force_exclusive: bool,

    /// Set a breakpoint on the address of the given symbol when
    /// enabling RTT BlockIfFull channel mode.
    ///
    /// Can be an absolute address (decimal or hex) or symbol name.
    #[arg(long)]
    pub breakpoint: Option<String>,

    /// Set a breakpoint on the address of the given symbol
    /// to signal a stopping condition.
    ///
    /// Can be an absolute address (decimal or hex) or symbol name.
    #[arg(long)]
    pub stop_on_breakpoint: Option<String>,

    /// Assume thumb mode when resolving symbols from the ELF file
    /// for breakpoints.
    #[arg(long)]
    pub thumb: bool,

    /// The ELF file containing the RTT symbols
    #[clap(long, name = "elf-file")]
    pub elf_file: Option<PathBuf>,

    /// Disable sending control plane commands to the target.
    /// By default, CMD_SET_ACTIVE is sent on startup and shutdown to
    /// start and stop tracing on the target.
    #[clap(long, name = "disable-control-plane")]
    pub disable_control_plane: bool,

    /// Send a stop command before a start command to reset tracing on the target.
    #[clap(long, name = "restart", conflicts_with = "disable-control-plane")]
    pub restart: bool,

    /// The RTT up (target to host) channel number to poll on (defaults to 1).
    #[clap(long, name = "up-channel", default_value = "1")]
    pub up_channel: u32,

    /// The RTT down (host to target) channel number to send start/stop commands on (defaults to 1).
    #[clap(long, name = "down-channel", default_value = "1")]
    pub down_channel: u32,

    /// Size of the host-side RTT buffer used to store data read off the target.
    #[clap(long, name = "rtt-reader-buffer-size", default_value = "1024")]
    pub rtt_read_buffer_size: u32,

    /// The host-side RTT polling interval.
    ///
    /// Note that when the interface returns no data, we delay using
    /// the idle poll interval to prevent USB connection instability.
    ///
    /// Accepts durations like "10ms" or "1minute 2seconds 22ms".
    #[clap(long, name = "rtt-poll-interval", default_value = "1ms")]
    pub rtt_poll_interval: humantime::Duration,

    /// The host-side RTT idle polling interval.
    ///
    /// Accepts durations like "10ms" or "1minute 2seconds 22ms".
    #[clap(long, name = "rtt-idle-poll-interval", default_value = "100ms")]
    pub rtt_idle_poll_interval: humantime::Duration,

    /// The output file to write to
    #[clap(long, short = 'o', default_value = "rtt.psf")]
    pub output: PathBuf,

    /// The remote proxy service to connect to.
    #[clap(name = "address", default_value = "127.0.0.1:8888")]
    pub remote: String,
}

fn main() {
    match do_main() {
        Ok(()) => (),
        Err(e) => {
            eprintln!("{e}");
            let mut cause = e.source();
            while let Some(err) = cause {
                eprintln!("Caused by: {err}");
                cause = err.source();
            }
            std::process::exit(exitcode::SOFTWARE);
        }
    }
}

fn do_main() -> Result<(), Box<dyn std::error::Error>> {
    let opts = Opts::parse();

    tracing_subscriber::fmt::init();

    let remote = if let Ok(socket_addr) = opts.remote.parse::<SocketAddr>() {
        socket_addr
    } else {
        let url = Url::parse(&opts.remote)
            .map_err(|e| format!("Failed to parse remote '{}' as URL. {}", opts.remote, e))?;
        debug!(remote_url = %url);
        let socket_addrs = url
            .socket_addrs(|| None)
            .map_err(|e| format!("Failed to resolve remote URL '{}'. {}", url, e))?;
        *socket_addrs
            .first()
            .ok_or_else(|| format!("Could not resolve URL '{}'", url))?
    };

    info!(path = %opts.output.display(), "Creating output file");
    let mut file = File::create(&opts.output)?;

    let maybe_control_block_address = if let Some(user_provided_addr) = opts.control_block_address {
        info!(
            rtt_addr = user_provided_addr,
            "Using explicit RTT control block address"
        );
        Some(user_provided_addr)
    } else if let Some(elf_file) = &opts.elf_file {
        info!(elf_file = %elf_file.display(), "Reading ELF file");
        let mut file = File::open(elf_file)?;
        if let Some(rtt_addr) = get_rtt_symbol(&mut file) {
            info!(rtt_addr = rtt_addr, "Found RTT symbol");
            Some(rtt_addr)
        } else {
            info!("Could not find RTT symbol in ELF file");
            None
        }
    } else {
        None
    };

    let maybe_setup_on_breakpoint_address = if let Some(bp_sym_or_addr) = &opts.breakpoint {
        if let Some(bp_addr) = bp_sym_or_addr.parse::<u64>().ok().or(u64::from_str_radix(
            bp_sym_or_addr.trim_start_matches("0x"),
            16,
        )
        .ok())
        {
            Some(bp_addr)
        } else {
            let elf_file = opts
                .elf_file
                .as_ref()
                .ok_or_else(|| "Using a breakpoint symbol name requires an ELF file".to_owned())?;
            let mut file = File::open(elf_file)?;
            let bp_addr = get_symbol(&mut file, bp_sym_or_addr).unwrap();
            if opts.thumb {
                Some(bp_addr & !1)
            } else {
                Some(bp_addr)
            }
        }
    } else {
        None
    };

    let maybe_stop_on_breakpoint_address = if let Some(bp_sym_or_addr) = &opts.stop_on_breakpoint {
        if let Some(bp_addr) = bp_sym_or_addr.parse::<u64>().ok().or(u64::from_str_radix(
            bp_sym_or_addr.trim_start_matches("0x"),
            16,
        )
        .ok())
        {
            Some(bp_addr)
        } else {
            let elf_file = opts
                .elf_file
                .as_ref()
                .ok_or_else(|| "Using a breakpoint symbol name requires an ELF file".to_owned())?;
            let mut file = File::open(elf_file)?;
            let bp_addr = get_symbol(&mut file, bp_sym_or_addr).unwrap();
            if opts.thumb {
                Some(bp_addr & !1)
            } else {
                Some(bp_addr)
            }
        }
    } else {
        None
    };

    let cfg = ProxySessionConfig {
        version: rtt_proxy::V1,
        probe: ProbeConfig {
            probe_selector: opts.probe_selector.map(|s| s.to_string()),
            protocol: opts.protocol.to_string(),
            speed_khz: opts.speed,
            target: opts.target.map(Target::Specific).unwrap_or(Target::Auto),
            attach_under_reset: opts.attach_under_reset,
            force_exclusive: opts.force_exclusive,
        },
        target: TargetConfig {
            core: opts.core,
            reset: opts.reset,
        },
        rtt: RttConfig {
            attach_timeout_ms: opts.attach_timeout.map(|t| t.as_millis() as _),
            setup_on_breakpoint_address: maybe_setup_on_breakpoint_address,
            stop_on_breakpoint_address: maybe_stop_on_breakpoint_address,
            control_block_address: maybe_control_block_address,
            up_channel: opts.up_channel,
            down_channel: opts.down_channel,
            disable_control_plane: opts.disable_control_plane,
            restart: opts.restart,
            rtt_read_buffer_size: opts.rtt_read_buffer_size,
            rtt_poll_interval_ms: opts.rtt_poll_interval.as_millis() as _,
            rtt_idle_poll_interval_ms: opts.rtt_idle_poll_interval.as_millis() as _,
        },
    };
    trace!(?cfg);

    let mut buffer = vec![0_u8; 2 * cfg.rtt.rtt_read_buffer_size as usize];

    info!(remote = %opts.remote, "Connecting to server");
    let mut sock = TcpStream::connect(remote)?;

    // Send session config
    info!("Starting a new session");
    let mut se = serde_json::Serializer::new(&mut sock);
    cfg.serialize(&mut se)?;

    // Read response
    let mut de = serde_json::Deserializer::from_reader(&mut sock);
    let status = ProxySessionStatus::deserialize(&mut de)?;

    match status {
        ProxySessionStatus::Started(id) => info!(%id, "Session started"),
        ProxySessionStatus::Error(e) => return Err(e.into()),
    }

    loop {
        let bytes_recvd = sock.read(&mut buffer)?;
        if bytes_recvd == 0 {
            break;
        }
        debug!(bytes_recvd, "Writting data to file");
        file.write_all(&buffer[..bytes_recvd])?;
    }

    info!("Shutting down");

    file.flush()?;

    Ok(())
}

fn get_rtt_symbol<T: io::Read + io::Seek>(stream: &mut T) -> Option<u64> {
    get_symbol(stream, "_SEGGER_RTT")
}

fn get_symbol<T: io::Read + io::Seek>(stream: &mut T, symbol: &str) -> Option<u64> {
    let mut buffer = Vec::new();
    if stream.read_to_end(&mut buffer).is_ok() {
        if let Ok(binary) = goblin::elf::Elf::parse(buffer.as_slice()) {
            for sym in &binary.syms {
                if let Some(name) = binary.strtab.get_at(sym.st_name) {
                    if name == symbol {
                        return Some(sym.st_value);
                    }
                }
            }
        }
    }
    None
}
