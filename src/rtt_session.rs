use crate::{
    manager::Operation, rate_limiter::RateLimiter, shared_state::SharedStateRc,
    trc_command::TrcCommand,
};
use crossbeam_channel::Sender;
use probe_rs::{
    rtt::{DownChannel, Rtt, ScanRegion, UpChannel},
    CoreStatus, HaltReason, RegisterValue, VectorCatchCondition,
};
use rtt_proxy::{ProxySessionControl, ProxySessionId, ProxySessionStatus, RttConfig, TargetConfig};
use serde::Deserialize;
use serde::Serialize;
use simple_moving_average::{NoSumSMA, SMA};
use std::{
    fmt,
    io::{self, Write},
    net::TcpStream,
    thread,
    time::{Duration, Instant},
};
use tracing::{debug, debug_span, error, info, info_span, trace, trace_span, warn};

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

    #[error("The core ({0}) isn't running")]
    CoreNotRunning(u32),

    #[error("Core status check failed. {0}")]
    CoreStatus(probe_rs::Error),

    // This is the mostly likely error when target power is lost
    // and we need to do debug probe recovery
    #[error("Failed to attach core. {0}")]
    CoreAttach(probe_rs::Error),

    #[error("Client disconnected")]
    ClientDisconnected,
}

#[derive(Debug)]
pub enum RttSessionState {
    /// Need to perform the core setup routine
    Init,
    /// Running the RTT attach routine
    RttAttach(Instant),
    /// Attached to RTT and actively reading data
    Run(Box<RttHandles>),
    /// Stopping conditions were reached, but can be restarted (i.e. after a power cycle or reset)
    Stopped(StoppedReason),
    /// Shutdown down and waiting to be culled, will never come back
    Shutdown,
}

#[derive(Debug)]
pub struct RttSession {
    id: ProxySessionId,
    name: String,
    target_cfg: TargetConfig,
    rtt_cfg: RttConfig,
    log_rtt_metrics: bool,
    client_response_sent: bool,
    shared_state: SharedStateRc,
    stream: TcpStream,
    state: RttSessionState,
    host_buffer: Vec<u8>,
    metrics: Metrics,
    rtt_scan_region: ScanRegion,
    last_rtt_read_had_data: bool,
    rtt_poll_limiter: RateLimiter,
    idle_poll_limiter: RateLimiter,
    recovery_limiter: RateLimiter,
    attach_retry_limiter: RateLimiter,
    stopping_condition_limiter: RateLimiter,
}

