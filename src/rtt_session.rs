use crate::{interruptor::Interruptor, manager::Operation, trc_command::TrcCommand};
use parking_lot::FairMutex;
use probe_rs::{
    rtt::{Rtt, ScanRegion},
    Core, CoreStatus, HaltReason, RegisterValue, Session, VectorCatchCondition,
};
use rtt_proxy::{ProbeConfig, ProxySessionId, ProxySessionStatus, RttConfig, TargetConfig};
use serde::Serialize;
use simple_moving_average::{NoSumSMA, SMA};
use std::{
    io::{self, Write},
    net::TcpStream,
    sync::mpsc,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};
use tracing::{debug, error, info, trace, warn};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("RTT session IO error. {0}")]
    Io(#[from] io::Error),

    #[error("RTT session JSON response error. {0}")]
    ResponseJson(#[from] serde_json::Error),

    #[error("Encountered an error with the debug probe. {0}")]
    Probe(#[from] probe_rs::Error),

    #[error("Encountered an RTT operation error. {0}")]
    Rtt(#[from] probe_rs::rtt::Error),

    #[error("The RTT down channel ({0}) is invalid")]
    DownChannelInvalid(usize),

    #[error("The RTT up channel ({0}) is invalid")]
    UpChannelInvalid(usize),

    #[error("The RTT session was requested to shutdown while initializing")]
    ShutdownRequestedWhileInitializing,
}

pub type JoinHandle = thread::JoinHandle<Result<(), Error>>;

#[derive(Debug)]
pub struct SpawnArgs {
    pub thread_name: String,
    pub proxy_session_id: ProxySessionId,
    pub probe_cfg: ProbeConfig,
    pub target_cfg: TargetConfig,
    pub rtt_cfg: RttConfig,
    pub log_rtt_metrics: bool,
    pub recovery_mode: bool,
    pub response_already_sent: bool,
    pub interruptor: Interruptor,
    pub shutdown_channel: mpsc::SyncSender<Operation>,
    pub session: Arc<FairMutex<Session>>,
    pub stream: TcpStream,
}

#[derive(Debug)]
pub struct RecoveryState {
    pub proxy_session_id: ProxySessionId,
    pub probe_cfg: ProbeConfig,
    pub target_cfg: TargetConfig,
    pub rtt_cfg: RttConfig,
    pub response_already_sent: bool,
    pub stream: TcpStream,
}

impl RecoveryState {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            proxy_session_id: self.proxy_session_id,
            probe_cfg: self.probe_cfg.clone(),
            target_cfg: self.target_cfg.clone(),
            rtt_cfg: self.rtt_cfg.clone(),
            response_already_sent: self.response_already_sent,
            stream: self.stream.try_clone()?,
        })
    }
}

pub fn spawn(args: SpawnArgs) -> io::Result<JoinHandle> {
    let SpawnArgs {
        thread_name,
        proxy_session_id,
        mut probe_cfg,
        mut target_cfg,
        mut rtt_cfg,
        log_rtt_metrics,
        recovery_mode,
        response_already_sent,
        interruptor,
        shutdown_channel,
        session,
        stream,
    } = args;

    let atomic_response_already_sent = ResponseSentState::new();
    if response_already_sent {
        atomic_response_already_sent.set();
    }

    // In recovery mode we disable hardware-level stateful configs
    if recovery_mode {
        // Disable exclusive probe access
        probe_cfg.force_exclusive = false;

        // If we got here, then attach-under-reset has worked at least once.
        // Don't reset on further probe attaches.
        probe_cfg.attach_under_reset = false;

        // Don't reset the core
        target_cfg.reset = false;

        // Disable breakpoints, likely will miss them anyhow
        rtt_cfg.setup_on_breakpoint_address = None;
        rtt_cfg.stop_on_breakpoint_address = None;
    }

    let cfg = Config {
        proxy_session_id,
        target_cfg: target_cfg.clone(),
        rtt_cfg: rtt_cfg.clone(),
        log_rtt_metrics,
        recovery_mode,
        response_already_sent: atomic_response_already_sent.clone(),
        interruptor,
        session,
        stream: stream.try_clone()?,
    };

    let builder = thread::Builder::new().name(thread_name);
    builder.spawn(move || {
        let res = rtt_session_thread(cfg);
        if let Err(e) = &res {
            warn!(error = %e, "RTT session returned an error");

            if target_cfg.auto_recover {
                // TODO cfg or recovery thread with time handling
                thread::sleep(Duration::from_millis(100));

                shutdown_channel
                    .send(Operation::RecoverSession(RecoveryState {
                        proxy_session_id,
                        probe_cfg,
                        target_cfg,
                        rtt_cfg,
                        response_already_sent: atomic_response_already_sent.is_set(),
                        stream,
                    }))
                    .ok();
                return res;
            }
        }

        shutdown_channel.send(Operation::PruneInactiveSessions).ok();

        res
    })
}

