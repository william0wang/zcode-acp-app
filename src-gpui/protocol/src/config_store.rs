//! Configuration screen state — the Rust port of the semantics
//! `tests/config-store-rules.test.ts` covers in `src/store/configSlice.ts`
//! (+ the `connectionResetPatch` halves of `src/store/appStore.ts`): the
//! store's own guards around the reset-card session, the stale-response
//! guards, the capability probe, what survives a hub switch, and the
//! pending-restart flag the backend restart clears.
//!
//! These are the rules neither the response-shape tests (decoding) nor the
//! REST tests (request spelling) can see; each one guards a failure that
//! would otherwise show up only as a dead button or a permanent
//! "unsupported" banner.
//!
//! Scope note: `switchServer` in TS resolves the target from the saved
//! server book and re-points the connection — that book is app-side. The
//! ported observable is the CONNECTION-RESET PATCH both paths run: the
//! config state (capability verdict, payloads, reset-card session,
//! pending-restart flag) is re-probed on the next hub, never carried over.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::quota::AccountUsageStats;
use crate::settings::{Effect, ResetCardInventory, ResetCardType, SettingsAll, SettingsUsage};
use crate::settings_client::{HubApiError, HubHttpClient, HttpTransport};
use crate::system_stats::SystemStatsResponse;
use crate::workflow::WorkflowOverviewResponse;

/// The configuration sections the app can open (TS `ConfigSection`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSection {
    Models,
    Skills,
    Mcp,
    Hooks,
    Agents,
    Quota,
    Usage,
    UsagePlatform,
    Backups,
    AppUpdate,
    Workflows,
    System,
}

/// The i18n keys the store surfaces as its outcome notices (the TS store
/// writes the same strings into `notice`).
pub const NOTICE_RESET_SPENT: &str = "notice.configResetSpent";
pub const NOTICE_RESET_SPEND_FAILED: &str = "notice.configResetSpendFailed";
pub const NOTICE_RESET_STALE: &str = "notice.configResetStale";
pub const NOTICE_NEEDS_RESTART: &str = "notice.configNeedsRestart";
pub const NOTICE_RESTARTED: &str = "notice.configRestarted";
pub const NOTICE_RESTART_FAILED: &str = "notice.configRestartFailed";

/// One reset-card spend gesture (the caller names the provider and the
/// nonce the gesture captured — reading them from the store at action
/// entry would let a provider switch mid-gesture spend the wrong card).
#[derive(Debug, Clone)]
pub struct SpendInput<'a> {
    pub provider_id: &'a str,
    pub nonce: &'a str,
    pub reset_type: ResetCardType,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConfigState {
    /// `false` once a settings route answered 404 — an older hub — so the
    /// screens say so instead of erroring; `None` = not probed yet.
    pub config_supported: Option<bool>,
    pub config_loading: bool,
    pub config_error: Option<String>,
    pub config_all: Option<SettingsAll>,
    pub config_models: Option<Value>,
    pub config_skills: Option<Value>,
    pub config_mcp: Option<Value>,
    pub config_hooks: Option<Value>,
    pub config_agents: Option<Value>,
    /// The range the LAST usage read asked for — a response arriving out of
    /// order (two taps, two in-flight reads) is recognised as stale and
    /// dropped rather than shown under the wrong range button.
    pub config_usage: Option<SettingsUsage>,
    pub config_usage_range: String,
    /// Platform usage: same read-on-open pattern and same stale-range guard
    /// as the local usage read; `false` in platform_supported means THIS hub
    /// predates the route (404), which degrades the platform tab alone.
    pub config_platform_usage: Option<Value>,
    pub config_platform_usage_range: String,
    pub config_platform_usage_supported: bool,
    pub config_backups: Option<Value>,
    pub config_app_update: Option<Value>,
    pub config_workflows: Option<WorkflowOverviewResponse>,
    /// System status: `false` in system_stats_supported means THIS hub
    /// predates /api/system-stats; the flag belongs to that page alone —
    /// config_supported must not retire every section over one endpoint.
    pub config_system_stats: Option<SystemStatsResponse>,
    pub config_system_stats_error: Option<String>,
    pub config_system_stats_supported: bool,
    /// The release channel an app-update read uses (a preview user checked
    /// against stable sees a version the install then refuses).
    pub update_channel: String,
    // Reset cards: the inventory plus which provider it belongs to. The
    // nonce is what a spend must send back, and so is the idempotency key:
    // one key per GESTURE, minted alongside the nonce, so every retry of
    // that spend sends the same value.
    pub reset_cards: Option<ResetCardInventory>,
    pub reset_provider_id: Option<String>,
    pub reset_nonce: Option<String>,
    pub reset_idempotency_key: Option<String>,
    /// The provider ids the hub says could own cards, and whether the
    /// credential store could be decrypted at all.
    pub reset_eligible: Vec<String>,
    pub reset_credentials: bool,
    pub reset_busy: bool,
    /// True once this bridge has recorded a needs-restart write since its
    /// last backend restart — drives the restart affordance.
    pub pending_restart: bool,
    /// The instance the restart addresses (the connected session's bridge);
    /// `None` = no session open, the restart is unavailable.
    pub instance_id: Option<String>,
    /// i18n-key notices (TS `notice`).
    pub notice: Option<String>,
    /// Plain-text failure messages (TS `notify`).
    pub notify: Option<String>,
    /// The combined quota (the plan-quota screen reuses the hub-level read);
    /// `parseUsageStats` semantics — a 2xx body that does not fit the
    /// combined shape reads as no data, not as a failed fetch.
    pub usage_stats: Option<AccountUsageStats>,
    pub quota_unavailable: bool,
}

