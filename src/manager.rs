use crate::{
    rate_limiter::RateLimiter,
    rtt_session::RttSession,
    shared_state::{SharedState, SharedStateRc},
};
use crossbeam_channel::{select_biased, tick, Receiver, Sender};
use probe_rs::{
    config::TargetSelector,
    probe::{list::Lister, DebugProbeInfo, DebugProbeSelector, WireProtocol},
    Permissions,
};
use rtt_proxy::{ProbeConfig, ProxySessionConfig, ProxySessionId, ProxySessionStatus};
use serde::{Deserialize, Serialize};
use std::collections::{hash_map, HashMap};
use std::{
    fmt, io,
    net::{SocketAddr, TcpStream},
    str::FromStr,
    thread,
    time::Duration,
};
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Manager IO error. {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug)]
pub enum Operation {
    Shutdown,
    ShutdownSession(ProxySessionId),
    HandleClient((TcpStream, SocketAddr)),
}

impl Operation {
    pub const OPERATION_CHANNEL_SIZE: usize = 8;
}

// This is the value probe-rs uses
const TARGET_VOLTAGE_POWER_ON_THRESHOLD: f32 = 1.4;

/// Put an upper bound of 1ms (1000 Hz) on any operation so
/// that we never attempt to do a series of debug probe operations
/// too fast. That can put the probe into a weird state.
const SERVICE_TICK_INTERVAL: Duration = Duration::from_millis(1);

pub fn spawn(
    log_rtt_metrics: bool,
    op_rx: Receiver<Operation>,
    op_tx_for_sessions: Sender<Operation>,
) -> io::Result<thread::JoinHandle<Result<(), Error>>> {
    let builder = thread::Builder::new().name("manager".into());
    builder.spawn(move || manager_thread(log_rtt_metrics, op_rx, op_tx_for_sessions))
}

fn manager_thread(
    log_rtt_metrics: bool,
    op_rx: Receiver<Operation>,
    op_tx_for_sessions: Sender<Operation>,
) -> Result<(), Error> {
    info!("Starting manager");

    let mut mngr = Manager::new(log_rtt_metrics, op_tx_for_sessions);
    let ticker = tick(SERVICE_TICK_INTERVAL);

    loop {
        select_biased! {
            recv(op_rx) -> op_res => {
                let Ok(op) = op_res else {
                    info!("Channel closed");
                    break;
                };
                match op {
                    Operation::Shutdown => {
                        debug!("Got shutdown request");
                        break;
                    }
                    Operation::ShutdownSession(id) => {
                        debug!(%id, "Got shutdown session request");
                        mngr.shutdown_session(id);
                    }
                    Operation::HandleClient((mut client, client_addr)) => {
                        debug!(peer = %client_addr, "Got client request, waiting for config");
                        let config_res = {
                            let mut de = serde_json::Deserializer::from_reader(&mut client);
                            ProxySessionConfig::deserialize(&mut de)
                        };
                        let mut cloned_stream = client.try_clone()?;
                        let mut client_resp_stream = serde_json::Serializer::new(&mut cloned_stream);
                        match config_res {
                            Ok(cfg) => {
                                debug!(peer = %client_addr, "Process session config");
                                trace!(?cfg);

                                // The RTT sesssion will send the Ok response
                                if let Err(e) = mngr.handle_new_session_req(cfg, client) {
                                    warn!(peer = %client_addr, error = %e, "Dropping client due to error");
                                    let resp = ProxySessionStatus::error(e);
                                    if resp.serialize(&mut client_resp_stream).is_err() {
                                        warn!(peer = %client_addr, "Failed to send response");
                                    }
                                }
                                std::mem::drop(cloned_stream);
                            }
                            Err(e) => {
                                warn!(peer = %client_addr, "Invalid session config");
                                let resp = ProxySessionStatus::error(e);
                                let _ignored = resp.serialize(&mut client_resp_stream).ok();
                            }
                        }
                    }
                }
            }

            recv(ticker) -> _ => {
                mngr.service_probes();
            }
        }
    }

    mngr.shutdown();

    Ok(())
}

#[derive(Debug)]
struct Manager {
    log_rtt_metrics: bool,
    op_tx_for_sessions: Sender<Operation>,
    probe_states: HashMap<ProbeId, ProbeState>,
}

impl Manager {
    fn new(log_rtt_metrics: bool, op_tx_for_sessions: Sender<Operation>) -> Self {
        Self {
            log_rtt_metrics,
            op_tx_for_sessions,
            probe_states: Default::default(),
        }
    }

    fn shutdown(&mut self) {
        info!("Shutting down");
        let probe_states = std::mem::take(&mut self.probe_states);
        for (_probe_id, mut probe_state) in probe_states.into_iter() {
            probe_state.shutdown_all_rtt_sessions();
        }
    }

