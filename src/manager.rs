use crate::{interruptor::Interruptor, rtt_session};
use parking_lot::FairMutex;
use probe_rs::{
    config::TargetSelector,
    probe::{list::Lister, DebugProbeInfo, DebugProbeSelector, WireProtocol},
    Permissions, Session,
};
use rtt_proxy::{
    ProbeConfig, ProxySessionConfig, ProxySessionId, ProxySessionStatus, RttConfig, TargetConfig,
};
use serde::{Deserialize, Serialize};
use std::collections::{hash_map, HashMap};
use std::{
    fmt, io,
    net::{SocketAddr, TcpStream},
    str::FromStr,
    sync::mpsc,
    sync::Arc,
    thread,
};
use tracing::{debug, info, trace, warn};
use uuid::Uuid;

#[derive(Debug)]
pub enum Operation {
    Shutdown,
    PruneInactiveSessions,
    RecoverSession(rtt_session::RecoveryState),
    HandleClient((TcpStream, SocketAddr)),
}

impl Operation {
    pub const OPERATION_CHANNEL_SIZE: usize = 32;
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Manager IO error. {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug)]
enum SessionReq {
    New((ProxySessionConfig, TcpStream)),
    Recovery(rtt_session::RecoveryState),
}

pub fn spawn(
    log_rtt_metrics: bool,
    op_rx: mpsc::Receiver<Operation>,
    op_tx_for_sessions: mpsc::SyncSender<Operation>,
) -> io::Result<thread::JoinHandle<Result<(), Error>>> {
    let builder = thread::Builder::new().name("manager".into());
    builder.spawn(move || manager_thread(log_rtt_metrics, op_rx, op_tx_for_sessions))
}