impl Default for ConfigState {
    fn default() -> Self {
        Self {
            config_supported: None,
            config_loading: false,
            config_error: None,
            config_all: None,
            config_models: None,
            config_skills: None,
            config_mcp: None,
            config_hooks: None,
            config_agents: None,
            config_usage: None,
            config_usage_range: "7d".to_string(),
            config_platform_usage: None,
            config_platform_usage_range: "7d".to_string(),
            config_platform_usage_supported: true,
            config_backups: None,
            config_app_update: None,
            config_workflows: None,
            config_system_stats: None,
            config_system_stats_error: None,
            config_system_stats_supported: true,
            update_channel: "stable".to_string(),
            reset_cards: None,
            reset_provider_id: None,
            reset_nonce: None,
            reset_idempotency_key: None,
            reset_eligible: Vec::new(),
            reset_credentials: true,
            reset_busy: false,
            pending_restart: false,
            instance_id: None,
            notice: None,
            notify: None,
            usage_stats: None,
            quota_unavailable: false,
        }
    }
}

impl ConfigState {
    /// The connection-reset patch (TS `connectionResetPatch`): configuration
    /// state describes ONE machine, so it is re-probed on the next hub, not
    /// carried across. `config_supported` matters most — a stale false is
    /// permanent (the probe only runs while the value is still None) — and
    /// `pending_restart` is the one that can do damage: left set across a
    /// switch, the next machine's screens offer a restart whose backend
    /// never had the write, cancelling its in-flight turns for nothing.
    pub fn connection_reset(&mut self) {
        *self = Self::default();
    }
}

/// Idempotency key for one reset-card GESTURE. A spent card is gone, so
/// the backend keys a spend on this value: a retry (dropped response,
/// double tap) answers the same outcome instead of burning a second card.
/// Generated ONCE per gesture and reused on every retry.
///
/// The backend contract fixes the shape at a UUID and refuses anything
/// else as code 3001 (parameter error), so the key is minted as
/// RFC 4122 v4.
pub fn new_idempotency_key() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    // RandomState seeds from the OS once per thread; hashing the clock and
    // the sequence through two fresh hashers (input order swapped) fills
    // the 16 bytes without pulling an RNG crate into the pure-logic layer.
    let seeded = |swap: bool| {
        let mut hasher = RandomState::new().build_hasher();
        if swap {
            seq.hash(&mut hasher);
            now.as_nanos().hash(&mut hasher);
        } else {
            now.as_nanos().hash(&mut hasher);
            seq.hash(&mut hasher);
        }
        hasher.finish()
    };
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&seeded(false).to_be_bytes());
    bytes[8..].copy_from_slice(&seeded(true).to_be_bytes());
    // Version 4, RFC 4122 variant.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// The configuration store over a shared Settings API client. Actions
/// mirror `configSlice.ts`; state reads mirror `useAppStore.getState()`.
pub struct ConfigStore<T> {
    client: HubHttpClient<T>,
    state: Mutex<ConfigState>,
}

impl<T: HttpTransport> ConfigStore<T> {
    pub fn new(client: HubHttpClient<T>) -> Self {
        Self {
            client,
            state: Mutex::new(ConfigState::default()),
        }
    }

    /// Zustand `getState()` — a consistent snapshot.
    pub fn state(&self) -> ConfigState {
        self.state.lock().unwrap().clone()
    }

    /// Zustand `setState(patch)` — the test seam the TS tests use directly
    /// (`useAppStore.setState({instanceId, pendingRestart})`).
    pub fn patch_state(&self, patch: impl FnOnce(&mut ConfigState)) {
        patch(&mut self.state.lock().unwrap());
    }

    /// The hub-switch / disconnect reset (see [`ConfigState::connection_reset`]).
    pub fn switch_server(&self) {
        self.state.lock().unwrap().connection_reset();
    }

    pub fn disconnect_hub(&self) {
        self.state.lock().unwrap().connection_reset();
    }

    /// Loads the /settings/all snapshot; a 404 marks the hub as too old.
    pub async fn load_config_all(&self) {
        self.patch_state(|s| {
            s.config_loading = true;
            s.config_error = None;
        });
        match self.client.settings_all().await {
            Ok(all) => {
                // The snapshot carries usage and reset-card eligibility, so
                // a section screen opening straight after the entry list
                // needs no second request for those two. Seed from clones;
                // the snapshot itself lands in `config_all` last.
                let usage = all.usage.clone();
                let usage_range = usage.as_ref().map(|u| u.range.as_str().to_string());
                let providers = all.reset_cards.providers.clone();
                let credentials = all.reset_cards.credentials;
                self.patch_state(|s| {
                    if let Some(usage) = usage {
                        s.config_usage = Some(usage);
                    }
                    if let Some(range) = usage_range {
                        s.config_usage_range = range;
                    }
                    // The hub reports the ids that COULD own cards, not the
                    // ones this account has — there is usually more than
                    // one and a picker is the norm. `credentials: false`
                    // means the store could not be decrypted, a different
                    // (non-fatal) state from having no plan at all.
                    s.reset_cards = None;
                    s.reset_provider_id = None;
                    s.reset_eligible = providers;
                    s.reset_credentials = credentials;
                    s.config_all = Some(all);
                    s.config_loading = false;
                    s.config_supported = Some(true);
                });
            }
            Err(HubApiError::Http { status: 404, .. }) => {
                // This hub predates the settings API (bridge < 0.47.0) — a
                // capability gap, not a failure; the screens explain it.
                self.patch_state(|s| {
                    s.config_supported = Some(false);
                    s.config_loading = false;
                });
            }
            Err(e) => {
                self.patch_state(|s| {
                    s.config_error = Some(e.to_string());
                    s.config_loading = false;
                });
            }
        }
    }