    fn shutdown_session(&mut self, id: ProxySessionId) {
        if let Some(probe_state) = self
            .probe_states
            .values_mut()
            .find(|ps| ps.rtt_sessions.contains_key(&id))
        {
            probe_state.shutdown_rtt_session(id);
        }
        self.prune_inactive_probes();
    }

    fn prune_inactive_probes(&mut self) {
        self.probe_states.retain(|id, state| {
            if state.rtt_sessions.is_empty() {
                debug!(probe = %id, "Pruning probe");
            }
            !state.rtt_sessions.is_empty()
        });
    }

    fn service_probes(&mut self) {
        for probe_state in self.probe_states.values_mut() {
            probe_state.service();
        }
    }

    fn handle_new_session_req(
        &mut self,
        req_cfg: ProxySessionConfig,
        client: TcpStream,
    ) -> Result<(), ManagerError> {
        self.prune_inactive_probes();

        let probe_info = find_probe_info(&req_cfg.probe)?;
        let probe_id = ProbeId::from(&probe_info);

        if req_cfg.probe.force_exclusive {
            if let Some(mut probe_state) = self.probe_states.remove(&probe_id) {
                debug!(probe = %probe_info, "Shutting down sessions for exclusive probe request");
                probe_state.shutdown_all_rtt_sessions();
            }
        }

        // Do initial setup if we haven't already done so for this probe
        let probe_state = match self.probe_states.entry(probe_id.clone()) {
            hash_map::Entry::Occupied(o) => {
                debug!(probe = %probe_info, "Found existing probe");
                if o.get().cfg != req_cfg.probe {
                    warn!(probe = %probe_info, "Existing configuration does not match requested configuration");
                }
                o.into_mut()
            }
            hash_map::Entry::Vacant(v) => {
                debug!(probe = %probe_info, "Opening probe");
                let session = setup_probe(&probe_info, &req_cfg.probe)?;
                v.insert(ProbeState::new(probe_info, req_cfg.probe, session))
            }
        };

        // Check if we already have an RTT session attached to the core at the
        // same control block address, and same up channel
        if probe_state.rtt_sessions.values().any(|rtt| {
            (rtt.target_config().core == req_cfg.target.core)
                && (rtt.rtt_config().control_block_address == req_cfg.rtt.control_block_address)
                && (rtt.rtt_config().up_channel == req_cfg.rtt.up_channel)
        }) {
            return Err(ManagerError::RttSessionAlreadyStarted);
        }

        let session_name = {
            let suffix = if req_cfg.target.bootloader {
                ":bl"
            } else if req_cfg.target.bootloader_companion_application {
                ":app"
            } else {
                ""
            };
            format!(
                "{}:{}{}",
                probe_state.cfg.target, req_cfg.target.core, suffix
            )
        };

        let id = Uuid::new_v4();
        let shared_state = probe_state.shared_state_for_core(req_cfg.target.core);
        let probe_rs_session = match &mut probe_state.session {
            ProbeSessionState::Active(session) => session,
            ProbeSessionState::NeedsRecovered => {
                warn!("Rejecting new client request to due probe being in recovery state");
                return Err(ManagerError::ProbeInRecovery);
            }
        };
        let rtt_session = RttSession::new(
            id,
            session_name,
            req_cfg.target,
            req_cfg.rtt,
            self.log_rtt_metrics,
            shared_state,
            self.op_tx_for_sessions.clone(),
            client,
            probe_rs_session,
        )?;
        probe_state.add_rtt_session(rtt_session);

        Ok(())
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum ProbeSessionState {
    Active(probe_rs::Session),
    NeedsRecovered,
}

#[derive(Debug)]
struct ProbeState {
    info: DebugProbeInfo,
    cfg: ProbeConfig,
    session: ProbeSessionState,
    per_core_shared_state: HashMap<u32, SharedStateRc>,
    rtt_sessions: HashMap<ProxySessionId, RttSession>,
    schedule: Vec<ProxySessionId>,
    next_schedule_idx: usize,
    num_sessions_needing_serviced_before_recovery_rentry: usize,
    recovery_limiter: RateLimiter,
    recovery_check_limiter: RateLimiter,
}

impl ProbeState {
    const RECOVERY_RETRY_INTERVAL: Duration = Duration::from_millis(50);
    const RECOVERY_CHECK_INTERVAL: Duration = Duration::from_millis(100);

    fn new(info: DebugProbeInfo, cfg: ProbeConfig, session: probe_rs::Session) -> Self {
        Self {
            info,
            cfg,
            session: ProbeSessionState::Active(session),
            per_core_shared_state: Default::default(),
            rtt_sessions: Default::default(),
            schedule: Default::default(),
            next_schedule_idx: 0,
            num_sessions_needing_serviced_before_recovery_rentry: 0,
            recovery_limiter: RateLimiter::new(Self::RECOVERY_RETRY_INTERVAL),
            recovery_check_limiter: RateLimiter::new(Self::RECOVERY_CHECK_INTERVAL),
        }
    }

    fn shutdown_all_rtt_sessions(&mut self) {
        let rtt_sessions = std::mem::take(&mut self.rtt_sessions);
        if let ProbeSessionState::Active(session) = &mut self.session {
            for (_id, mut rtt_session) in rtt_sessions.into_iter() {
                rtt_session.shutdown(session);
            }
        }
    }

    fn shutdown_rtt_session(&mut self, id: ProxySessionId) {
        if let Some(mut rtt_session) = self.rtt_sessions.remove(&id) {
            if let ProbeSessionState::Active(session) = &mut self.session {
                rtt_session.shutdown(session);
            }

            // Remove it from the scheduler
            if let Some(idx) = self.schedule.iter().position(|sid| *sid == id) {
                self.schedule.remove(idx);
                // Reset the scheduler
                // NOTE: could be smarter about this and preserve the next item
                self.next_schedule_idx = 0;
            }
        }
    }

    fn shared_state_for_core(&mut self, core: u32) -> SharedStateRc {
        self.per_core_shared_state
            .entry(core)
            .or_insert(SharedState::new_rc())
            .clone()
    }

    fn add_rtt_session(&mut self, rtt_session: RttSession) {
        let id = rtt_session.id();
        self.rtt_sessions.insert(id, rtt_session);
        self.schedule.push(id);
    }

    fn enter_recovery(&mut self) {
        debug!(probe = %self.info, "Entering recovery");
        let debug_session = std::mem::replace(&mut self.session, ProbeSessionState::NeedsRecovered);
        std::mem::drop(debug_session);

        for shared_state in self.per_core_shared_state.values() {
            shared_state.reset();
        }

        self.next_schedule_idx = 0;
        self.num_sessions_needing_serviced_before_recovery_rentry = self.rtt_sessions.len();
    }

    fn recover(&mut self) {
        if self.recovery_limiter.check() {
            match setup_probe(&self.info, &self.cfg) {
                Err(e) => {
                    warn!(probe = %self.info, error = %e, "Debug probe recovery failed");
                }
                Ok(session) => {
                    debug!(probe = %self.info, "Debug probe recovery succeeded");
                    self.session = ProbeSessionState::Active(session);
                }
            }
        }
    }

    fn service(&mut self) {
        match &mut self.session {
            ProbeSessionState::NeedsRecovered => {
                self.recover();
                return;
            }
            ProbeSessionState::Active(session) => {
                if let Some(rtt_session) = self
                    .schedule
                    .get(self.next_schedule_idx)
                    .and_then(|id| self.rtt_sessions.get_mut(id))
                {
                    rtt_session.update(session);
                    self.num_sessions_needing_serviced_before_recovery_rentry = self
                        .num_sessions_needing_serviced_before_recovery_rentry
                        .saturating_sub(1);
                }
            }
        }

        // Increment the scheduler
        self.next_schedule_idx += 1;
        if self.next_schedule_idx >= self.schedule.len() {
            self.next_schedule_idx = 0;
        }

        // Do a heuristic check to see if the debug probe
        // needs to be re-attached (likely due to a target power cycle).
        // The debug sequences need to be re-run when that occurs.
        //
        // We only due this if any of the sessions have auto recovery enabled
        // and they're not all shutdown.
        if self.recovery_check_limiter.check()
            && self.num_sessions_needing_serviced_before_recovery_rentry == 0
        {
            // We've serviced every RTT session at least once following
            // a previous recovery

            let any_session_using_recovery = self
                .rtt_sessions
                .values()
                .any(|rtt| rtt.target_config().auto_recover && !rtt.is_shutdown());

            if !self.rtt_sessions.is_empty() && any_session_using_recovery {
                // We have sessions with auto recover enabled

                let any_sessions_running = self.rtt_sessions.values().any(|rtt| rtt.is_running());
                let any_session_stopped_due_to_core_attach = self
                    .rtt_sessions
                    .values()
                    .any(|rtt| rtt.is_stopped_due_to_core_attach());

                // No sessions are running, and at least 1 is stopped due to a core attach error
                if !any_sessions_running && any_session_stopped_due_to_core_attach {
                    debug!(probe = %self.info, "Debug probe status failed the heuristic check");
                    self.enter_recovery();
                }
            }
        }
    }
}

fn find_probe_info(cfg: &ProbeConfig) -> Result<DebugProbeInfo, ManagerError> {
    let available_probes = available_probes();

    for (idx, probe) in available_probes.iter().enumerate() {
        debug!(probe_idx = idx, probe = %probe, "Discovered debug probe");
    }

    if let Some(selector) = cfg.probe_selector.as_deref() {
        let selector = DebugProbeSelector::from_str(selector)?;
        let probe_id = ProbeId::from(&selector);
        debug!(%selector, "Searching for debug probe");

        // Does this probe exist?
        Ok(available_probes
            .into_iter()
            .find(|p| ProbeId::from(p) == probe_id)
            .ok_or(ManagerError::ProbeNotFound(selector))?)
    } else {
        debug!("Using the first available debug probe");

        // We support omitting the selector if there's only a single
        // available probe
        if available_probes.len() > 1 {
            Err(ManagerError::NoProbeSelector)
        } else if let Some(first_probe) = available_probes.first() {
            Ok(first_probe.clone())
        } else {
            Err(ManagerError::NoProbesAvailable)
        }
    }
}

fn setup_probe(
    probe_info: &DebugProbeInfo,
    cfg: &ProbeConfig,
) -> Result<probe_rs::Session, ManagerError> {
    let wire_proto = WireProtocol::from_str(&cfg.protocol)
        .map_err(|_| ManagerError::WireProtocol(cfg.protocol.clone()))?;

    let mut probe = probe_info.open()?;

    debug!(protocol = %wire_proto, speed_khz = cfg.speed_khz, "Configuring probe");
    probe.select_protocol(wire_proto)?;
    probe.set_speed(cfg.speed_khz)?;

    debug!(
        target = %cfg.target,
        under_reset = cfg.attach_under_reset,
        "Attaching to chip"
    );

    let target_selector = match &cfg.target {
        rtt_proxy::Target::Auto => TargetSelector::Auto,
        rtt_proxy::Target::Specific(s) => TargetSelector::Unspecified(s.to_owned()),
    };

    let target_voltage = probe.get_target_voltage()?;

    let vtref_string = target_voltage
        .map(|v| format!("{}", v))
        .unwrap_or_else(|| "NA".to_string());
    debug!(target_voltage = vtref_string);

    // Use target voltage to check for target powered on
    if target_voltage.unwrap_or(TARGET_VOLTAGE_POWER_ON_THRESHOLD)
        < TARGET_VOLTAGE_POWER_ON_THRESHOLD
    {
        return Err(ManagerError::TargetNotPowered);
    }

    if cfg.attach_under_reset {
        Ok(probe.attach_under_reset(target_selector, Permissions::default())?)
    } else {
        Ok(probe.attach(target_selector, Permissions::default())?)
    }
}

fn available_probes() -> Vec<DebugProbeInfo> {
    let lister = Lister::new();
    lister.list_all()
}

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
struct ProbeId {
    pub vendor_id: u16,
    pub product_id: u16,
    pub serial_number: Option<String>,
}

impl From<&DebugProbeSelector> for ProbeId {
    fn from(value: &DebugProbeSelector) -> Self {
        Self {
            vendor_id: value.vendor_id,
            product_id: value.product_id,
            serial_number: value.serial_number.clone(),
        }
    }
}

impl From<&DebugProbeInfo> for ProbeId {
    fn from(value: &DebugProbeInfo) -> Self {
        Self {
            vendor_id: value.vendor_id,
            product_id: value.product_id,
            serial_number: value.serial_number.clone(),
        }
    }
}

impl fmt::Display for ProbeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:04x}", self.vendor_id, self.product_id)?;
        if let Some(ref sn) = self.serial_number {
            write!(f, ":{sn}")?;
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
enum ManagerError {
    #[error("Manager IO error. {0}")]
    Io(#[from] io::Error),

    #[error(transparent)]
    DebugProbeSelector(#[from] probe_rs::probe::DebugProbeSelectorParseError),

    #[error("No debug probes available")]
    NoProbesAvailable,

    #[error("A probe selector must be provided when multiple debug probes are available")]
    NoProbeSelector,

    #[error("No debug probe found matching the provided selector '{0}'")]
    ProbeNotFound(DebugProbeSelector),

    #[error("Unsupported wire protocol '{0}'. Only SWD and JTAG are supported.")]
    WireProtocol(String),

    #[error("Encountered an error with the debug probe. {0}")]
    Probe(#[from] probe_rs::Error),

    #[error("Encountered an error with the debug probe. {0}")]
    DebugProbe(#[from] probe_rs::probe::DebugProbeError),

    #[error("An RTT session is already attached to this target's core at the same control block address and up channel")]
    RttSessionAlreadyStarted,

    #[error("Target not powered (VTref less than threshold)")]
    TargetNotPowered,

    #[error("The debug probe is currently in recovery mode")]
    ProbeInRecovery,
}