fn manager_thread(
    log_rtt_metrics: bool,
    op_rx: mpsc::Receiver<Operation>,
    op_tx_for_sessions: mpsc::SyncSender<Operation>,
) -> Result<(), Error> {
    info!("Starting manager");

    let mut mngr = Manager::new(log_rtt_metrics, op_tx_for_sessions);

    loop {
        let Ok(op) = op_rx.recv() else {
            info!("Channel closed");
            break;
        };
        match op {
            Operation::Shutdown => {
                info!("Got shutdown request");
                break;
            }
            Operation::PruneInactiveSessions => {
                debug!("Got prune inactive sessions request");
                mngr.prune_sessions(false);
            }
            Operation::RecoverSession(recovery_state) => {
                debug!(id = %recovery_state.proxy_session_id, "Recovering session");
                mngr.prune_sessions(false);
                if let Err(e) =
                    mngr.handle_new_session_req(SessionReq::Recovery(recovery_state.try_clone()?))
                {
                    warn!(id = %recovery_state.proxy_session_id, error = %e, "Session recovery failed");

                    // TODO cfg or recovery thread with time handling
                    std::thread::sleep(std::time::Duration::from_millis(10));

                    // Try again
                    mngr.op_tx_for_sessions
                        .send(Operation::RecoverSession(recovery_state))
                        .ok();
                }
            }
            Operation::HandleClient((mut client, client_addr)) => {
                debug!(peer = %client_addr, "Waiting for config");
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

                        // The RTT sesssion thread will send the Ok response
                        if let Err(e) = mngr.handle_new_session_req(SessionReq::New((cfg, client)))
                        {
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

    info!("Shutting down");
    mngr.shutdown_blocking();

    Ok(())
}

#[derive(Debug)]
struct Manager {
    log_rtt_metrics: bool,
    probe_states: HashMap<ProbeId, ProbeState>,
    op_tx_for_sessions: mpsc::SyncSender<Operation>,
}

impl Manager {
    fn new(log_rtt_metrics: bool, op_tx_for_sessions: mpsc::SyncSender<Operation>) -> Self {
        Self {
            log_rtt_metrics,
            probe_states: Default::default(),
            op_tx_for_sessions,
        }
    }

    fn shutdown_blocking(&mut self) {
        for probe_state in self.probe_states.values() {
            probe_state.sessions_interruptor.set();
        }
        self.prune_sessions(true);
    }

    fn prune_sessions(&mut self, wait_for_session_shutdown: bool) {
        for (probe_id, probe_state) in self.probe_states.iter_mut() {
            // Remove finished proxy sessions
            probe_state.proxy_sessions.retain(|id, state| {
                if state
                    .join_handle
                    .as_ref()
                    .map(|jh| jh.is_finished() || wait_for_session_shutdown)
                    .unwrap_or(false)
                {
                    if let Some(join_handle) = state.join_handle.take() {
                        debug!(probe = %probe_id, proxy_session_id = %id, "Pruning RTT session");
                        let _ = join_handle.join().ok();
                    }
                }

                // Only retain sessions that are still active
                state.join_handle.is_some()
            });
        }

        // Remove probes that no proxy sessions
        self.probe_states.retain(|probe_id, state| {
            if state.proxy_sessions.is_empty() {
                debug!(probe = %probe_id, "Pruning probe");
            }

            !state.proxy_sessions.is_empty()
        });
    }

    fn handle_new_session_req(&mut self, req: SessionReq) -> Result<(), ManagerError> {
        let (recovery_mode, response_already_sent, req_cfg, client) = match req {
            SessionReq::New((cfg, c)) => (false, false, cfg, c),
            SessionReq::Recovery(recovery_state) => (
                true,
                recovery_state.response_already_sent,
                ProxySessionConfig {
                    version: rtt_proxy::V1,
                    probe: recovery_state.probe_cfg,
                    target: recovery_state.target_cfg,
                    rtt: recovery_state.rtt_cfg,
                },
                recovery_state.stream,
            ),
        };

        let probe_info = Self::find_probe_info(&req_cfg.probe)?;
        let probe_id = ProbeId::from(&probe_info);

        if req_cfg.probe.force_exclusive {
            if let Some(probe_state) = self.probe_states.get_mut(&probe_id) {
                debug!(probe = %probe_info, "Shutting down sessions for exclusive probe request");
                probe_state.shutdown_sessions();
            }
        }

        self.prune_sessions(false);

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
                let session = Self::setup_probe(&probe_info, &req_cfg.probe)?;
                v.insert(ProbeState {
                    cfg: req_cfg.probe.clone(),
                    sessions_interruptor: Interruptor::default(),
                    session: Arc::new(FairMutex::new(session)),
                    proxy_sessions: Default::default(),
                })
            }
        };

        // Check if we already have an RTT session attached to the core at the
        // same control block address
        if probe_state.proxy_sessions.values().any(|ps| {
            (ps.target_cfg.core == req_cfg.target.core)
                && (ps.rtt_cfg.control_block_address == req_cfg.rtt.control_block_address)
        }) {
            return Err(ManagerError::RttSessionAlreadyStarted);
        }

        let proxy_session_id = Uuid::new_v4();
        let spawn_args = rtt_session::SpawnArgs {
            proxy_session_id,
            // NOTE: tracing_subscribe will indent to largest thread name,
            // and you can't turn that off
            // https://github.com/tokio-rs/tracing/issues/2465
            thread_name: format!(
                "{}::{}:{}",
                probe_id, probe_state.cfg.target, req_cfg.target.core
            ),
            probe_cfg: req_cfg.probe,
            target_cfg: req_cfg.target.clone(),
            rtt_cfg: req_cfg.rtt.clone(),
            log_rtt_metrics: self.log_rtt_metrics,
            recovery_mode,
            response_already_sent,
            interruptor: probe_state.sessions_interruptor.clone(),
            shutdown_channel: self.op_tx_for_sessions.clone(),
            session: probe_state.session.clone(),
            stream: client,
        };

        let join_handle = rtt_session::spawn(spawn_args)?;
        probe_state.proxy_sessions.insert(
            proxy_session_id,
            RttSessionState {
                target_cfg: req_cfg.target,
                rtt_cfg: req_cfg.rtt,
                join_handle: Some(join_handle),
            },
        );

        Ok(())
    }

    fn find_probe_info(cfg: &ProbeConfig) -> Result<DebugProbeInfo, ManagerError> {
        let available_probes = available_probes();

        for (idx, probe) in available_probes.iter().enumerate() {
            debug!(probe_idx = idx, probe = %probe, "Discovered debug probe");
        }

        let probe_info = if let Some(selector) = cfg.probe_selector.as_deref() {
            let selector = DebugProbeSelector::from_str(selector)?;
            let probe_id = ProbeId::from(&selector);
            debug!(%selector, "Searching for debug probe");

            // Does this probe exist?
            available_probes
                .into_iter()
                .find(|p| ProbeId::from(p) == probe_id)
                .ok_or(ManagerError::ProbeNotFound(selector))?
        } else {
            debug!("Using the first available debug probe");

            // We support omitting the selector if there's only a single
            // available probe
            if available_probes.len() > 1 {
                return Err(ManagerError::NoProbeSelector);
            } else if let Some(first_probe) = available_probes.first() {
                first_probe.clone()
            } else {
                return Err(ManagerError::NoProbesAvailable);
            }
        };

        Ok(probe_info)
    }

    fn setup_probe(
        probe_info: &DebugProbeInfo,
        cfg: &ProbeConfig,
    ) -> Result<Session, ManagerError> {
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

        let session = if cfg.attach_under_reset {
            probe.attach_under_reset(target_selector, Permissions::default())?
        } else {
            probe.attach(target_selector, Permissions::default())?
        };

        Ok(session)
    }
}

#[derive(Debug)]
struct ProbeState {
    cfg: ProbeConfig,
    sessions_interruptor: Interruptor,
    session: Arc<FairMutex<Session>>,
    proxy_sessions: HashMap<ProxySessionId, RttSessionState>,
}

impl ProbeState {
    fn shutdown_sessions(&mut self) {
        self.sessions_interruptor.set();

        let mut proxy_sessions = HashMap::new();
        std::mem::swap(&mut self.proxy_sessions, &mut proxy_sessions);

        for (id, mut state) in proxy_sessions.into_iter() {
            if let Some(join_handle) = state.join_handle.take() {
                debug!(proxy_session_id = %id, "Shutting down RTT session");
                let _ = join_handle.join().ok();
            }
        }
    }
}

#[derive(Debug)]
struct RttSessionState {
    target_cfg: TargetConfig,
    rtt_cfg: RttConfig,
    join_handle: Option<rtt_session::JoinHandle>,
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

    #[error("An RTT session is already attached to this target's core at the same control block address")]
    RttSessionAlreadyStarted,
}