    /// Loads one section's own endpoint. `range` carries the section's
    /// option (the usage range) so a screen can refetch without the store
    /// knowing its controls.
    pub async fn load_config_section(&self, section: ConfigSection, range: Option<&str>) {
        self.patch_state(|s| {
            s.config_loading = true;
            s.config_error = None;
        });
        let result = self.run_section_load(section, range).await;
        match result {
            Ok(()) => self.patch_state(|s| {
                s.config_loading = false;
            }),
            Err(HubApiError::Http { status: 404, .. }) => self.patch_state(|s| {
                s.config_supported = Some(false);
                s.config_loading = false;
            }),
            Err(e) => self.patch_state(|s| {
                s.config_error = Some(e.to_string());
                s.config_loading = false;
            }),
        }
    }

    async fn run_section_load(
        &self,
        section: ConfigSection,
        range: Option<&str>,
    ) -> Result<(), HubApiError> {
        match section {
            ConfigSection::Models => {
                let v = self.client.settings_models().await?;
                self.patch_state(|s| s.config_models = Some(v));
            }
            ConfigSection::Skills => {
                let v = self.client.settings_skills().await?;
                self.patch_state(|s| s.config_skills = Some(v));
            }
            ConfigSection::Mcp => {
                let v = self.client.settings_mcp().await?;
                self.patch_state(|s| s.config_mcp = Some(v));
            }
            ConfigSection::Hooks => {
                let v = self.client.settings_hooks().await?;
                self.patch_state(|s| s.config_hooks = Some(v));
            }
            ConfigSection::Agents => {
                let v = self.client.settings_agents().await?;
                self.patch_state(|s| s.config_agents = Some(v));
            }
            ConfigSection::Backups => {
                let v = self.client.settings_backups().await?;
                self.patch_state(|s| s.config_backups = Some(v));
            }
            ConfigSection::AppUpdate => {
                let channel = self.state().update_channel.clone();
                let v = self.client.settings_app_update(Some(&channel)).await?;
                self.patch_state(|s| s.config_app_update = Some(v));
            }
            ConfigSection::Workflows => {
                let v = self.client.workflow_overview().await?;
                self.patch_state(|s| s.config_workflows = Some(v));
            }
            ConfigSection::Quota => {
                // The plan-quota screen reuses the hub-level quota the side
                // panels already show — one source of truth.
                self.refresh_usage_stats().await;
            }
            ConfigSection::Usage => {
                // The route wraps the payload: `{ok, usage}`. Storing the
                // whole response would make every `usage.available` read
                // undefined and the screen would show its "no data" empty
                // state forever.
                let requested = range.unwrap_or("7d").to_string();
                // Record the ask BEFORE awaiting: the guard below compares
                // against the newest request, and a read that lands first
                // must not overwrite the record of one still in flight.
                self.patch_state(|s| s.config_usage_range = requested.clone());
                let payload = self.client.settings_usage(&requested).await?;
                // Stale-response guard: the range buttons fire a fresh read
                // each tap and nothing serializes them — drop any answer
                // that no longer matches what the user asked for last.
                let answered = payload
                    .get("usage")
                    .and_then(|u| u.get("range"))
                    .and_then(Value::as_str)
                    .unwrap_or(&requested)
                    .to_string();
                if answered != self.state().config_usage_range {
                    return Ok(());
                }
                let usage = payload.get("usage").cloned();
                let decoded = usage
                    .map(|u| serde_json::from_value::<SettingsUsage>(u).map_err(|e| {
                        HubApiError::Unexpected(format!("invalid usage payload: {e}"))
                    }))
                    .transpose()?;
                self.patch_state(|s| s.config_usage = decoded);
            }
            ConfigSection::UsagePlatform => {
                // Same envelope pattern and the same stale-range guard as
                // the local usage read above; a 404 here retires ONLY the
                // platform tab (config_supported belongs to /settings/all).
                let requested = range.unwrap_or("7d").to_string();
                self.patch_state(|s| s.config_platform_usage_range = requested.clone());
                match self.client.settings_usage_platform(&requested).await {
                    Ok(payload) => {
                        let answered = payload
                            .get("platformUsage")
                            .and_then(|u| u.get("range"))
                            .and_then(Value::as_str)
                            .unwrap_or(&requested)
                            .to_string();
                        if answered != self.state().config_platform_usage_range {
                            return Ok(());
                        }
                        let platform = payload.get("platformUsage").cloned();
                        self.patch_state(|s| s.config_platform_usage = platform);
                    }
                    Err(HubApiError::Http { status: 404, .. }) => {
                        self.patch_state(|s| s.config_platform_usage_supported = false);
                    }
                    Err(e) => return Err(e),
                }
            }
            ConfigSection::System => self.load_system_stats().await,
        }
        Ok(())
    }