#[derive(Debug)]
struct Config {
    proxy_session_id: ProxySessionId,
    target_cfg: TargetConfig,
    rtt_cfg: RttConfig,
    log_rtt_metrics: bool,
    recovery_mode: bool,
    response_already_sent: ResponseSentState,
    interruptor: Interruptor,
    session: Arc<FairMutex<Session>>,
    stream: TcpStream,
}

// Just re-using the atomic bool semantics of Interruptor
type ResponseSentState = Interruptor;

fn rtt_session_thread(cfg: Config) -> Result<(), Error> {
    let Config {
        proxy_session_id,
        target_cfg,
        rtt_cfg,
        log_rtt_metrics,
        recovery_mode,
        response_already_sent,
        interruptor,
        session,
        mut stream,
    } = cfg;
    let session_mutex = session;

    info!(id = %proxy_session_id, recovery_mode, "Starting RTT session");

    let buf_size = if rtt_cfg.rtt_read_buffer_size < 64 {
        RttConfig::DEFAULT_RTT_BUFFER_SIZE as usize
    } else {
        rtt_cfg.rtt_read_buffer_size as usize
    };
    let poll_interval = Duration::from_millis(rtt_cfg.rtt_poll_interval_ms as _);
    let idle_poll_interval = if rtt_cfg.rtt_idle_poll_interval_ms == 0 {
        Duration::from_millis(1)
    } else {
        Duration::from_millis(rtt_cfg.rtt_idle_poll_interval_ms as _)
    };
    // Only check core status every 100ms, based on the idle interval
    let on_stop_breakpoint_limit = if rtt_cfg.rtt_idle_poll_interval_ms == 0 {
        100_u32
    } else {
        std::cmp::max(100 / rtt_cfg.rtt_idle_poll_interval_ms, 1)
    };
    let no_data_stop_timeout_duration = rtt_cfg
        .no_data_stop_timeout_ms
        .map(|ms| Duration::from_millis(std::cmp::max(1, ms as _)));

    let mut host_buffer = vec![0_u8; buf_size];
    let mut metrics = Metrics::new(host_buffer.len());

    // Get a lock on the session while we do setup
    let mut session = session_mutex.lock();

    let rtt_scan_region = if let Some(control_block_addr) = rtt_cfg.control_block_address {
        debug!(
            control_block_addr = format_args!("0x{:X}", control_block_addr),
            "Using explicit RTT control block address"
        );
        ScanRegion::Exact(control_block_addr)
    } else {
        session.target().rtt_scan_regions.clone()
    };

    let mut core = session.core(target_cfg.core as _)?;
    let core_status = core.status()?;
    debug!(?core_status);

    if target_cfg.reset {
        debug!("Reset and halt core");
        // This is what probe-rs does
        core.reset_and_halt(Duration::from_millis(100))?;

        let sp_reg = core.stack_pointer();
        let sp: RegisterValue = core.read_core_reg(sp_reg.id())?;
        let pc_reg = core.program_counter();
        let pc: RegisterValue = core.read_core_reg(pc_reg.id())?;
        debug!(pc = %pc, sp = %sp);
    }

    // Disable any previous vector catching (i.e. user just ran probe-rs run or a debugger)
    core.disable_vector_catch(VectorCatchCondition::All)?;
    core.clear_all_hw_breakpoints()?;

    // Set the breakpoint for setup
    if let Some(bp_addr) = rtt_cfg.setup_on_breakpoint_address {
        let num_bp = core.available_breakpoint_units()?;
        debug!(
            available_breakpoints = num_bp,
            addr = format_args!("0x{:X}", bp_addr),
            "Setting breakpoint to do RTT channel setup"
        );
        core.set_hw_breakpoint(bp_addr)?;
    }

    // Start the core if it's halted
    if target_cfg.reset || !matches!(core_status, CoreStatus::Running) {
        let sp_reg = core.stack_pointer();
        let sp: RegisterValue = core.read_core_reg(sp_reg.id())?;
        let pc_reg = core.program_counter();
        let pc: RegisterValue = core.read_core_reg(pc_reg.id())?;
        debug!(pc = %pc, sp = %sp, "Run core");
        core.run()?;
    }

    if let Some(bp_addr) = rtt_cfg.setup_on_breakpoint_address {
        debug!("Waiting for breakpoint");
        'bp_loop: loop {
            if interruptor.is_set() {
                break;
            }

            let core_status = core.status()?;

            match core_status {
                CoreStatus::Running => (),
                CoreStatus::Halted(halt_reason) => match halt_reason {
                    HaltReason::Breakpoint(_) => {
                        let sp_reg = core.stack_pointer();
                        let sp: RegisterValue = core.read_core_reg(sp_reg.id())?;
                        let pc_reg = core.program_counter();
                        let pc: RegisterValue = core.read_core_reg(pc_reg.id())?;
                        debug!(pc = %pc, sp = %sp, "Breakpoint hit");
                        break 'bp_loop;
                    }
                    _ => {
                        warn!(reason = ?halt_reason, "Unexpected halt reason");
                        break 'bp_loop;
                    }
                },
                state => {
                    warn!(state = ?state, "Core is in an unexpected state");
                    break 'bp_loop;
                }
            }

            thread::sleep(Duration::from_millis(100));
        }

        debug!("Clear breakpoint after setup post-hit");
        core.clear_hw_breakpoint(bp_addr)?;

        // The core is run below
    }

    // Set the breakpoint for on-stop
    if let Some(bp_addr) = rtt_cfg.stop_on_breakpoint_address {
        let num_bp = core.available_breakpoint_units()?;
        debug!(
            available_breakpoints = num_bp,
            addr = format_args!("0x{:X}", bp_addr),
            check_interval = on_stop_breakpoint_limit,
            "Setting breakpoint for stopping condition"
        );
        core.set_hw_breakpoint(bp_addr)?;
    }

    let rtt = if let Some(to) = rtt_cfg.attach_timeout_ms {
        attach_retry_loop(
            &mut core,
            &rtt_scan_region,
            &interruptor,
            Duration::from_millis(to.into()),
        )?
    } else {
        debug!("Attaching to RTT");
        Rtt::attach_region(&mut core, &rtt_scan_region)?
    };
    debug!(
        addr = format_args!("0x{:X}", rtt.ptr()),
        "Found RTT control block"
    );

    let up_channel = rtt
        .up_channel(rtt_cfg.up_channel as _)
        .ok_or(Error::UpChannelInvalid(rtt_cfg.up_channel as _))?;
    let up_channel_mode = up_channel.mode(&mut core)?;
    debug!(channel = up_channel.number(), mode = ?up_channel_mode, buffer_size = up_channel.buffer_size(), "Opened up channel");
    let down_channel = rtt
        .down_channel(rtt_cfg.down_channel as _)
        .ok_or(Error::DownChannelInvalid(rtt_cfg.down_channel as _))?;
    debug!(
        channel = down_channel.number(),
        buffer_size = down_channel.buffer_size(),
        "Opened down channel"
    );

    // Send the client a success response once we're pretty sure things are
    // working if we haven't already
    if !response_already_sent.is_set() {
        debug!("Sending client response");
        let resp = ProxySessionStatus::session_started(proxy_session_id);
        let mut se = serde_json::Serializer::new(&mut stream);
        resp.serialize(&mut se)?;
        response_already_sent.set();
    }

    // We've done the initial setup, release the lock and switch over to on-demand sessions
    std::mem::drop(core);
    std::mem::drop(session);

    session_op(&session_mutex, |session| {
        let mut core = session.core(target_cfg.core as _)?;

        if !rtt_cfg.disable_control_plane {
            if rtt_cfg.restart {
                debug!("Sending stop command");
                let cmd = TrcCommand::StopTracing.to_wire_bytes();
                down_channel.write(&mut core, &cmd)?;
                thread::sleep(Duration::from_millis(10));
            }

            debug!("Sending start command");
            let cmd = TrcCommand::StartTracing.to_wire_bytes();
            down_channel.write(&mut core, &cmd)?;
        }

        // Run the core if we hit the breakpoint
        if rtt_cfg.setup_on_breakpoint_address.is_some() {
            debug!("Run core post breakpoint");
            core.run()?;
        }
        Ok(())
    })?;

    let mut last_nonzero_read = Instant::now();
    let mut zero_counter = 0_u32;
    let mut halted_on_breakpoint_addr = None;
    while !interruptor.is_set() {
        let rtt_bytes_read = session_op(&session_mutex, |session| {
            let mut core = session.core(target_cfg.core as _)?;
            Ok(up_channel.read(&mut core, &mut host_buffer)?)
        })?;

        if rtt_bytes_read != 0 {
            trace!(bytes = rtt_bytes_read, "Writing RTT data");

            if let Err(e) = stream.write_all(&host_buffer[..rtt_bytes_read]) {
                info!(error = %e, "Client disconnected");
                break;
            }

            zero_counter = 0;
            last_nonzero_read = Instant::now();
        } else {
            // No data

            // Check for no-data on-stop timeout
            if let Some(timeout) = no_data_stop_timeout_duration {
                if Instant::now().duration_since(last_nonzero_read) >= timeout {
                    debug!(timeout = ?timeout, "Stopping due to no-data timeout");
                    break;
                }
            }

            // Check for on-stop breakpoint
            if let Some(bp_addr) = rtt_cfg.stop_on_breakpoint_address {
                zero_counter = zero_counter.saturating_add(1);

                // No RTT bytes, check core status if we're configured to stop on
                // a breakpoint
                if zero_counter >= on_stop_breakpoint_limit {
                    zero_counter = 0;

                    let core_status = session_op(&session_mutex, |session| {
                        let mut core = session.core(target_cfg.core as _)?;
                        let status = core.status()?;
                        Ok(status)
                    })?;

                    match core_status {
                        CoreStatus::Running => (),
                        CoreStatus::Halted(halt_reason) => match halt_reason {
                            HaltReason::Breakpoint(_) => {
                                session_op(&session_mutex, |session| {
                                    let mut core = session.core(target_cfg.core as _)?;
                                    let sp_reg = core.stack_pointer();
                                    let sp: RegisterValue = core.read_core_reg(sp_reg.id())?;
                                    let pc_reg = core.program_counter();
                                    let pc: RegisterValue = core.read_core_reg(pc_reg.id())?;
                                    debug!(pc = %pc, sp = %sp, "On-stop breakpoint hit");
                                    Ok(())
                                })?;
                                halted_on_breakpoint_addr = Some(bp_addr);
                                break;
                            }
                            _ => {
                                warn!(reason = ?halt_reason, "Unexpected halt reason");
                                halted_on_breakpoint_addr = Some(bp_addr);
                                break;
                            }
                        },
                        state => {
                            warn!(state = ?state, "Core is in an unexpected state");
                            halted_on_breakpoint_addr = Some(bp_addr);
                            break;
                        }
                    }
                }
            }
        }

        // This is more-or-less what probe-rs does.
        // If the polling frequency is too high, the USB connection to the probe
        // can become unstable.
        if rtt_bytes_read != 0 {
            thread::sleep(poll_interval);
        } else {
            thread::sleep(idle_poll_interval);
        }

        if log_rtt_metrics {
            metrics.update(rtt_bytes_read);
        }
    }

    info!("Shutting down");

    if !rtt_cfg.disable_control_plane {
        debug!("Sending stop command");
        session_op(&session_mutex, |session| {
            let mut core = session.core(target_cfg.core as _)?;
            let cmd = TrcCommand::StopTracing.to_wire_bytes();
            Ok(down_channel.write(&mut core, &cmd)?)
        })?;
    }

    if let Some(bp_addr) = halted_on_breakpoint_addr {
        debug!("Resume core after on-stop breakpoint");
        session_op(&session_mutex, |session| {
            let mut core = session.core(target_cfg.core as _)?;
            if let Err(e) = core.clear_hw_breakpoint(bp_addr) {
                warn!(
                    addr = format_args!("0x{:X}", bp_addr),
                    error = %e,
                    "Failed to clear hardware breakpoint"
                );
            }
            core.run()?;
            Ok(())
        })?;
    }

    Ok(())
}