impl RttSession {
    const STOPPING_CONDITION_CHECK_INTERVAL: Duration = Duration::from_millis(50);
    const RECOVERY_CHECK_INTERVAL: Duration = Duration::from_millis(50);
    const ATTACH_RETRY_INTERVAL: Duration = Duration::from_millis(20);

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ProxySessionId,
        name: String,
        target_cfg: TargetConfig,
        rtt_cfg: RttConfig,
        log_rtt_metrics: bool,
        shared_state: SharedStateRc,
        op_channel: Sender<Operation>,
        stream: TcpStream,
        session: &mut probe_rs::Session,
    ) -> io::Result<Self> {
        let _jh = spawn_control_thread(
            id,
            format!("{}:control", name),
            op_channel,
            stream.try_clone()?,
        )?;

        let _span = debug_span!("RTT session setup", name).entered();
        debug!(%id);

        let buf_size = if rtt_cfg.rtt_read_buffer_size < 64 {
            RttConfig::DEFAULT_RTT_BUFFER_SIZE as usize
        } else {
            rtt_cfg.rtt_read_buffer_size as usize
        };

        let rtt_poll_limiter = RateLimiter::new(Duration::from_millis(std::cmp::max(
            1,
            rtt_cfg.rtt_poll_interval_ms,
        ) as _));

        let idle_poll_limiter = RateLimiter::new(Duration::from_millis(std::cmp::max(
            1,
            rtt_cfg.rtt_idle_poll_interval_ms,
        ) as _));

        let recovery_limiter = RateLimiter::new(Self::RECOVERY_CHECK_INTERVAL);

        let attach_retry_limiter = RateLimiter::new(Self::ATTACH_RETRY_INTERVAL);

        let stopping_condition_limiter = RateLimiter::new(Self::STOPPING_CONDITION_CHECK_INTERVAL);

        let host_buffer = vec![0_u8; buf_size];
        let metrics = Metrics::new(host_buffer.len());

        let rtt_scan_region = if let Some(control_block_addr) = rtt_cfg.control_block_address {
            debug!(
                control_block_addr = format_args!("0x{:X}", control_block_addr),
                "Using explicit RTT control block address"
            );
            ScanRegion::Exact(control_block_addr)
        } else {
            session.target().rtt_scan_regions.clone()
        };

        Ok(Self {
            id,
            name,
            target_cfg,
            rtt_cfg,
            log_rtt_metrics,
            client_response_sent: false,
            shared_state,
            stream,
            state: RttSessionState::Init,
            host_buffer,
            metrics,
            rtt_scan_region,
            last_rtt_read_had_data: true,
            rtt_poll_limiter,
            idle_poll_limiter,
            recovery_limiter,
            attach_retry_limiter,
            stopping_condition_limiter,
        })
    }

    pub fn id(&self) -> ProxySessionId {
        self.id
    }

    pub fn target_config(&self) -> &TargetConfig {
        &self.target_cfg
    }

    pub fn rtt_config(&self) -> &RttConfig {
        &self.rtt_cfg
    }

    pub fn is_shutdown(&self) -> bool {
        matches!(self.state, RttSessionState::Shutdown)
    }

    pub fn is_running(&self) -> bool {
        matches!(self.state, RttSessionState::Run(_))
    }

    pub fn is_stopped(&self) -> bool {
        matches!(self.state, RttSessionState::Stopped(_))
    }

    pub fn is_stopped_due_to_core_attach(&self) -> bool {
        matches!(
            self.state,
            RttSessionState::Stopped(StoppedReason::Error(Error::CoreAttach(_)))
        )
    }

    pub fn shutdown(&mut self, session: &mut probe_rs::Session) {
        if !matches!(self.state, RttSessionState::Shutdown) {
            let _span = info_span!("RTT session shutdown", name = self.name).entered();

            // Do normal stop routine
            self.stop(session, StoppedReason::Shutdown);

            // Drop the client connection
            self.stream.shutdown(std::net::Shutdown::Both).ok();

            self.transition_state(RttSessionState::Shutdown);
        }
    }

    fn stop(&mut self, session: &mut probe_rs::Session, reason: StoppedReason) {
        if !matches!(
            self.state,
            RttSessionState::Stopped(_) | RttSessionState::Shutdown
        ) {
            let _ = self.shared_state.dec_clear_vector_catch_and_breakpoints();
            let disable_vector_catch = self.shared_state.dec_vector_catch_enabled();
            self.last_rtt_read_had_data = true;

            if let Ok(mut core) = session.core(self.target_cfg.core as _) {
                // Only disable vector catches if we're the last session to do so
                if disable_vector_catch {
                    debug!("Disabling vector catch");
                    core.disable_vector_catch(VectorCatchCondition::All).ok();
                }

                // Send the stop command if we can
                if let RttSessionState::Run(rtt_handles) = &self.state {
                    if !self.rtt_cfg.disable_control_plane {
                        debug!("Sending stop command");
                        let cmd = TrcCommand::StopTracing.to_wire_bytes();
                        rtt_handles.down_channel.write(&mut core, &cmd).ok();
                    }
                }

                // Clear breakpoints
                if let Some(bp_addr) = self.rtt_cfg.stop_on_breakpoint_address {
                    if let Err(e) = core.clear_hw_breakpoint(bp_addr) {
                        warn!(
                            addr = format_args!("0x{:X}", bp_addr),
                            error = %e,
                            "Failed to clear hardware breakpoint"
                        );
                    }
                }

                if matches!(
                    reason,
                    StoppedReason::Breakpoint(_) | StoppedReason::ResetCatch
                ) {
                    debug!("Resume core after stopping condition");
                    // Run the core if we're halted due to breakpoint or reset
                    if core.run().is_err() {
                        warn!("Failed to resume core");
                    }
                }
            }

            self.transition_state(RttSessionState::Stopped(reason));
        }
    }

    // Manager calls this each scheduled iteration, regardless of state
    pub fn update(&mut self, session: &mut probe_rs::Session) {
        // Shutdown state is non-recoverable
        if self.is_shutdown() {
            return;
        }

        let _span = match &self.state {
            RttSessionState::Init => trace_span!("RTT init", name = self.name).entered(),
            RttSessionState::RttAttach(_) => trace_span!("RTT attach", name = self.name).entered(),
            RttSessionState::Run(_) => trace_span!("RTT run", name = self.name).entered(),
            RttSessionState::Shutdown => trace_span!("Shutdown", name = self.name).entered(),
            RttSessionState::Stopped(_) => trace_span!("Stopped", name = self.name).entered(),
        };

        // Attempt to recover if we're configured to do so
        if self.is_stopped() && self.target_cfg.auto_recover {
            // Rate limit the recovery attemps
            if self.recovery_limiter.check() {
                debug!("Attempting to check core status for recovery");
                match session.core(self.target_cfg.core as _) {
                    Ok(mut core) => {
                        if let Ok(core_status) = core.status() {
                            debug!(core = self.target_cfg.core, ?core_status, "Core recovery");
                            self.transition_state(RttSessionState::Init);
                        }
                    }
                    Err(e) => {
                        // We're already in the stopped state, update the error reason
                        self.state =
                            RttSessionState::Stopped(StoppedReason::Error(Error::CoreAttach(e)));
                    }
                }
            }

            // We'll try again on the next cycle
        } else {
            // Update state
            if let Err(e) = self.update_inner(session) {
                warn!(error = %e);
                if self.target_cfg.auto_recover && !matches!(e, Error::ClientDisconnected) {
                    self.stop(session, StoppedReason::Error(e));
                } else {
                    self.shutdown(session);
                }
            }
        }
    }

    fn transition_state(&mut self, new_state: RttSessionState) {
        debug!(
            prev_state = %self.state,
            state = %new_state,
            "Transition state"
        );
        self.state = new_state;
    }

    fn update_inner(&mut self, session: &mut probe_rs::Session) -> Result<(), Error> {
        let mut state = RttSessionState::Init;
        std::mem::swap(&mut state, &mut self.state);
        match state {
            RttSessionState::Init => {
                self.do_init(session)?;
                self.attach_retry_limiter.reset();
                self.transition_state(RttSessionState::RttAttach(Instant::now()));
            }
            RttSessionState::RttAttach(started_at) => {
                self.state = RttSessionState::RttAttach(started_at);

                if !self.attach_retry_limiter.check() {
                    return Ok(());
                }

                let rtt = match self.do_rtt_attach(session) {
                    Ok(rtt) => rtt,
                    Err(e) => {
                        if let Some(ms) = self.rtt_cfg.attach_timeout_ms {
                            if matches!(e, Error::Rtt(probe_rs::rtt::Error::ControlBlockNotFound)) {
                                let timeout = Duration::from_millis(ms as _);
                                if Instant::now().duration_since(started_at) > timeout {
                                    return Err(e);
                                } else {
                                    // We'll try again on the next schedule
                                    return Ok(());
                                }
                            }
                        }

                        return Err(e);
                    }
                };

                // Send the client a success response once we're pretty sure things are
                // working if we haven't already
                if !self.client_response_sent {
                    debug!("Sending client response");
                    let resp = ProxySessionStatus::session_started(self.id);
                    let mut se = serde_json::Serializer::new(&mut self.stream);
                    resp.serialize(&mut se)?;
                    self.client_response_sent = true;
                }

                self.rtt_poll_limiter.reset();
                self.idle_poll_limiter.reset();
                self.recovery_limiter.reset();
                self.stopping_condition_limiter.reset();

                self.transition_state(RttSessionState::Run(Box::new(rtt)));
            }
            RttSessionState::Run(rtt_handles) => {
                let res = self.do_run(session, &rtt_handles);
                self.state = RttSessionState::Run(rtt_handles);
                match res? {
                    StoppingConditionStatus::NotReached => (),
                    StoppingConditionStatus::Breakpoint(addr) => {
                        let reason = StoppedReason::Breakpoint(addr);
                        debug!(%reason, "Stopping condition reached");
                        self.stop(session, reason);
                    }
                    StoppingConditionStatus::ResetCatch => {
                        let reason = StoppedReason::ResetCatch;
                        debug!(%reason, "Stopping condition reached");
                        self.stop(session, reason);
                    }
                }
            }
            RttSessionState::Shutdown => {
                // Do nothing
                self.state = RttSessionState::Shutdown;
            }
            RttSessionState::Stopped(r) => {
                // Do nothing
                self.state = RttSessionState::Stopped(r)
            }
        }
        Ok(())
    }

    fn do_init(&mut self, session: &mut probe_rs::Session) -> Result<(), Error> {
        let mut core = session
            .core(self.target_cfg.core as _)
            .map_err(Error::CoreAttach)?;
        let core_status = core.status().map_err(Error::CoreStatus)?;
        debug!(core = self.target_cfg.core, ?core_status);

        if self.target_cfg.reset {
            debug!("Reset and halt core");
            // This is what probe-rs does
            core.reset_and_halt(Duration::from_millis(100))?;

            let sp_reg = core.stack_pointer();
            let sp: RegisterValue = core.read_core_reg(sp_reg.id())?;
            let pc_reg = core.program_counter();
            let pc: RegisterValue = core.read_core_reg(pc_reg.id())?;
            debug!(pc = %pc, sp = %sp);
        }

        // Disable any previous vector catching and breakpoints
        // if we're the first core to do so (possible this is a shared core session)
        if self.shared_state.inc_clear_vector_catch_and_breakpoints() {
            debug!("Clearing previous vector catches and breakpoints");
            core.disable_vector_catch(VectorCatchCondition::All)?;
            core.clear_all_hw_breakpoints()?;
        }

        // If we're the bootloader or companion app core, enable reset vector catching
        // early on, other cores will do the same after RTT attach succeeds
        // since they would otherwise trip the catch immediately if they're
        // waiting to be started by primary core
        if self.target_cfg.bootloader || self.target_cfg.bootloader_companion_application {
            // First session on the core will enable it
            if self.shared_state.inc_vector_catch_enabled() {
                debug!("Enabling CoreReset vector catch");
                core.enable_vector_catch(VectorCatchCondition::CoreReset)?;
            }
        }

        // Start the core if it's halted, auxilary cores are expected to not be halted
        if !self.target_cfg.bootloader_companion_application
            && (self.target_cfg.reset || matches!(core_status, CoreStatus::Halted(_)))
        {
            let sp_reg = core.stack_pointer();
            let sp: RegisterValue = core.read_core_reg(sp_reg.id())?;
            let pc_reg = core.program_counter();
            let pc: RegisterValue = core.read_core_reg(pc_reg.id())?;
            debug!(pc = %pc, sp = %sp, "Run core");
            core.run()?;
        }

        // Core needs to be running to continue
        let core_status = core.status().map_err(Error::CoreStatus)?;
        if !matches!(core_status, CoreStatus::Running) {
            return Err(Error::CoreNotRunning(self.target_cfg.core));
        }

        // Set the breakpoint for on-stop
        if let Some(bp_addr) = self.rtt_cfg.stop_on_breakpoint_address {
            let num_bp = core.available_breakpoint_units()?;
            debug!(
                available_breakpoints = num_bp,
                addr = format_args!("0x{:X}", bp_addr),
                "Setting breakpoint for stopping condition"
            );
            core.set_hw_breakpoint(bp_addr)?;
        }

        Ok(())
    }

    fn do_rtt_attach(&mut self, session: &mut probe_rs::Session) -> Result<RttHandles, Error> {
        let mut core = session
            .core(self.target_cfg.core as _)
            .map_err(Error::CoreAttach)?;

        debug!("Attaching RTT region");
        let mut rtt = Rtt::attach_region(&mut core, &self.rtt_scan_region)?;
        debug!(
            addr = format_args!("0x{:X}", rtt.ptr()),
            "Found RTT control block"
        );

        if self.rtt_cfg.up_channel as usize > rtt.up_channels.len() {
            return Err(Error::UpChannelInvalid(self.rtt_cfg.up_channel as _));
        }
        if self.rtt_cfg.down_channel as usize > rtt.down_channels.len() {
            return Err(Error::DownChannelInvalid(self.rtt_cfg.down_channel as _));
        }

        let up_channel = rtt.up_channels.remove(self.rtt_cfg.up_channel as _);
        let up_channel_mode = up_channel.mode(&mut core)?;
        debug!(channel = up_channel.number(), mode = ?up_channel_mode, buffer_size = up_channel.buffer_size(), "Opened up channel");

        let down_channel = rtt.down_channels.remove(self.rtt_cfg.down_channel as _);
        debug!(
            channel = down_channel.number(),
            buffer_size = down_channel.buffer_size(),
            "Opened down channel"
        );

        if !self.rtt_cfg.disable_control_plane {
            if self.rtt_cfg.restart {
                debug!("Sending stop command");
                let cmd = TrcCommand::StopTracing.to_wire_bytes();
                down_channel.write(&mut core, &cmd)?;
                thread::sleep(Duration::from_millis(10));
            }

            debug!("Sending start command");
            let cmd = TrcCommand::StartTracing.to_wire_bytes();
            down_channel.write(&mut core, &cmd)?;
        }

        // Auxilary cores get vector catching enabled after we know they're running
        if !self.target_cfg.bootloader && !self.target_cfg.bootloader_companion_application {
            // First session on the core will enable it
            if self.shared_state.inc_vector_catch_enabled() {
                debug!("Enabling CoreReset vector catch");
                core.enable_vector_catch(VectorCatchCondition::CoreReset)?;
            }
        }

        Ok(RttHandles {
            rtt,
            up_channel,
            down_channel,
        })
    }

    fn do_run(
        &mut self,
        session: &mut probe_rs::Session,
        handles: &RttHandles,
    ) -> Result<StoppingConditionStatus, Error> {
        let mut core = session
            .core(self.target_cfg.core as _)
            .map_err(Error::CoreAttach)?;

        let rate_limiter = match self.last_rtt_read_had_data {
            true => &mut self.rtt_poll_limiter,
            false => &mut self.idle_poll_limiter,
        };

        let rtt_bytes_read = if rate_limiter.check() {
            let bytes_read = handles.up_channel.read(&mut core, &mut self.host_buffer)?;

            if self.log_rtt_metrics {
                self.metrics.update(bytes_read);
            }

            self.last_rtt_read_had_data = bytes_read != 0;

            bytes_read
        } else {
            0
        };

        if rtt_bytes_read != 0 {
            trace!(bytes = rtt_bytes_read, "Writing RTT data");

            if let Err(e) = self.stream.write_all(&self.host_buffer[..rtt_bytes_read]) {
                info!(error = %e, "Client disconnected");
                return Err(Error::ClientDisconnected);
            }
        } else {
            // No data

            // Check for stopping conditions (breakpoint, vector catch)
            if self.stopping_condition_limiter.check() {
                let core_status = core.status().map_err(Error::CoreStatus)?;
                match core_status {
                    CoreStatus::Running => (),
                    CoreStatus::Halted(halt_reason) => {
                        let sp_reg = core.stack_pointer();
                        let sp: RegisterValue = core.read_core_reg(sp_reg.id())?;
                        let pc_reg = core.program_counter();
                        let pc: RegisterValue = core.read_core_reg(pc_reg.id())?;
                        let pc_addr: u64 = match pc {
                            RegisterValue::U32(v) => v.into(),
                            RegisterValue::U64(v) => v,
                            RegisterValue::U128(v) => v as _,
                        };
                        debug!(core = self.target_cfg.core, pc = %pc, sp = %sp, "Core is halted");
                        match halt_reason {
                            HaltReason::Breakpoint(_) => {
                                if let Some(on_stop_addr) = self.rtt_cfg.stop_on_breakpoint_address
                                {
                                    if on_stop_addr != pc_addr {
                                        warn!(bp_addr = on_stop_addr, pc = %pc, "Program counter doesn't match configured breakpoint");
                                    }
                                }
                                return Ok(StoppingConditionStatus::Breakpoint(pc_addr));
                            }
                            HaltReason::Exception => {
                                // We only enable CoreReset vector catch, so this is a reset
                                return Ok(StoppingConditionStatus::ResetCatch);
                            }
                            _ => {
                                warn!(reason = ?halt_reason, "Unexpected halt reason");
                                return Ok(StoppingConditionStatus::ResetCatch);
                            }
                        }
                    }
                    state => {
                        warn!(state = ?state, "Core is in an unexpected state");
                        return Ok(StoppingConditionStatus::ResetCatch);
                    }
                }
            }
        }

        Ok(StoppingConditionStatus::NotReached)
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
enum StoppingConditionStatus {
    NotReached,
    Breakpoint(u64),
    ResetCatch,
}

#[derive(Debug)]
pub enum StoppedReason {
    Shutdown,
    Error(Error),
    ResetCatch,
    Breakpoint(u64),
}

#[derive(Debug)]
pub struct RttHandles {
    rtt: Rtt,
    /// RTT up (target to host) channel
    up_channel: UpChannel,
    /// RTT down (host to target) channel
    down_channel: DownChannel,
}

#[derive(Debug)]
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

type JoinHandle = thread::JoinHandle<()>;
fn spawn_control_thread(
    proxy_session_id: ProxySessionId,
    thread_name: String,
    op_channel: Sender<Operation>,
    mut stream: TcpStream,
) -> io::Result<JoinHandle> {
    let builder = thread::Builder::new().name(thread_name);
    builder.spawn(move || {
        info!(id = %proxy_session_id, "Starting RTT session control");

        let mut de = serde_json::Deserializer::from_reader(&mut stream);
        match ProxySessionControl::deserialize(&mut de) {
            Ok(_cmd) => {
                info!("Shutting down");
            }
            Err(e) => {
                warn!(error = %e, "Shutting down");
            }
        }

        stream.shutdown(std::net::Shutdown::Both).ok();

        op_channel
            .send(Operation::ShutdownSession(proxy_session_id))
            .ok();
    })
}

impl fmt::Display for StoppedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoppedReason::Shutdown => f.write_str("shutdown"),
            StoppedReason::Error(_) => f.write_str("error"),
            StoppedReason::Breakpoint(addr) => write!(f, "breakpoint=0x{:X}", addr),
            StoppedReason::ResetCatch => f.write_str("reset"),
        }
    }
}

impl fmt::Display for RttSessionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RttSessionState::Init => f.write_str("INIT"),
            RttSessionState::RttAttach(_) => f.write_str("RTT_ATTACH"),
            RttSessionState::Run(rtt) => write!(f, "RUN(cb = 0x{:X})", rtt.rtt.ptr()),
            RttSessionState::Shutdown => f.write_str("SHUTDOWN"),
            RttSessionState::Stopped(reason) => write!(f, "STOPPED({})", reason),
        }
    }
}