    /// Reads the machine-level system-status snapshot. 404 = the hub
    /// predates the route — retire this page alone. Anything else is a
    /// blip: keep the last snapshot, no error paint over working content;
    /// the FIRST load of a broken connection still needs a story.
    pub async fn load_system_stats(&self) {
        self.patch_state(|s| s.config_system_stats_error = None);
        match self.client.system_stats().await {
            Ok(stats) => self.patch_state(|s| {
                s.config_system_stats = Some(stats);
                s.config_system_stats_error = None;
            }),
            Err(HubApiError::Http { status: 404, .. }) => {
                self.patch_state(|s| s.config_system_stats_supported = false);
            }
            Err(e) => {
                let message = e.to_string();
                let had_data = self.state().config_system_stats.is_some();
                self.patch_state(|s| {
                    if !had_data {
                        s.config_system_stats_error = Some(message);
                    }
                });
            }
        }
    }

    /// Applies a write and reports whether it landed. The effect class is
    /// the whole point: an immediate write is done, a needs-restart one
    /// has not landed yet and the user must know which. `Ok(None)` means
    /// the write wrapper found no client — nothing was sent, and reporting
    /// success would show a toggle that moved on a machine that never
    /// heard about it.
    pub async fn apply_config_write<F, Fut>(&self, label: &str, write: F, reload: &[ConfigSection]) -> bool
    where
        F: FnOnce(&HubHttpClient<T>) -> Fut,
        Fut: std::future::Future<Output = Result<Option<Effect>, HubApiError>>,
    {
        match write(&self.client).await {
            // None means the write wrapper found no client — the connection
            // dropped while the screen was open, and NOTHING was sent.
            // Reporting that as success would show a toggle that moved on a
            // machine that never heard about it.
            Ok(None) => {
                self.notify(format!("{label} failed: not connected"));
                false
            }
            Ok(Some(Effect::NeedsRestart)) => {
                self.patch_state(|s| {
                    s.pending_restart = true;
                    s.notice = Some(NOTICE_NEEDS_RESTART.to_string());
                });
                for section in reload {
                    self.load_config_section(*section, None).await;
                }
                true
            }
            Ok(Some(Effect::Immediate)) => {
                for section in reload {
                    self.load_config_section(*section, None).await;
                }
                true
            }
            Err(e) => {
                self.notify(format!("{label} failed: {e}"));
                false
            }
        }
    }

    /// Reads the reset-card inventory for one provider. The previous
    /// provider's cards are dropped NOW, not when the read lands: while it
    /// is in flight the screen would still show provider A's cards, and if
    /// this read FAILS it would keep showing them — with a nonce the next
    /// spend then sends for a provider it is not looking at.
    pub async fn load_reset_cards(&self, provider_id: &str) {
        self.patch_state(|s| {
            s.reset_busy = true;
            s.reset_provider_id = Some(provider_id.to_string());
            s.reset_cards = None;
            s.reset_nonce = None;
            s.reset_idempotency_key = None;
        });
        match self.client.reset_card_status(provider_id).await {
            Ok(status) => {
                let nonce = status.reset_cards.nonce.clone();
                let cards = status.reset_cards;
                // The provider picker can fire two reads in quick
                // succession. Without this guard the SLOWER one lands last
                // and its nonce is stored alongside the FASTER one's
                // providerId — a spend then pairs a nonce minted for a
                // different provider, which the route always rejects with
                // 409, and the user sees a broken button with no cause to
                // guess.
                if self.state().reset_provider_id.as_deref() != Some(provider_id) {
                    return;
                }
                self.patch_state(|s| {
                    s.reset_cards = Some(cards);
                    s.reset_nonce = Some(nonce);
                    // Minted HERE, with the nonce it belongs to, so a
                    // spend's every retry sends the same key.
                    s.reset_idempotency_key = Some(new_idempotency_key());
                    s.reset_busy = false;
                });
            }
            Err(e) => {
                // Same guard: an error from a read the user has already
                // moved on from must not surface as a toast about a
                // provider they are not looking at.
                if self.state().reset_provider_id.as_deref() != Some(provider_id) {
                    return;
                }
                self.patch_state(|s| s.reset_busy = false);
                self.notify(format!("reset card status failed: {e}"));
            }
        }
    }

    /// Spends one reset card. Returns whether a card was actually used.
    pub async fn spend_reset_card(&self, input: SpendInput<'_>) -> bool {
        // hub.ts/configSlice guard: no provider or no nonce means there is
        // nothing spendable — refuse before anything goes out.
        if input.provider_id.is_empty() || input.nonce.is_empty() {
            return false;
        }
        // If the user has switched provider since the gesture began, this
        // nonce belongs to another provider's inventory and spending it now
        // would burn the wrong card.
        let current = self.state().reset_provider_id.clone();
        if let Some(current) = &current {
            if current != input.provider_id {
                return false;
            }
        }
        self.patch_state(|s| s.reset_busy = true);
        // The key minted with this nonce, NOT a fresh one. The backend keys
        // the spend on it, so a retry (the 502/504 the bridge answers when
        // a spend may already have been carried out) sends the same value
        // and gets the same outcome.
        let idempotency_key = self
            .state()
            .reset_idempotency_key
            .clone()
            .unwrap_or_else(new_idempotency_key);
        let result = self
            .client
            .spend_reset_card(crate::settings::SpendResetCard {
                provider_id: input.provider_id.to_string(),
                reset_type: input.reset_type,
                nonce: input.nonce.to_string(),
                idempotency_key: idempotency_key.clone(),
            })
            .await;
        match result {
            Ok(res) => {
                self.patch_state(|s| s.reset_busy = false);
                let used = res.get("used").and_then(Value::as_bool) != Some(false);
                self.patch_state(|s| {
                    s.notice = Some(
                        if used {
                            NOTICE_RESET_SPENT
                        } else {
                            NOTICE_RESET_SPEND_FAILED
                        }
                        .to_string(),
                    )
                });
                // Refresh both surfaces: the card list (one fewer card) and
                // the quota (the window it cleared). The refresh also
                // issues the next nonce.
                self.load_reset_cards(input.provider_id).await;
                self.refresh_usage_stats().await;
                used
            }
            Err(HubApiError::Http { status: 409, .. }) => {
                self.patch_state(|s| s.reset_busy = false);
                // 409 = the nonce is no longer the one the server issued
                // (a newer status read replaced it, or the user switched
                // provider). Re-read the status to mint a fresh one and say
                // so: without this the button looks broken and the only
                // escape is finding the refresh control by trial and
                // error. The route burns a nonce only AFTER the spend
                // settles, so a mid-request failure is NOT a 409 — the
                // same nonce and key still work on the retry path.
                self.load_reset_cards(input.provider_id).await;
                self.patch_state(|s| s.notice = Some(NOTICE_RESET_STALE.to_string()));
                false
            }
            Err(e) => {
                self.patch_state(|s| s.reset_busy = false);
                self.notify(format!("reset card spend failed: {e}"));
                false
            }
        }
    }