fn attach_retry_loop(
    core: &mut Core,
    scan_region: &ScanRegion,
    interruptor: &Interruptor,
    timeout: Duration,
) -> Result<Rtt, Error> {
    debug!(?timeout, "Attaching to RTT");
    let start = Instant::now();
    while Instant::now().duration_since(start) <= timeout {
        if interruptor.is_set() {
            return Err(Error::ShutdownRequestedWhileInitializing);
        }

        match Rtt::attach_region(core, scan_region) {
            Ok(rtt) => return Ok(rtt),
            Err(e) => {
                if matches!(e, probe_rs::rtt::Error::ControlBlockNotFound) {
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }

                error!(error = %e, "Failed to attach to RTT");
                return Err(e.into());
            }
        }
    }

    // Timeout reached
    warn!("Timed out attaching to RTT");
    Ok(Rtt::attach(core)?)
}

fn session_op<F, T>(session_mutex: &Arc<FairMutex<Session>>, mut f: F) -> Result<T, Error>
where
    F: FnMut(&mut Session) -> Result<T, Error>,
{
    use std::ops::DerefMut;
    let mut session = session_mutex.lock();
    f(session.deref_mut())
}

struct Metrics {
    rtt_buffer_size: u64,
    window_start: Instant,
    read_cnt: u64,
    bytes_read: u64,
    read_zero_cnt: u64,
    read_max_cnt: u64,
    sma: NoSumSMA<f64, f64, 8>,
}

