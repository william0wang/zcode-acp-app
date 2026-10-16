//! The hub's machine-level system status (`/api/system-stats`, bridge
//! 0.62.0), ported from `src/lib/types.ts`.
//!
//! Server reference: `src/remote/system-stats.ts` (`SystemStats`) — shape
//! identical to the TS `SystemStatsResponse`. Every field is independently
//! best-effort — a probe the hub could not run (denied inside a sandbox,
//! absent on this hardware) arrives as null and the page shows a
//! placeholder, never an error. Rate fields (CPU %, network B/s, hub CPU %)
//! are deltas the hub computes across reads: the FIRST read after a hub
//! start reports null for them.

use serde::{Deserialize, Serialize};

/// One filesystem volume of the system-status page (server `VolumeStats`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsVolume {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub available_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsHost {
    pub hostname: String,
    /// e.g. "15.8 (24H23)" — null when `sw_vers` was unavailable.
    pub os_version: Option<String>,
    /// Hardware model id, e.g. "Mac14,9".
    pub model: Option<String>,
    /// Chip description from the CPU table, e.g. "Apple M2 Pro".
    pub chip: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsCpu {
    pub cores: u64,
    /// Delta of os.cpus() idle/total across calls; null before a baseline
    /// exists.
    pub usage_pct: Option<f64>,
    pub load_avg: [f64; 3],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsMemory {
    pub total_bytes: u64,
    /// free+speculative+inactive+purgeable pages (≈ Activity Monitor's
    /// "available"); os.freemem() fallback.
    pub available_bytes: Option<u64>,
    pub used_bytes: Option<u64>,
    pub swap_total_bytes: Option<u64>,
    pub swap_used_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsStorage {
    /// null when statfs failed; `home` is null again when it is the same
    /// volume as root.
    pub root: Option<SystemStatsVolume>,
    pub home: Option<SystemStatsVolume>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SystemStatsPowerSource {
    Ac,
    Battery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsBattery {
    /// False on a desktop machine (no battery).
    pub present: bool,
    pub percent: Option<u64>,
    pub power_source: Option<SystemStatsPowerSource>,
    /// Raw pmset wording: charging / discharging / charged / …
    pub status: Option<String>,
    pub remaining_min: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsPower {
    /// True while something holds PreventUserIdleSystemSleep — the machine
    /// will not idle-sleep.
    pub prevent_sleep: Option<bool>,
    /// Processes holding that assertion (e.g. ["Amphetamine"]).
    pub sleep_holders: Vec<String>,
    /// Thermal CPU speed limit (100 = full speed; null = no reading
    /// recorded).
    pub cpu_speed_limit_pct: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsNetwork {
    /// Non-internal IPv4 addresses.
    pub addresses: Vec<String>,
    pub ssid: Option<String>,
    /// Summed over all interfaces except lo0, delta across calls.
    pub rx_bytes_per_s: Option<u64>,
    pub tx_bytes_per_s: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsHubProcess {
    pub pid: u64,
    pub rss_bytes: u64,
    /// Delta baseline; null on the first read after hub start.
    pub cpu_pct: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsBridgeProcess {
    pub id: String,
    pub workspace: Option<String>,
    pub pid: Option<u64>,
    pub rss_bytes: Option<u64>,
    pub cpu_pct: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsProcesses {
    pub hub: SystemStatsHubProcess,
    /// One entry per registered bridge; rss/cpuPct via `ps`, null when
    /// denied.
    pub bridges: Vec<SystemStatsBridgeProcess>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsHub {
    pub version: String,
    pub uptime_s: u64,
    pub instances: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemStatsResponse {
    pub collected_at: u64,
    pub host: SystemStatsHost,
    pub uptime_s: u64,
    pub cpu: SystemStatsCpu,
    pub memory: SystemStatsMemory,
    pub storage: SystemStatsStorage,
    pub battery: SystemStatsBattery,
    pub power: SystemStatsPower,
    pub network: SystemStatsNetwork,
    pub processes: SystemStatsProcesses,
    pub hub: SystemStatsHub,
}