    /// Marks the reset history read (clearing an unread badge is cosmetic;
    /// a failure is not worth a toast).
    pub async fn mark_reset_history_read(&self) {
        let Some(provider_id) = self.state().reset_provider_id.clone() else {
            return;
        };
        if self
            .client
            .mark_reset_history_read(&provider_id)
            .await
            .is_ok()
        {
            self.load_reset_cards(&provider_id).await;
        }
    }

    /// Combined quota via the hub's /api/quota — pull-only, errors land in
    /// `quota_unavailable` (hide data, retry later), never in a toast.
    pub async fn refresh_usage_stats(&self) {
        match self.client.quota().await {
            Ok(result) => {
                // parseUsageStats: a payload that does not fit the combined
                // GLM + Opencode Go (+ optional Ollama) shape nulls the data
                // but is NOT a failed fetch — `quota_unavailable` stays
                // false, exactly like the TS store's set() on success.
                let parsed = serde_json::from_value::<AccountUsageStats>(result).ok();
                self.patch_state(|s| {
                    s.usage_stats = parsed;
                    s.quota_unavailable = false;
                })
            }
            Err(_) => self.patch_state(|s| {
                s.usage_stats = None;
                s.quota_unavailable = true;
            }),
        }
    }

    /// Restarts the backend of the instance that owns the configuration,
    /// so needs-restart writes take effect. Returns the interrupted-turn
    /// count (-1 = unavailable/failed).
    ///
    /// `closed: false` means the old subprocess survived — the writes this
    /// restart existed to apply did NOT take effect. Clearing the flag
    /// here would hide the affordance the user needs, so it stays armed.
    pub async fn restart_config_backend(&self) -> i64 {
        let Some(instance_id) = self.state().instance_id.clone() else {
            // No session open: no instance to address, restart unavailable.
            return -1;
        };
        match self.client.restart_backend(&instance_id).await {
            Ok(res) => {
                if res.closed {
                    self.patch_state(|s| {
                        s.pending_restart = false;
                        s.notice = Some(NOTICE_RESTARTED.to_string());
                    });
                } else {
                    self.patch_state(|s| s.notice = Some(NOTICE_RESTART_FAILED.to_string()));
                }
                res.cancelled_turns as i64
            }
            Err(e) => {
                self.notify(format!("backend restart failed: {e}"));
                -1
            }
        }
    }

    fn notify(&self, message: String) {
        self.patch_state(|s| s.notify = Some(message));
    }
}

#[cfg(test)]
mod tests {
    //! Port of `tests/config-store-rules.test.ts`. Port notes:
    //!
    //! - The TS suite resets a module-singleton store with `disconnectHub`;
    //!   Rust builds a fresh `ConfigStore` per test — same clean state.
    //! - `switchServer(id)` in TS resolves the target from the saved book;
    //!   the ported observable is the connection-reset patch it runs, so the
    //!   port calls [`ConfigStore::switch_server`].
    //! - Scripted bodies are completed to the shapes the SERVER sends (Rust
    //!   decodes strictly; the TS minimal mocks relied on unvalidated casts).

    use std::sync::Arc;

    use serde_json::{json, Value};

    use super::*;
    use crate::settings::McpServerUpsert;
    use crate::settings_client::test_support::{
        delayed, ok_json, refused, ScriptedTransport,
    };
    use crate::settings_client::HubHttpClient;

    const BIGMODEL: &str = "account:bigmodel-individual-coding-plan";
    const ZAI: &str = "account:zai-individual-coding-plan";

    fn transport() -> Arc<ScriptedTransport> {
        Arc::new(ScriptedTransport::new())
    }

    fn store(t: &Arc<ScriptedTransport>) -> ConfigStore<Arc<ScriptedTransport>> {
        ConfigStore::new(HubHttpClient::new("http://hub/", "tok", t.clone()))
    }

    /// A complete reset-card status body with the given nonce (the server
    /// always sends the whole inventory — coding-plan.ts:219-260).
    fn cards(nonce: &str) -> Value {
        json!({
            "ok": true,
            "resetCards": {
                "availableFiveHour": [],
                "availableWeek": [],
                "latestFiveHour": null,
                "latestWeek": null,
                "hasUnreadHistory": false,
                "nonce": nonce
            }
        })
    }