impl Metrics {
    const WINDOW_DURATION: Duration = Duration::from_secs(2);

    fn new(host_rtt_buffer_size: usize) -> Self {
        Self {
            rtt_buffer_size: host_rtt_buffer_size as u64,
            window_start: Instant::now(),
            read_cnt: 0,
            bytes_read: 0,
            read_zero_cnt: 0,
            read_max_cnt: 0,
            sma: NoSumSMA::new(),
        }
    }

    fn reset(&mut self) {
        self.read_cnt = 0;
        self.bytes_read = 0;
        self.read_zero_cnt = 0;
        self.read_max_cnt = 0;

        self.window_start = Instant::now();
    }

    fn update(&mut self, bytes_read: usize) {
        let dur = Instant::now().duration_since(self.window_start);

        self.read_cnt += 1;
        self.bytes_read += bytes_read as u64;
        if bytes_read == 0 {
            self.read_zero_cnt += 1;
        } else {
            if bytes_read as u64 == self.rtt_buffer_size {
                self.read_max_cnt += 1;
            }
            self.sma.add_sample(bytes_read as f64);
        }

        if dur >= Self::WINDOW_DURATION {
            let bytes = self.bytes_read as f64;
            let secs = dur.as_secs_f64();

            info!(
                transfer_rate = format_args!("{}/s", human_bytes::human_bytes(bytes / secs)),
                cnt = self.read_cnt,
                zero_cnt = self.read_zero_cnt,
                max_cnt = self.read_max_cnt,
                avg = self.sma.get_average(),
            );

            self.reset();
        }
    }
}
