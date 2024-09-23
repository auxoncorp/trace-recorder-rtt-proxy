use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

pub type Version = u8;
pub const V1: Version = 1;

pub type ProxySessionId = Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProxySessionConfig {
    pub version: Version,
    pub probe: ProbeConfig,
    pub target: TargetConfig,
    pub rtt: RttConfig,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum ProxySessionStatus {
    Started(ProxySessionId),
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum ProxySessionControl {
    Shutdown(ProxySessionId),
}

impl ProxySessionStatus {
    pub fn session_started(id: ProxySessionId) -> Self {
        ProxySessionStatus::Started(id)
    }

    pub fn error<T: ToString>(err: T) -> Self {
        ProxySessionStatus::Error(err.to_string())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub enum Target {
    Auto,
    Specific(String),
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Auto => f.write_str("AUTO"),
            Target::Specific(s) => f.write_str(s),
        }
    }
}

/// Probe config, only applied on the first session attaching to the probe
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct ProbeConfig {
    /// If None, connect to the first available probe (only supported if a single probe is
    /// connected)
    pub probe_selector: Option<String>,
    pub protocol: String,
    /// Protocol speed in kHz
    pub speed_khz: u32,
    /// If None, try to automatically identify the target chip, by reading
    /// identifying information from the probe and / or target
    pub target: Target,
    pub attach_under_reset: bool,
    /// If True, any existing sessions using this probe will be shut down
    pub force_exclusive: bool,
}

/// Target config
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct TargetConfig {
    /// Automatically attempt to recover the debug probe connection
    /// when an error is encountered.
    pub auto_recover: bool,
    pub core: u32,
    /// Reset and halt the core before starting the RTT reader session
    pub reset: bool,
    /// This session will have exclusive access to the core's
    /// control functionality (i.e. hardware breakpoints, reset, etc).
    /// If another session (i.e. the application to be booted by the bootloader)
    /// is requested on this core, it will be suspended until this session
    /// signals completion.
    pub bootloader: bool,
    /// This session will not drive any of the core's
    /// control functionality (i.e. hardware breakpoints, reset, etc)
    pub bootloader_companion_application: bool,
}

/// RTT config
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default)]
pub struct RttConfig {
    pub attach_timeout_ms: Option<u32>,
    pub setup_on_breakpoint_address: Option<u64>,
    pub stop_on_breakpoint_address: Option<u64>,
    pub no_data_stop_timeout_ms: Option<u32>,
    /// If None, memory will be scanned for the control block
    pub control_block_address: Option<u64>,
    pub up_channel: u32,
    pub down_channel: u32,
    pub disable_control_plane: bool,
    /// Sends the stop command before the start command
    pub restart: bool,
    pub rtt_read_buffer_size: u32,
    pub rtt_poll_interval_ms: u32,
    pub rtt_idle_poll_interval_ms: u32,
}

impl RttConfig {
    pub const DEFAULT_POLL_INTERVAL_MS: u32 = 1;
    pub const DEFAULT_IDLE_POLL_INTERVAL_MS: u32 = 100;
    pub const DEFAULT_RTT_BUFFER_SIZE: u32 = 1024;
}

impl Default for RttConfig {
    fn default() -> Self {
        Self {
            attach_timeout_ms: None,
            setup_on_breakpoint_address: None,
            stop_on_breakpoint_address: None,
            no_data_stop_timeout_ms: None,
            control_block_address: None,
            up_channel: 1,
            down_channel: 1,
            disable_control_plane: false,
            restart: false,
            rtt_read_buffer_size: Self::DEFAULT_RTT_BUFFER_SIZE,
            rtt_poll_interval_ms: Self::DEFAULT_POLL_INTERVAL_MS,
            rtt_idle_poll_interval_ms: Self::DEFAULT_IDLE_POLL_INTERVAL_MS,
        }
    }
}