    /// A complete /settings/all body (server settings-endpoint.ts:631-672).
    fn settings_all_body() -> Value {
        json!({
            "ok": true,
            "models": {},
            "skills": [],
            "mcp": {},
            "hooks": {},
            "agents": [],
            "usage": {
                "available": false,
                "range": "7d",
                "summary": {
                    "totalTokens": 0, "inputTokens": 0, "outputTokens": 0,
                    "reasoningTokens": 0, "cacheReadTokens": 0, "requestCount": 0,
                    "sessionCount": 0, "turnCount": 0, "toolCallCount": 0,
                    "models": 0, "activeDays": 0
                },
                "models": [],
                "daily": []
            },
            "resetCards": { "providers": [], "credentials": true }
        })
    }

    /// A complete `{ok, usage}` envelope for one range.
    fn usage_body(range: &str) -> Value {
        json!({
            "ok": true,
            "usage": {
                "available": true,
                "range": range,
                "summary": {
                    "totalTokens": 1, "inputTokens": 0, "outputTokens": 0,
                    "reasoningTokens": 0, "cacheReadTokens": 0, "requestCount": 1,
                    "sessionCount": 0, "turnCount": 0, "toolCallCount": 0,
                    "models": 1, "activeDays": 1
                },
                "models": [],
                "daily": []
            }
        })
    }

    fn body_of(req: &crate::settings_client::HttpRequest) -> Value {
        let body = req.body.as_deref().expect("request had a body");
        serde_json::from_str(body).unwrap()
    }

    // --- the reset-card nonce ---------------------------------------------

    #[tokio::test]
    async fn a_spend_refused_with_409_refreshes_the_nonce_instead_of_leaving_it_dead() {
        let t = transport();
        let s = store(&t);
        // 409 means the nonce is no longer the one the server issued — a
        // newer status read replaced it, or the user switched provider. The
        // button would then be permanently dead with no way out but finding
        // the refresh control by trial and error.
        t.route("reset-cards", ok_json(cards("n-1")));
        s.load_reset_cards(BIGMODEL).await;
        assert_eq!(s.state().reset_nonce.as_deref(), Some("n-1"));

        t.route("reset-cards/use", refused(409, "nonce is stale — refresh the reset card status"));
        t.route("reset-cards?", ok_json(cards("n-2")));
        let spent = s
            .spend_reset_card(SpendInput {
                provider_id: BIGMODEL,
                nonce: "n-1",
                reset_type: ResetCardType::FiveHour,
            })
            .await;

        assert!(!spent);
        // A fresh status read replaced the dead nonce, so the next tap can
        // work — and the user is told why, rather than shown a bare failure.
        assert_eq!(s.state().reset_nonce.as_deref(), Some("n-2"));
        assert_eq!(s.state().notice.as_deref(), Some("notice.configResetStale"));
    }

    #[tokio::test]
    async fn a_spend_refused_for_another_reason_does_not_silently_reread_the_status() {
        let t = transport();
        let s = store(&t);
        t.route("reset-cards", ok_json(cards("n-1")));
        s.load_reset_cards(BIGMODEL).await;

        // A non-409 refusal (an eligibility or upstream failure) says
        // nothing about the nonce, so re-reading it would mask the real
        // error with a second request the user did not ask for.
        t.route("reset-cards/use", refused(403, "reset cards require an account coding-plan provider id"));
        let spent = s
            .spend_reset_card(SpendInput {
                provider_id: BIGMODEL,
                nonce: "n-1",
                reset_type: ResetCardType::FiveHour,
            })
            .await;

        assert!(!spent);
        assert_eq!(s.state().reset_nonce.as_deref(), Some("n-1"));
        // One status read and one refused spend — no extra read on the way
        // out.
        let reset_calls = t.requests_containing("/settings/reset-cards");
        assert_eq!(reset_calls.len(), 2);
    }

    #[tokio::test]
    async fn a_slow_status_read_cannot_overwrite_the_provider_the_user_switched_to() {
        let t = transport();
        let s = store(&t);
        // The provider picker fires a read per tap. The first call resolves
        // LAST — that is the race, and it is the response that would
        // otherwise land on top of the user's newer choice.
        t.route(
            "providerId=account%3Abigmodel-individual-coding-plan",
            delayed(ok_json(cards("n-bigmodel")), 20),
        );
        t.route(
            "providerId=account%3Azai-individual-coding-plan",
            ok_json(cards("n-zai")),
        );

        tokio::join!(
            s.load_reset_cards(BIGMODEL),
            s.load_reset_cards(ZAI),
        );

        // The nonce and the providerId must name the SAME provider, or the
        // spend pairs a nonce with a provider it was never minted for and
        // is refused with a 409 that reads as a broken button.
        assert_eq!(s.state().reset_provider_id.as_deref(), Some(ZAI));
        assert_eq!(s.state().reset_nonce.as_deref(), Some("n-zai"));
    }

    #[tokio::test]
    async fn a_spend_refuses_when_the_user_has_switched_provider_since_the_gesture_began() {
        let t = transport();
        let s = store(&t);
        t.route("reset-cards", ok_json(cards("n-1")));
        s.load_reset_cards(BIGMODEL).await;

        // The gesture captured provider A and its nonce. Before the spend
        // leaves, the user taps provider B. Spending A's nonce against B's
        // id is refused (the backend pairs them and answers 409, reading as
        // a broken button), and spending B's card when the user confirmed A
        // is worse, so the spend must not go out at all.
        t.route("reset-cards", ok_json(cards("n-2")));
        s.load_reset_cards(ZAI).await;

        let before = t.requests_containing("/reset-cards/use").len();
        let spent = s
            .spend_reset_card(SpendInput {
                provider_id: BIGMODEL,
                nonce: "n-1",
                reset_type: ResetCardType::FiveHour,
            })
            .await;

        assert!(!spent);
        assert_eq!(t.requests_containing("/reset-cards/use").len(), before);
    }

    // --- what survives a hub switch ----------------------------------------

    #[tokio::test]
    async fn switching_hubs_reprobes_the_settings_capability_instead_of_trusting_the_old_verdict()
    {
        let t = transport();
        let s = store(&t);
        // The first hub predates the settings API, so the probe answers 404
        // and the screens explain the gap.
        t.route("settings/all", refused(404, "not found"));
        s.load_config_all().await;
        assert_eq!(s.state().config_supported, Some(false));

        // The second hub is current. Its capability verdict must be
        // discovered, not inherited: `config_supported` only re-probes
        // while it is still null, so a stale false is permanent and every
        // config screen reports "unsupported".
        s.switch_server();
        assert_eq!(s.state().config_supported, None);

        t.route("settings/all", ok_json(settings_all_body()));
        s.load_config_all().await;
        assert_eq!(s.state().config_supported, Some(true));
    }

    #[tokio::test]
    async fn switching_hubs_drops_the_previous_machines_payloads() {
        let t = transport();
        let s = store(&t);
        t.route(
            "settings/models",
            ok_json(json!({ "ok": true, "models": { "available": [{ "modelId": "glm-4.6" }] } })),
        );
        s.load_config_section(ConfigSection::Models, None).await;
        assert!(s.state().config_models.is_some());

        s.switch_server();
        // Otherwise the new hub's model screen flashes the old machine's
        // list before its own payload lands.
        assert!(s.state().config_models.is_none());
        assert!(s.state().config_all.is_none());
    }

    #[tokio::test]
    async fn switching_hubs_clears_the_pending_restart_flag() {
        let t = transport();
        let s = store(&t);
        // A needs-restart write was made to THIS bridge, so the flag is
        // armed and the screens offer a restart. The next hub's backend
        // never saw that write — restarting it would cancel its in-flight
        // turns to apply nothing, so the flag must not travel with the
        // user.
        s.patch_state(|s| s.pending_restart = true);
        assert!(s.state().pending_restart);

        s.switch_server();
        assert!(!s.state().pending_restart);
    }

    #[tokio::test]
    async fn a_spend_reuses_the_idempotency_key_minted_with_its_nonce() {
        let t = transport();
        let s = store(&t);
        t.route("reset-cards", ok_json(cards("n-1")));
        s.load_reset_cards(BIGMODEL).await;
        let minted = s
            .state()
            .reset_idempotency_key
            .clone()
            .expect("key minted with the nonce");
        assert!(!minted.is_empty());

        t.route("reset-cards/use", ok_json(json!({ "ok": true, "used": true })));
        t.route("reset-cards?", ok_json(cards("n-2")));
        s.spend_reset_card(SpendInput {
            provider_id: BIGMODEL,
            nonce: "n-1",
            reset_type: ResetCardType::Week,
        })
        .await;

        // The key is what makes a RETRY safe: the backend keys the spend on
        // it, so the same value answers the same outcome instead of burning
        // a second card.
        let spend = &t.requests_containing("reset-cards/use")[0];
        assert_eq!(body_of(spend)["idempotencyKey"], json!(minted));
    }

    #[tokio::test]
    async fn a_retried_spend_after_a_timeout_sends_the_same_idempotency_key() {
        let t = transport();
        let s = store(&t);
        t.route("reset-cards", ok_json(cards("n-1")));
        s.load_reset_cards(BIGMODEL).await;
        let minted = s.state().reset_idempotency_key.clone().expect("minted");

        // First attempt: the bridge answers 502 — "your request may have
        // been carried out". The card may already be gone, so the client
        // MUST retry with the same key rather than mint a new one.
        t.route("reset-cards/use", refused(502, "coding_plan_reset_http_error:502"));
        s.spend_reset_card(SpendInput {
            provider_id: BIGMODEL,
            nonce: "n-1",
            reset_type: ResetCardType::Week,
        })
        .await;
        t.route("reset-cards/use", ok_json(json!({ "ok": true, "used": true })));
        t.route("reset-cards?", ok_json(cards("n-2")));
        s.spend_reset_card(SpendInput {
            provider_id: BIGMODEL,
            nonce: "n-1",
            reset_type: ResetCardType::Week,
        })
        .await;

        let spends = t.requests_containing("reset-cards/use");
        assert_eq!(spends.len(), 2);
        assert_eq!(body_of(&spends[0])["idempotencyKey"], json!(minted));
        assert_eq!(body_of(&spends[1])["idempotencyKey"], json!(minted));
    }

    #[tokio::test]
    async fn a_failed_provider_read_drops_the_previous_providers_cards() {
        let t = transport();
        let s = store(&t);
        let b = "account:zai-coding-plan";
        t.route("reset-cards", ok_json(cards("n-a")));
        s.load_reset_cards(BIGMODEL).await;
        assert_eq!(s.state().reset_nonce.as_deref(), Some("n-a"));

        // Provider B's read fails. Holding A's inventory while the user is
        // looking at B leaves a nonce that belongs to another plan's cards,
        // and the next spend sends it — the route always rejects a
        // mismatched nonce with 409, so the button dies with no visible
        // cause.
        t.route("reset-cards", refused(500, "credentials_unavailable"));
        s.load_reset_cards(b).await;
        assert_eq!(s.state().reset_provider_id.as_deref(), Some(b));
        assert!(s.state().reset_nonce.is_none());
        assert!(s.state().reset_idempotency_key.is_none());
        assert!(s.state().reset_cards.is_none());
    }

    #[tokio::test]
    async fn a_usage_response_for_a_superseded_range_is_dropped() {
        let t = transport();
        let s = store(&t);
        // "All" answers slowly, "7d" fast — the order the taps happen in is
        // not the order the answers arrive in, and nothing serializes the
        // two reads.
        t.route("settings/usage?range=all", delayed(ok_json(usage_body("all")), 60));
        t.route("settings/usage?range=7d", delayed(ok_json(usage_body("7d")), 5));

        tokio::join!(
            s.load_config_section(ConfigSection::Usage, Some("all")),
            s.load_config_section(ConfigSection::Usage, Some("7d")),
        );

        // The 7d answer is the newest ask, so it is what the screen shows.
        // Storing the slower All response underneath it would put
        // whole-history numbers under the 7d button.
        assert_eq!(s.state().config_usage_range, "7d");
        assert_eq!(
            s.state().config_usage.as_ref().map(|u| u.range.as_str()),
            Some("7d")
        );
    }

    // --- the backend restart flag -------------------------------------------

    #[tokio::test]
    async fn a_backend_restart_that_did_not_close_the_old_process_keeps_the_flag_armed() {
        let t = transport();
        let s = store(&t);
        s.patch_state(|s| {
            s.instance_id = Some("1".to_string());
            s.pending_restart = true;
        });
        // `closed: false` means the old subprocess survived, so the writes
        // this restart existed to apply did NOT take effect. Clearing the
        // flag here would remove the only affordance that can apply them,
        // leaving the user with saved changes that never land and no
        // button to press.
        t.route("backend/restart", ok_json(json!({ "cancelledTurns": 0, "closed": false })));
        s.restart_config_backend().await;
        assert!(s.state().pending_restart);
    }

    #[tokio::test]
    async fn a_backend_restart_that_closed_the_old_process_clears_the_flag() {
        let t = transport();
        let s = store(&t);
        s.patch_state(|s| {
            s.instance_id = Some("1".to_string());
            s.pending_restart = true;
        });
        t.route("backend/restart", ok_json(json!({ "cancelledTurns": 2, "closed": true })));
        let cancelled = s.restart_config_backend().await;
        assert_eq!(cancelled, 2);
        assert!(!s.state().pending_restart);
    }

    #[tokio::test]
    async fn a_restart_with_no_instance_connected_is_unavailable() {
        let t = transport();
        let s = store(&t);
        assert_eq!(s.restart_config_backend().await, -1);
    }

    // --- the effect class of a write (apply_config_write) --------------------

    #[tokio::test]
    async fn a_needs_restart_write_arms_the_restart_affordance() {
        let t = transport();
        let s = store(&t);
        t.route(
            "settings/mcp/ctx7",
            ok_json(json!({ "ok": true, "effect": "needs-restart" })),
        );
        // The closure clones the client so the returned future owns it
        // (apply_config_write hands it a short-lived borrow).
        let wrote = |c: &HubHttpClient<Arc<ScriptedTransport>>| {
            let c = c.clone();
            async move {
                c.upsert_mcp_server("ctx7", McpServerUpsert::default())
                    .await
                    .map(|w| w.effect)
            }
        };
        let ok = s.apply_config_write("update mcp", wrote, &[]).await;
        assert!(ok);
        // The write has NOT landed yet — the flag is what surfaces that.
        assert!(s.state().pending_restart);
        assert_eq!(s.state().notice.as_deref(), Some("notice.configNeedsRestart"));
    }

    #[tokio::test]
    async fn an_immediate_write_does_not_arm_the_restart_affordance() {
        let t = transport();
        let s = store(&t);
        t.route(
            "settings/models",
            ok_json(json!({ "ok": true, "effect": "immediate" })),
        );
        let wrote = |c: &HubHttpClient<Arc<ScriptedTransport>>| {
            let c = c.clone();
            async move {
                c.add_model(crate::settings::ModelUpsert {
                    provider_id: "builtin:x".into(),
                    model_id: "m".into(),
                    ..Default::default()
                })
                .await
                .map(|w| w.effect)
            }
        };
        let ok = s.apply_config_write("add model", wrote, &[]).await;
        assert!(ok);
        assert!(!s.state().pending_restart);
        assert!(s.state().notice.is_none());
    }

    #[tokio::test]
    async fn a_refused_write_surfaces_its_reason_and_reports_failure() {
        let t = transport();
        let s = store(&t);
        t.route("settings/agents", refused(400, "that is a built-in agent"));
        let wrote = |c: &HubHttpClient<Arc<ScriptedTransport>>| {
            let c = c.clone();
            async move {
                c.upsert_agent("general-purpose", crate::settings::AgentUpsert::default())
                    .await
                    .map(|w| w.effect)
            }
        };
        let ok = s.apply_config_write("upsert agent", wrote, &[]).await;
        assert!(!ok);
        assert!(!s.state().pending_restart);
        let notify = s.state().notify.expect("failure notified");
        assert_eq!(notify, "upsert agent failed: that is a built-in agent");
    }
}
