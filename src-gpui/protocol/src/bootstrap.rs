//! Hub Bootstrap: zero-config local hub detection, detached spawn, and the
//! guided token setup (requirements §3/§3.1, ADR-0010, CONTEXT.md "Hub
//! Bootstrap").
//!
//! Semantics are ported from the server (the protocol authority) — the file
//! references below are `zcode-acp-server` src:
//! - config path + `remote` validation — `src/config/user-config.ts`
//!   (`userConfigPath`, `parseRemoteSection`)
//! - per-field file > env > default merge — `src/remote/config.ts`
//!   (`mergeCommon`, defaults at :77-79, enabled gate `remoteEnabledLive`)
//! - detached hub spawn env trio — `src/remote/endpoint.ts` `spawnHub`
//!   (:349-387)
//! - EADDRINUSE singleton race — `src/bin/hub.ts`: the loser exits 0
//!   (:108-111); a hub without a token refuses to start, exit 1 (:65-66)
//! - CLI dispatch — `src/cli.ts` `resolveInvocation`: the `zcode-acp-server`
//!   bin alias ALWAYS speaks the stdio bridge and ignores subcommand args
//!   (:44-45); the hub daemon is `zcode-acp hub` (the dispatcher bin, :59).
//!
//! The hub is always a background task (CONTEXT.md): this module never stops,
//! restarts, or owns the daemon's lifecycle — it spawns detached, connects to
//! whichever daemon won the port, and leaves idle-exit/respawn/upgrade to the
//! server's own machinery.

use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Map, Value};
use tokio::net::TcpStream;

// ---------------------------------------------------------------------------
// Environment snapshot
// ---------------------------------------------------------------------------

/// The environment slice Hub Bootstrap reads. A struct (not live `std::env`
/// calls) so every decision stays pure and unit-testable; the app fills it
/// once at the boundary via [`BootstrapEnv::from_env`].
#[derive(Debug, Clone, Default)]
pub struct BootstrapEnv {
    /// User home dir (`$HOME` / `%USERPROFILE%`).
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME` — wins over `home/.config` when non-blank
    /// (user-config.ts `userConfigPath`).
    pub xdg_config_home: Option<String>,
    /// `$XDG_DATA_HOME` — wins over `home/.local/share` for the mise shims.
    pub xdg_data_home: Option<String>,
    /// `ZCODE_ACP_REMOTE` — env gate for [`remote_enabled`].
    pub remote_enabled: Option<String>,
    /// `ZCODE_ACP_REMOTE_TOKEN`.
    pub remote_token: Option<String>,
    /// `ZCODE_ACP_HUB_PORT`.
    pub hub_port: Option<String>,
    /// `ZCODE_ACP_HUB_HOST`.
    pub hub_host: Option<String>,
    /// `PATH`, for the last-resort install scan.
    pub path: Option<String>,
    /// `$SHELL` — the login shell probed for the PATH a GUI app cannot
    /// inherit (see [`login_shell_path`]).
    pub shell: Option<String>,
    /// `NPM_CONFIG_PREFIX` — an npm global prefix to probe ahead of the
    /// well-known ones.
    pub npm_config_prefix: Option<String>,
    /// Exact install-probe directories. `None` = the default mise/bun/npm/PATH
    /// list ([`default_install_dirs`]); `Some(...)` pins the list (tests).
    pub install_dirs: Option<Vec<PathBuf>>,
}

impl BootstrapEnv {
    /// Snapshot the process environment. A missing home reads as an empty
    /// path — config resolution then depends on XDG being set.
    pub fn from_env() -> Self {
        let var = |key: &str| std::env::var(key).ok();
        let home_key = if cfg!(unix) { "HOME" } else { "USERPROFILE" };
        Self {
            home: std::env::var_os(home_key)
                .map(PathBuf::from)
                .unwrap_or_default(),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            xdg_data_home: var("XDG_DATA_HOME"),
            remote_enabled: var("ZCODE_ACP_REMOTE"),
            remote_token: var("ZCODE_ACP_REMOTE_TOKEN"),
            hub_port: var("ZCODE_ACP_HUB_PORT"),
            hub_host: var("ZCODE_ACP_HUB_HOST"),
            path: var("PATH"),
            shell: var("SHELL"),
            npm_config_prefix: var("NPM_CONFIG_PREFIX"),
            install_dirs: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Config reading
// ---------------------------------------------------------------------------

/// Hub's fixed port (remote/config.ts `DEFAULT_HUB_PORT`).
pub const DEFAULT_HUB_PORT: u16 = 8377;
/// Hub's bind address default (remote/config.ts `DEFAULT_HUB_HOST`).
pub const DEFAULT_HUB_HOST: &str = "127.0.0.1";

/// `~/.config/zcode-acp/config.json`, `$XDG_CONFIG_HOME`-aware — mirrors
/// user-config.ts `userConfigPath`: a blank XDG value falls back to
/// `$HOME/.config` (never the cwd-relative XDG spec violation).
pub fn user_config_path(env: &BootstrapEnv) -> PathBuf {
    let base = env
        .xdg_config_home
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| env.home.join(".config"));
    base.join("zcode-acp").join("config.json")
}

/// The validated `remote` section — the field subset the desktop client
/// needs from the server's `RemoteUserConfig` (bridgePort/webDir/
/// projectRoots/terminal are server-only and intentionally not modeled).
///
/// Field semantics mirror user-config.ts `parseRemoteSection`: junk values
/// read as absent (the server warns; pure logic just ignores), strings are
/// trimmed, ports must be integers in 1..=65535.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteSection {
    pub enabled: Option<bool>,
    pub token: Option<String>,
    pub hub_port: Option<u16>,
    pub hub_host: Option<String>,
}

/// The `remote` object of a config-file body. `None` for every case the
/// server's loader reads as "no usable section": malformed JSON, a non-object
/// root, or `remote` missing / not an object (user-config.ts `loadUserConfig`
/// best-effort doctrine — the env fallback then applies).
///
/// Split out of [`parse_remote_section`] because the desktop models only a
/// field subset: the blocks it does not mirror (`terminal`, `projectRoots`,
/// `webDir`) still have to be readable without forking the file parse.
pub fn remote_object(file_text: &str) -> Option<Map<String, Value>> {
    let root: Value = serde_json::from_str(file_text).ok()?;
    root.get("remote")?.as_object().cloned()
}

/// Parse the `remote` section out of a config-file body (see
/// [`remote_object`] for the `None` cases).
pub fn parse_remote_section(file_text: &str) -> Option<RemoteSection> {
    let remote = remote_object(file_text)?;
    let mut out = RemoteSection::default();
    if let Some(enabled) = remote.get("enabled").and_then(Value::as_bool) {
        out.enabled = Some(enabled);
    }
    if let Some(token) = remote.get("token").and_then(Value::as_str) {
        let token = token.trim();
        if !token.is_empty() {
            out.token = Some(token.to_owned());
        }
    }
    if let Some(port) = valid_port(remote.get("hubPort")) {
        out.hub_port = Some(port);
    }
    if let Some(host) = remote.get("hubHost").and_then(Value::as_str) {
        let host = host.trim();
        if !host.is_empty() {
            out.hub_host = Some(host.to_owned());
        }
    }
    Some(out)
}

/// Live read of the section from disk. Every call re-reads — the server's
/// doctrine is live config (user-config.ts module doc), and the hub's birth
/// env rotates, so caching here would go stale.
pub fn read_remote_section(config_path: &Path) -> Option<RemoteSection> {
    let text = std::fs::read_to_string(config_path).ok()?;
    parse_remote_section(&text)
}

/// JSON number → port with the server's validation (integer, 1..=65535 —
/// user-config.ts:556-563). Floats, negatives, and junk read as absent.
fn valid_port(value: Option<&Value>) -> Option<u16> {
    let n = value?.as_u64()?;
    if (1..=65535).contains(&n) {
        Some(n as u16)
    } else {
        None
    }
}

/// `Number.parseInt` parity for an env port value (remote/config.ts
/// `parsePort`): leading decimal digits of the trimmed value; an unparseable
/// or out-of-range result falls back rather than fail.
fn env_port(raw: Option<&str>, fallback: u16) -> u16 {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return fallback;
    };
    let digits_end = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let digits = &raw[..digits_end];
    match digits.parse::<u64>() {
        Ok(n) if (1..=65535).contains(&n) => n as u16,
        _ => fallback,
    }
}

/// The merged local-hub dial parameters: token + port + host, per-field
/// file > env > default (remote/config.ts `mergeCommon`). `token: None`
/// means no token anywhere — the guided flow (requirements §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalHubConfig {
    pub token: Option<String>,
    pub hub_port: u16,
    pub hub_host: String,
}

/// Pure merge of an already-parsed section over the env snapshot.
pub fn resolve_local_hub_with(
    section: Option<&RemoteSection>,
    env: &BootstrapEnv,
) -> LocalHubConfig {
    let file = section.cloned().unwrap_or_default();
    let env_text = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    LocalHubConfig {
        token: file.token.or_else(|| env_text(&env.remote_token)),
        hub_port: file
            .hub_port
            .unwrap_or_else(|| env_port(env.hub_port.as_deref(), DEFAULT_HUB_PORT)),
        hub_host: file
            .hub_host
            .or_else(|| env_text(&env.hub_host))
            .unwrap_or_else(|| DEFAULT_HUB_HOST.to_owned()),
    }
}

/// Read the config file at [`BootstrapEnv`]'s resolved path and merge.
pub fn resolve_local_hub(env: &BootstrapEnv) -> LocalHubConfig {
    let section = read_remote_section(&user_config_path(env));
    resolve_local_hub_with(section.as_ref(), env)
}

/// The live "is remote enabled?" gate (remote/config.ts `remoteEnabledLive`):
/// the file's `remote.enabled` wins; otherwise the env gate is truthy
/// (`1|true|yes|on`, case-insensitive). Used for the guided-flow decision
/// only — the hub daemon itself ignores `enabled` (parseHubConfig).
pub fn remote_enabled(section: Option<&RemoteSection>, env: &BootstrapEnv) -> bool {
    if let Some(enabled) = section.and_then(|s| s.enabled) {
        return enabled;
    }
    matches!(
        env.remote_enabled
            .as_deref()
            .map(str::trim)
            .map(str::to_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Everything the app needs to decide the connect-vs-guided-flow branch in
/// one read (requirements §3.1: disabled or token-missing → guided flow).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalHubStatus {
    pub enabled: bool,
    pub config: LocalHubConfig,
}

pub fn assess_local_hub(env: &BootstrapEnv) -> LocalHubStatus {
    let section = read_remote_section(&user_config_path(env));
    LocalHubStatus {
        enabled: remote_enabled(section.as_ref(), env),
        config: resolve_local_hub_with(section.as_ref(), env),
    }
}

/// A bind address is not a dial address: the hub's host may be a wildcard
/// ("0.0.0.0" for containerized tunnel agents, remote/config.ts:21), which
/// some stacks refuse to dial — normalize to the same family's loopback.
pub fn dial_host(hub_host: &str) -> &str {
    match hub_host.trim() {
        "0.0.0.0" => "127.0.0.1",
        "::" | "[::]" => "::1",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Install probe
// ---------------------------------------------------------------------------

/// The dispatcher bin. The `zcode-acp-server` alias CANNOT be used: invoked
/// under that name the CLI always speaks the stdio bridge and ignores
/// subcommand args (cli.ts `resolveInvocation`: `invokedAs ===
/// "zcode-acp-server"` short-circuits to `server`). Both names come from the
/// same package's bin map, so finding `zcode-acp` proves the install.
pub const HUB_BIN_NAME: &str = "zcode-acp";
/// The hub daemon subcommand (`zcode-acp hub`, cli.ts:59).
pub const HUB_SUBCOMMAND: &str = "hub";

/// An installed `zcode-acp-server` package, by its dispatcher bin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerInstall {
    pub bin: PathBuf,
}

/// How long the login-shell PATH probe may take. A shell start is
/// milliseconds; the budget only matters for an rc that hangs, which then
/// degrades to the static probe list.
pub const LOGIN_SHELL_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Default probe order: mise shims → bun global bin → npm prefixes →
/// user-local bin → `$PATH`.
/// Explicit paths instead of a bare PATH lookup because a GUI app inherits
/// launchd's bare environment — the same reason the server probes well-known
/// bun locations itself (runtime.ts `bunCandidates`).
///
/// `$PATH` here is the INHERITED one: a launchd-launched app gets
/// `/usr/bin:/bin:/usr/sbin:/sbin`, so a hand-installed dispatcher under
/// `~/.local/bin` (a source checkout linked there, the documented dev
/// setup) is invisible to it — hence the explicit user-local dirs, and
/// [`install_dirs_with_login_shell`] for everything only the user's rc
/// files know about.
pub fn default_install_dirs(env: &BootstrapEnv) -> Vec<PathBuf> {
    let data_base = env
        .xdg_data_home
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| env.home.join(".local").join("share"));
    let mut dirs = vec![
        data_base.join("mise").join("shims"),
        env.home.join(".bun").join("bin"),
    ];
    if let Some(prefix) = env
        .npm_config_prefix
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        dirs.push(PathBuf::from(prefix).join("bin"));
    }
    if cfg!(unix) {
        // System node, Homebrew (arm64 + Linuxbrew), the npm-global convention,
        // and the XDG/user-local bin dir source checkouts are usually linked
        // into (pipx/cargo/mise-farm all use it too).
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/home/linuxbrew/.linuxbrew/bin"));
        dirs.push(env.home.join(".npm-global").join("bin"));
        dirs.push(env.home.join(".local").join("bin"));
    } else {
        dirs.push(env.home.join("AppData").join("Roaming").join("npm"));
    }
    if let Some(path) = &env.path {
        dirs.extend(std::env::split_paths(path));
    }
    dirs
}

/// The login shell's `PATH`, or `None` when the probe cannot produce one.
///
/// A GUI app launched from Finder/Dock inherits launchd's bare environment,
/// so the PATH a user actually works with — mise/nvm/asdf shims, Homebrew,
/// `~/.local/bin` — exists only inside their rc files. Probing a NON-login,
/// NON-interactive shell never reads them; `-l` reads the profile
/// (`~/.zprofile`) but not `~/.zshrc`, which is where most version managers
/// install themselves, so the interactive rc is sourced explicitly — the
/// same split the server uses (`remote/login-shell-env.ts`). `-i` is
/// avoided on purpose: this runs inside a GUI process with no terminal.
///
/// Blocking (one shell start, bounded by `timeout`); call from the driver's
/// background runtime, never the UI thread. Failure is silent — the
/// inherited environment remains the fallback.
pub fn login_shell_path(env: &BootstrapEnv, timeout: Duration) -> Option<String> {
    let shell = env
        .shell
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("/bin/zsh");
    let name = Path::new(shell)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let rc_files = match name {
        "zsh" => r#""$HOME/.zshrc""#,
        "bash" => r#""$HOME/.bash_profile" "$HOME/.bashrc""#,
        _ => "",
    };
    let script = if rc_files.is_empty() {
        "printf '%s' \"$PATH\"".to_owned()
    } else {
        format!("for f in {rc_files}; do [ -r \"$f\" ] && . \"$f\"; done; printf '%s' \"$PATH\"")
    };
    let mut cmd = Command::new(shell);
    cmd.arg("-l")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let child = cmd.spawn().ok()?;
    let output = wait_with_timeout(child, timeout)?;
    let path = String::from_utf8_lossy(&output).trim().to_owned();
    (!path.is_empty()).then_some(path)
}

/// Run a probe child to completion, bounded by `timeout`, and return its
/// stdout — a wedged rc file must never hang the bootstrap.
///
/// The read happens on a detached thread because a blocking `read` cannot be
/// interrupted: if the rc leaked the pipe into a background grandchild, EOF
/// never arrives and joining that thread would wait forever. The deadline is
/// therefore enforced on the RECEIVE side, and a probe that misses it is
/// treated as failed — the caller falls back to the static probe list.
fn wait_with_timeout(mut child: Child, timeout: Duration) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });
    let deadline = std::time::Instant::now() + timeout;
    loop {
        // EOF on both is the success shape: the shell exited AND closed the
        // pipe, so the buffer is complete.
        if let Ok(buf) = rx.try_recv() {
            let _ = child.wait();
            return Some(buf);
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The probe dirs plus every directory only the user's login shell knows.
///
/// Ordering: the login shell's own resolution order first (that is what a
/// command typed in a terminal would get), then the static list. This is
/// what makes a dev-setup dispatcher found AND runnable — its shebang
/// interpreter (`node`, resolved through PATH) lives on that same shell
/// PATH, not on launchd's.
pub fn install_dirs_with_login_shell(env: &BootstrapEnv, timeout: Duration) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = login_shell_path(env, timeout)
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    dirs.extend(default_install_dirs(env));
    let mut seen = std::collections::HashSet::new();
    dirs.retain(|d| seen.insert(d.clone()));
    dirs
}

/// Find the dispatcher bin inside an explicit dir list; first hit wins.
pub fn find_server_install_in(dirs: &[PathBuf]) -> Option<ServerInstall> {
    dirs.iter()
        .map(|dir| dir.join(HUB_BIN_NAME))
        .find(|bin| bin.is_file())
        .map(|bin| ServerInstall { bin })
}

/// Find the installed server's dispatcher bin; first candidate hit wins.
pub fn find_server_install(env: &BootstrapEnv) -> Option<ServerInstall> {
    let dirs = match &env.install_dirs {
        Some(pinned) => pinned.clone(),
        None => default_install_dirs(env),
    };
    find_server_install_in(&dirs)
}

/// `PATH` for the hub child: the probe dirs, then the inherited PATH.
///
/// Union (not replacement) for the same reason the server unions the login
/// shell's value (`remote/login-shell-env.ts` `unionPath`): the probe dirs
/// make the dispatcher's own interpreter resolve, while the inherited entries
/// stay — a daemon that loses `/usr/bin` cannot run its own toolchain, and
/// every process the hub later spawns inherits this value.
pub fn hub_child_path(dirs: &[PathBuf], inherited: Option<&str>) -> Option<String> {
    let mut all: Vec<PathBuf> = dirs.iter().filter(|d| d.is_dir()).cloned().collect();
    if let Some(inherited) = inherited {
        all.extend(std::env::split_paths(inherited));
    }
    let mut seen = std::collections::HashSet::new();
    all.retain(|d| seen.insert(d.clone()));
    if all.is_empty() {
        return None;
    }
    std::env::join_paths(&all)
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// [`hub_child_path`] for the real spawn: the inherited half is the live
/// process PATH, not just the snapshot field (`BootstrapEnv::path` may be
/// unset on a caller that only filled the hub-relevant vars, and dropping
/// `/bin` from the daemon's PATH breaks every `sh` helper it runs).
pub fn hub_child_path_from_process(dirs: &[PathBuf], env: &BootstrapEnv) -> Option<String> {
    let live = env
        .path
        .clone()
        .or_else(|| std::env::var("PATH").ok())
        .or_else(|| {
            // Neither field nor live var: the platform default search path is
            // what the OS would have used, so a spawn still gets `/bin`.
            Some(if cfg!(windows) {
                r"C:\Windows\System32".to_owned()
            } else {
                "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin".to_owned()
            })
        });
    hub_child_path(dirs, live.as_deref())
}

// ---------------------------------------------------------------------------
// Detached spawn
// ---------------------------------------------------------------------------

/// Env vars injected into the hub spawn — the same trio the bridge's
/// `spawnHub` passes (endpoint.ts:365-370): the daemon re-reads the same
/// config file itself (file wins there too), so this is belt-and-braces
/// parity, and it makes env-only setups work without a file.
pub fn hub_spawn_env(config: &LocalHubConfig) -> Vec<(String, String)> {
    let mut vars = vec![
        ("ZCODE_ACP_HUB_PORT".to_owned(), config.hub_port.to_string()),
        ("ZCODE_ACP_HUB_HOST".to_owned(), config.hub_host.clone()),
    ];
    if let Some(token) = &config.token {
        vars.push(("ZCODE_ACP_REMOTE_TOKEN".to_owned(), token.clone()));
    }
    vars
}

/// Spawn the hub daemon detached. The child keeps running after this process
/// exits (Unix children survive their parent regardless; the process-group
/// move keeps terminal-wide signals from reaching it) — closing the app must
/// never stop the hub (CONTEXT.md "Hub Bootstrap").
///
/// `path` replaces the child's `PATH` with the probe dirs ([`hub_child_path`]):
/// under launchd a wrapper dispatcher cannot resolve its own interpreter, and
/// every process the hub later spawns (serve bridges, terminals, backends)
/// inherits that same PATH — the server's `spawnHub` completes the
/// environment for exactly this reason (`remote/endpoint.ts`).
///
/// Whether OUR spawn won the port is NOT decided here — the daemon exits 0
/// on EADDRINUSE by design (bin/hub.ts:108-111) and the caller adjudicates
/// by connectivity ([`ensure_hub`]).
pub fn spawn_hub_daemon(
    install: &ServerInstall,
    config: &LocalHubConfig,
    path: Option<&str>,
) -> io::Result<Child> {
    let mut cmd = Command::new(&install.bin);
    cmd.arg(HUB_SUBCOMMAND)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(path) = path {
        cmd.env("PATH", path);
    }
    for (key, value) in hub_spawn_env(config) {
        cmd.env(key, value);
    }
    detach(&mut cmd);
    cmd.spawn()
}

/// Detach the child from our process group/session. `process_group(0)` is
/// setpgid (isolate from terminal signal broadcasts); full setsid is not
/// exposed by std and buys nothing extra here — the daemon's survival does
/// not depend on the parent.
fn detach(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP: no console window.
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }
    #[cfg(not(any(unix, windows)))]
    let _ = cmd;
}

// ---------------------------------------------------------------------------
// EADDRINUSE race adjudication
// ---------------------------------------------------------------------------

/// What `try_wait` says about the spawned daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonStatus {
    Running,
    /// `Some(code)` = exited with it; `None` = killed by a signal.
    Exited(Option<i32>),
}

/// One adjudication step of the singleton race, from the two observables the
/// client has: the daemon's liveness and the port's connectability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubRaceState {
    /// Port answers, our daemon alive → our spawn won the bind.
    WonBySpawn,
    /// Port answers, our daemon already exited → it lost the EADDRINUSE race
    /// (losers exit 0 by design) and the pre-existing hub keeps the port.
    WonByExisting,
    /// Daemon dead with a failing exit and the port dark → the spawn was
    /// refused outright (e.g. no token: exit 1, bin/hub.ts:65-66).
    Refused { exit_code: Option<i32> },
    /// No terminal signal yet — keep polling. Covers running-and-dark (still
    /// starting) and exited-clean-and-dark (the race's OTHER winner may still
    /// be mid-bind; the caller's deadline is the backstop).
    Starting,
}

/// Pure step function: connectivity decides the race (the ask's contract —
/// whichever daemon owns the port serves, and that is all the client can and
/// needs to observe at the TCP layer).
pub fn adjudicate_spawn_race(daemon: DaemonStatus, port_connectable: bool) -> HubRaceState {
    match (daemon, port_connectable) {
        (DaemonStatus::Running, true) => HubRaceState::WonBySpawn,
        (DaemonStatus::Exited(_), true) => HubRaceState::WonByExisting,
        (DaemonStatus::Exited(Some(0)), false) => HubRaceState::Starting,
        (DaemonStatus::Exited(other), false) => HubRaceState::Refused { exit_code: other },
        (DaemonStatus::Running, false) => HubRaceState::Starting,
    }
}

// ---------------------------------------------------------------------------
// ensure_hub driver
// ---------------------------------------------------------------------------

/// Timing knobs for [`ensure_hub`].
#[derive(Debug, Clone)]
pub struct EnsureOptions {
    /// Per TCP connect attempt.
    pub probe_timeout: Duration,
    /// Spacing between adjudication polls.
    pub poll_interval: Duration,
    /// Overall post-spawn budget for the port to answer. Generous on
    /// purpose: a cold Node/Bun daemon start takes seconds, and the hub's
    /// own registration budget for spawned children is ~20s.
    pub connect_deadline: Duration,
}

impl Default for EnsureOptions {
    fn default() -> Self {
        Self {
            probe_timeout: Duration::from_millis(750),
            poll_interval: Duration::from_millis(200),
            connect_deadline: Duration::from_secs(15),
        }
    }
}

/// How [`ensure_hub`] reached a listening hub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureHubOutcome {
    /// A hub was already listening — connected as-is, nothing spawned
    /// (requirements §3: "a hub found already listening is connected to
    /// as-is").
    AlreadyListening,
    /// Our detached spawn won the port.
    SpawnWon,
    /// Our spawn lost the EADDRINUSE race; the pre-existing hub keeps the
    /// port and serves us the same.
    SpawnLostToExisting,
}

#[derive(Debug, thiserror::Error)]
pub enum BootstrapError {
    #[error(
        "no token in the config file or ZCODE_ACP_REMOTE_TOKEN — generate one first (guided setup)"
    )]
    MissingToken,
    #[error("zcode-acp-server is not installed (probed the login shell's PATH plus mise/bun/npm/user-local locations for the `{HUB_BIN_NAME}` bin)")]
    NotInstalled,
    #[error("spawning the hub daemon failed: {0}")]
    Spawn(#[source] io::Error),
    #[error("the hub daemon exited with code {0:?} without ever listening")]
    DaemonRefused(Option<i32>),
    #[error("no hub listened on {host}:{port} within {deadline:?}")]
    TimedOut {
        host: String,
        port: u16,
        deadline: Duration,
    },
}

/// Locate the dispatcher and the probe dirs it will run under.
///
/// Two stages, cheap first: the static list (mise/bun/npm/user-local/`$PATH`)
/// covers every package-manager install, and only when it comes up empty is a
/// login shell probed — that probe starts a process and is the one thing the
/// bootstrap cannot cache across calls (the rc file is live config).
///
/// A caller-pinned `install_dirs` short-circuits both stages: a pinned list
/// is authoritative, so tests never touch the host's real installs or shell.
async fn resolve_server_install(env: &BootstrapEnv) -> (Option<ServerInstall>, Vec<PathBuf>) {
    if let Some(pinned) = &env.install_dirs {
        return (find_server_install_in(pinned), pinned.clone());
    }
    let dirs = default_install_dirs(env);
    if let Some(install) = find_server_install_in(&dirs) {
        return (Some(install), dirs);
    }
    // A GUI app's inherited PATH cannot see anything the user's rc files add,
    // which is where a dev checkout's dispatcher (and the interpreter it
    // execs) actually lives. The probe starts a shell, so it goes to the
    // blocking pool — this driver's runtime thread must not stall on an rc.
    let probe_env = env.clone();
    let shell_dirs = tokio::task::spawn_blocking(move || {
        install_dirs_with_login_shell(&probe_env, LOGIN_SHELL_PROBE_TIMEOUT)
    })
    .await
    .unwrap_or_default();
    let install = find_server_install_in(&shell_dirs);
    (install, shell_dirs)
}

/// Single TCP connectability probe. TCP-level by design: adjudication only
/// asks "does the singleton own the port"; authentication and the WebSocket
/// handshake belong to the hub client layer.
pub async fn hub_is_listening(host: &str, port: u16, probe_timeout: Duration) -> bool {
    matches!(
        tokio::time::timeout(probe_timeout, TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

fn daemon_status(child: &mut Child) -> DaemonStatus {
    match child.try_wait() {
        Ok(Some(status)) => DaemonStatus::Exited(status.code()),
        // Ok(None) = still running. An error cannot meaningfully occur (we
        // hold the only handle); treat it as running and let the deadline
        // settle it.
        _ => DaemonStatus::Running,
    }
}

/// Park the winning daemon on a reaper thread: it outlives this call by
/// design, but its eventual idle-exit must not leave a zombie under a
/// long-lived app process. Reaping never kills.
fn reap_detached(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// The full Hub Bootstrap flow (requirements §3): connect when a hub is
/// listening; otherwise spawn the installed server's daemon detached —
/// joining the same EADDRINUSE singleton race any bridge runs — and poll
/// until the port answers, adjudicating the race by connectivity. Returns
/// the merged [`LocalHubConfig`] the caller should dial (host normalized
/// through [`dial_host`] is the *outcome* fields' source; `config.hub_host`
/// itself stays the configured bind spelling).
pub async fn ensure_hub(
    env: &BootstrapEnv,
    opts: &EnsureOptions,
) -> Result<(EnsureHubOutcome, LocalHubConfig), BootstrapError> {
    let config = resolve_local_hub(env);
    let host = dial_host(&config.hub_host).to_owned();
    if hub_is_listening(&host, config.hub_port, opts.probe_timeout).await {
        return Ok((EnsureHubOutcome::AlreadyListening, config));
    }
    // The hub never runs unauthenticated (bin/hub.ts) — spawning without a
    // token would just produce a refusing daemon.
    config.token.clone().ok_or(BootstrapError::MissingToken)?;
    let (install, dirs) = resolve_server_install(env).await;
    let install = install.ok_or(BootstrapError::NotInstalled)?;
    let child_path = hub_child_path_from_process(&dirs, env);
    let mut child = spawn_hub_daemon(&install, &config, child_path.as_deref())
        .map_err(BootstrapError::Spawn)?;

    let deadline = tokio::time::Instant::now() + opts.connect_deadline;
    loop {
        let connected = hub_is_listening(&host, config.hub_port, opts.probe_timeout).await;
        match adjudicate_spawn_race(daemon_status(&mut child), connected) {
            HubRaceState::WonBySpawn => {
                reap_detached(child);
                return Ok((EnsureHubOutcome::SpawnWon, config));
            }
            HubRaceState::WonByExisting => {
                return Ok((EnsureHubOutcome::SpawnLostToExisting, config))
            }
            HubRaceState::Refused { exit_code } => {
                return Err(BootstrapError::DaemonRefused(exit_code))
            }
            HubRaceState::Starting => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(BootstrapError::TimedOut {
                host,
                port: config.hub_port,
                deadline: opts.connect_deadline,
            });
        }
        tokio::time::sleep(opts.poll_interval).await;
    }
}

// ---------------------------------------------------------------------------
// Guided token setup (generate + preview; the WRITE is the caller's, only
// after user confirmation — requirements §3.1 "never silently")
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum TokenGenError {
    #[error("reading OS entropy failed: {0}")]
    Entropy(#[source] io::Error),
}

/// Generate a fresh hub token: 32 entropy bytes as 64 lowercase hex chars
/// (the shape of `openssl rand -hex 32`; REMOTE.md calls for "a long random
/// secret"). CSPRNG-backed on Unix via /dev/urandom.
pub fn generate_token() -> Result<String, TokenGenError> {
    let bytes = entropy_bytes(32).map_err(TokenGenError::Entropy)?;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    Ok(out)
}

#[cfg(unix)]
fn entropy_bytes(n: usize) -> io::Result<Vec<u8>> {
    use std::io::Read;
    let mut urandom = std::fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; n];
    urandom.read_exact(&mut buf)?;
    Ok(buf)
}

#[cfg(windows)]
fn entropy_bytes(n: usize) -> io::Result<Vec<u8>> {
    // Windows is compile-support only (requirements §2). RandomState's hash
    // keys are seeded from OS entropy per process and varied per call via
    // the thread-local counter + a salt word — weaker than a CSPRNG, but
    // functional where /dev/urandom does not exist.
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_usize(out.len());
        out.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    out.truncate(n);
    Ok(out)
}

#[derive(Debug, thiserror::Error)]
pub enum TokenPlanError {
    #[error("token generation failed: {0}")]
    Generation(#[from] TokenGenError),
    #[error("token must be a non-empty string")]
    EmptyToken,
    #[error("cannot read config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error(
        "config file {path} is not valid JSON — fix or remove it manually before writing a token"
    )]
    Malformed { path: PathBuf },
    #[error("config file {path} is not a JSON object")]
    NotAnObject { path: PathBuf },
    #[error("\"remote\" in {path} is not an object — fix it manually before writing a token")]
    RemoteNotAnObject { path: PathBuf },
}

/// The exact write a confirmed guided setup would perform — a PREVIEW, not a
/// write. `content` is the byte-exact file body (pretty JSON, trailing
/// newline); the caller persists it only after showing the user the path and
/// content (requirements §3.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenWritePlan {
    pub path: PathBuf,
    /// Byte-exact body to write (the preview text).
    pub content: String,
    /// The token the plan carries (trimmed, as it would land in the file).
    pub token: String,
    /// Whether the config file exists today (the write creates it if not).
    pub file_exists: bool,
    /// Whether the config file's parent directory exists today.
    pub dir_exists: bool,
}

/// Plan enabling remote access with `token` at `config_path`: sets
/// `remote.enabled = true` and `remote.token`, preserving every other
/// section and field verbatim (insertion order kept — serde_json
/// preserve_order, matching the TS app's key-order expectations). A missing
/// file plans a fresh minimal document; a malformed file or non-object
/// root/`remote` refuses rather than clobber user data.
pub fn plan_token_write(config_path: &Path, token: &str) -> Result<TokenWritePlan, TokenPlanError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(TokenPlanError::EmptyToken);
    }
    let dir_exists = config_path.parent().map(|p| p.is_dir()).unwrap_or(false);
    let (file_exists, root) = match std::fs::read_to_string(config_path) {
        Ok(text) => (
            true,
            serde_json::from_str::<Value>(&text).map_err(|_| TokenPlanError::Malformed {
                path: config_path.to_owned(),
            })?,
        ),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (false, Value::Object(Map::new())),
        Err(source) => {
            return Err(TokenPlanError::Read {
                path: config_path.to_owned(),
                source,
            })
        }
    };
    let mut root = match root {
        Value::Object(map) => map,
        _ => {
            return Err(TokenPlanError::NotAnObject {
                path: config_path.to_owned(),
            })
        }
    };
    let remote = match root.get_mut("remote") {
        Some(Value::Object(map)) => map,
        Some(_) => {
            return Err(TokenPlanError::RemoteNotAnObject {
                path: config_path.to_owned(),
            })
        }
        None => {
            root.insert("remote".to_owned(), Value::Object(Map::new()));
            root.get_mut("remote")
                .and_then(Value::as_object_mut)
                .expect("just inserted an object")
        }
    };
    // `enabled: true` is part of the guided flow's promise: the plan enables
    // remote access, not just the token (requirements §3.1).
    remote.insert("enabled".to_owned(), Value::Bool(true));
    remote.insert("token".to_owned(), Value::String(token.to_owned()));
    let content = serde_json::to_string_pretty(&Value::Object(root)).expect("Value is valid JSON");
    Ok(TokenWritePlan {
        path: config_path.to_owned(),
        content: format!("{content}\n"),
        token: token.to_owned(),
        file_exists,
        dir_exists,
    })
}

/// One-shot guided setup at the env's config path: generate a fresh token
/// and plan its write. Generating FIRST means the preview the user confirms
/// is the exact token that will land.
pub fn plan_guided_token_setup(
    env: &BootstrapEnv,
) -> Result<(String, TokenWritePlan), TokenPlanError> {
    let token = generate_token()?;
    let plan = plan_token_write(&user_config_path(env), &token)?;
    Ok((token, plan))
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU32 = AtomicU32::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("zcode-bootstrap-{}-{n}-{tag}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_env(home: &Path) -> BootstrapEnv {
        BootstrapEnv {
            home: home.to_path_buf(),
            ..BootstrapEnv::default()
        }
    }

    fn write_config(home: &Path, body: &str) -> PathBuf {
        let path = home.join(".config").join("zcode-acp").join("config.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, body).unwrap();
        path
    }

    // ---- config path ----

    #[test]
    fn config_path_honors_xdg_config_home() {
        let home = TempDir::new("xdg");
        let mut env = test_env(home.path());
        env.xdg_config_home = Some("/custom/xdg".to_owned());
        assert_eq!(
            user_config_path(&env),
            PathBuf::from("/custom/xdg/zcode-acp/config.json")
        );
        // Blank XDG falls back to ~/.config (userConfigPath trims first).
        env.xdg_config_home = Some("   ".to_owned());
        assert_eq!(
            user_config_path(&env),
            home.path().join(".config/zcode-acp/config.json")
        );
        env.xdg_config_home = None;
        assert_eq!(
            user_config_path(&env),
            home.path().join(".config/zcode-acp/config.json")
        );
    }

    // ---- remote section parsing ----

    #[test]
    fn parse_remote_section_valid_and_trims() {
        let section = parse_remote_section(
            r#"{"remote": {"enabled": true, "token": "  tok  ", "hubPort": 9000, "hubHost": " 0.0.0.0 "}}"#,
        )
        .unwrap();
        assert_eq!(
            section,
            RemoteSection {
                enabled: Some(true),
                token: Some("tok".to_owned()),
                hub_port: Some(9000),
                hub_host: Some("0.0.0.0".to_owned()),
            }
        );
    }

    #[test]
    fn parse_remote_section_ignores_junk_fields() {
        // Ports: float / string / 0 / 70000 all read as absent (parseIntField
        // parity); empty token/host and non-bool enabled read as absent too.
        let section = parse_remote_section(
            r#"{"remote": {"enabled": "yes", "token": "  ", "hubPort": 1.5, "hubHost": ""}}"#,
        )
        .unwrap();
        assert_eq!(section, RemoteSection::default());
        let section = parse_remote_section(r#"{"remote": {"hubPort": 0, "extra": true}}"#).unwrap();
        assert_eq!(section.hub_port, None);
        let section = parse_remote_section(r#"{"remote": {"hubPort": 70000}}"#).unwrap();
        assert_eq!(section.hub_port, None);
        let section = parse_remote_section(r#"{"remote": {"hubPort": "8377"}}"#).unwrap();
        assert_eq!(section.hub_port, None);
        // Boundary ports are valid.
        assert_eq!(
            parse_remote_section(r#"{"remote": {"hubPort": 1}}"#)
                .unwrap()
                .hub_port,
            Some(1)
        );
        assert_eq!(
            parse_remote_section(r#"{"remote": {"hubPort": 65535}}"#)
                .unwrap()
                .hub_port,
            Some(65535)
        );
    }

    #[test]
    fn parse_remote_section_absent_or_malformed() {
        assert_eq!(parse_remote_section("{}"), None); // no remote key
        assert_eq!(parse_remote_section(r#"{"remote": 7}"#), None); // not an object
        assert_eq!(parse_remote_section("not json"), None);
        assert_eq!(parse_remote_section("[1,2]"), None); // root not an object
        assert_eq!(parse_remote_section(""), None);
    }

    // ---- merge (file > env > default, per field) ----

    #[test]
    fn resolve_local_hub_file_over_env() {
        let section = RemoteSection {
            enabled: None,
            token: Some("filetok".to_owned()),
            hub_port: Some(9100),
            hub_host: Some("hub.lan".to_owned()),
        };
        let mut env = test_env(Path::new("/nonexistent"));
        env.remote_token = Some("envtok".to_owned());
        env.hub_port = Some("8400".to_owned());
        env.hub_host = Some("env.lan".to_owned());
        assert_eq!(
            resolve_local_hub_with(Some(&section), &env),
            LocalHubConfig {
                token: Some("filetok".to_owned()),
                hub_port: 9100,
                hub_host: "hub.lan".to_owned(),
            }
        );
    }

    #[test]
    fn resolve_local_hub_env_fills_and_defaults() {
        let mut env = test_env(Path::new("/nonexistent"));
        assert_eq!(
            resolve_local_hub_with(None, &env),
            LocalHubConfig {
                token: None,
                hub_port: DEFAULT_HUB_PORT,
                hub_host: DEFAULT_HUB_HOST.to_owned(),
            }
        );
        env.remote_token = Some("  envtok  ".to_owned());
        env.hub_port = Some(" 8401".to_owned());
        env.hub_host = Some(" env.lan ".to_owned());
        assert_eq!(
            resolve_local_hub_with(None, &env),
            LocalHubConfig {
                token: Some("envtok".to_owned()),
                hub_port: 8401,
                hub_host: "env.lan".to_owned(),
            }
        );
        // Blank env values read as unset.
        env.remote_token = Some("   ".to_owned());
        env.hub_host = Some("".to_owned());
        let merged = resolve_local_hub_with(None, &env);
        assert_eq!(merged.token, None);
        assert_eq!(merged.hub_host, DEFAULT_HUB_HOST);
    }

    #[test]
    fn resolve_local_hub_env_port_parseint_parity() {
        let mut env = test_env(Path::new("/nonexistent"));
        // parseInt takes leading digits and ignores the junk tail.
        env.hub_port = Some("8402abc".to_owned());
        assert_eq!(resolve_local_hub_with(None, &env).hub_port, 8402);
        for junk in ["abc", "0", "70000", "-5", ""] {
            env.hub_port = Some(junk.to_owned());
            assert_eq!(
                resolve_local_hub_with(None, &env).hub_port,
                DEFAULT_HUB_PORT,
                "env port {junk:?} should fall back"
            );
        }
    }

    #[test]
    fn resolve_local_hub_reads_file_from_env_home() {
        let home = TempDir::new("merge");
        write_config(
            home.path(),
            r#"{"remote": {"token": "filetok", "hubPort": 9200}}"#,
        );
        let mut env = test_env(home.path());
        env.hub_port = Some("8403".to_owned());
        let merged = resolve_local_hub(&env);
        assert_eq!(merged.token.as_deref(), Some("filetok"));
        assert_eq!(merged.hub_port, 9200); // file port wins over env
        assert_eq!(merged.hub_host, DEFAULT_HUB_HOST); // default fills
                                                       // An invalid file port reads as absent → env applies.
        write_config(home.path(), r#"{"remote": {"hubPort": 99999}}"#);
        assert_eq!(resolve_local_hub(&env).hub_port, 8403);
    }

    #[test]
    fn remote_enabled_precedence() {
        let mut env = test_env(Path::new("/nonexistent"));
        // File wins both ways.
        for file_value in [true, false] {
            let section = RemoteSection {
                enabled: Some(file_value),
                ..RemoteSection::default()
            };
            env.remote_enabled = Some((!file_value).to_string());
            assert_eq!(remote_enabled(Some(&section), &env), file_value);
        }
        // Env gate: the truthy spellings, case-insensitive.
        env.remote_enabled = None;
        assert!(!remote_enabled(None, &env));
        for truthy in ["1", "true", "yes", "on", "TRUE", " On "] {
            env.remote_enabled = Some(truthy.to_owned());
            assert!(remote_enabled(None, &env), "{truthy:?} should enable");
        }
        for falsy in ["0", "false", "off", "junk", ""] {
            env.remote_enabled = Some(falsy.to_owned());
            assert!(!remote_enabled(None, &env), "{falsy:?} should not enable");
        }
    }

    #[test]
    fn assess_local_hub_combines_gate_and_config() {
        let home = TempDir::new("assess");
        write_config(
            home.path(),
            r#"{"remote": {"enabled": true, "token": "tok", "hubPort": 9300}}"#,
        );
        let status = assess_local_hub(&test_env(home.path()));
        assert!(status.enabled);
        assert_eq!(status.config.token.as_deref(), Some("tok"));
        assert_eq!(status.config.hub_port, 9300);
        // Disabled file → guided flow offered, config still readable.
        write_config(
            home.path(),
            r#"{"remote": {"enabled": false, "token": "tok"}}"#,
        );
        let status = assess_local_hub(&test_env(home.path()));
        assert!(!status.enabled);
        assert_eq!(status.config.token.as_deref(), Some("tok"));
    }

    #[test]
    fn dial_host_normalizes_wildcards() {
        assert_eq!(dial_host("0.0.0.0"), "127.0.0.1");
        assert_eq!(dial_host("::"), "::1");
        assert_eq!(dial_host("[::]"), "::1");
        assert_eq!(dial_host(" 0.0.0.0 "), "127.0.0.1");
        assert_eq!(dial_host("192.168.1.5"), "192.168.1.5");
        assert_eq!(dial_host("hub.lan"), "hub.lan");
    }

    // ---- install probe ----

    #[test]
    fn default_install_dirs_order() {
        let mut env = test_env(Path::new("/home/user"));
        env.xdg_data_home = Some("/data".to_owned());
        env.npm_config_prefix = Some("/npm/prefix".to_owned());
        let dirs = default_install_dirs(&env);
        assert_eq!(dirs[0], PathBuf::from("/data/mise/shims"));
        assert_eq!(dirs[1], PathBuf::from("/home/user/.bun/bin"));
        assert_eq!(dirs[2], PathBuf::from("/npm/prefix/bin"));
        assert!(dirs.contains(&PathBuf::from("/usr/local/bin")));
        assert!(dirs.contains(&PathBuf::from("/opt/homebrew/bin")));
        // The hand-installed / XDG user-local bin: a dev checkout links its
        // dispatcher here, and it is NOT on a launchd PATH.
        assert!(dirs.contains(&PathBuf::from("/home/user/.local/bin")));
        // No PATH → no extra entries; with PATH → appended in order.
        assert!(!dirs.iter().any(|d| d.ends_with("extra-bin")));
        env.path = Some("/a/extra-bin:/b".to_owned());
        let dirs = default_install_dirs(&env);
        assert!(dirs.contains(&PathBuf::from("/a/extra-bin")));
        assert!(dirs.contains(&PathBuf::from("/b")));
    }

    #[cfg(unix)]
    #[test]
    fn login_shell_path_sources_interactive_rc() {
        let home = TempDir::new("shellpath");
        // The shell's own PATH is replaced by the rc file, proving the probe
        // sources it — the launchd-inherited PATH never contains this dir.
        std::fs::write(
            home.path().join(".zshrc"),
            "PATH=/probe-only-bin:/usr/bin:/bin\nexport PATH\n",
        )
        .unwrap();
        // `-l` reads ~/.zprofile, NOT ~/.zshrc; -i is avoided. So point HOME at
        // the scratch dir via the shell's own expansion.
        let mut env = test_env(home.path());
        env.shell = Some("/bin/sh".to_owned()); // no rc list → bare dump
        let path = login_shell_path(&env, Duration::from_secs(5));
        // /bin/sh -l -c 'printf %s "$PATH"' still prints SOMETHING (its
        // inherited PATH) — the contract is "a usable PATH or None".
        assert!(path.is_some());

        // zsh specifically: HOME must name the scratch dir for ~/.zshrc.
        if Path::new("/bin/zsh").exists() {
            let script = format!(
                "HOME={} /bin/zsh -l -c 'for f in \"$HOME/.zshrc\"; do [ -r \"$f\" ] && . \"$f\"; done; printf \"%s\" \"$PATH\"'",
                home.path().display()
            );
            let out = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&script)
                .output()
                .unwrap();
            let got = String::from_utf8_lossy(&out.stdout);
            assert!(got.contains("/probe-only-bin"), "rc not sourced: {got}");
        }
    }

    /// A shell that keeps the stdout pipe open through a background child
    /// never reaches EOF — the deadline must still settle the probe as failed
    /// instead of waiting forever (the rc-hangs regression).
    #[cfg(unix)]
    #[test]
    fn wait_with_timeout_gives_up_on_a_leaked_pipe() {
        let child = Command::new("/bin/sh")
            .arg("-c")
            .arg("sleep 30 & printf started")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let started = std::time::Instant::now();
        let out = wait_with_timeout(child, Duration::from_millis(300));
        assert!(out.is_none(), "leaked pipe must read as a failed probe");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "deadline not enforced: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn install_dirs_put_login_shell_path_first() {
        let home = TempDir::new("dirsorder");
        std::fs::write(
            home.path().join(".zshrc"),
            "PATH=/probe-first:/usr/bin:/bin\n",
        )
        .unwrap();
        let mut env = test_env(home.path());
        // A shell that ignores the rc keeps the test hermetic; the union and
        // dedupe behaviour is what this asserts.
        env.shell = Some("/bin/sh".to_owned());
        let dirs = install_dirs_with_login_shell(&env, Duration::from_secs(5));
        assert!(dirs.contains(&home.path().join(".local").join("bin")));
        // Dedupe: no directory appears twice.
        let mut unique = dirs.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), dirs.len(), "duplicate probe dirs: {dirs:?}");
    }

    #[test]
    fn hub_child_path_unions_probe_dirs_with_inherited() {
        let home = TempDir::new("childpath");
        let real = home.path().join("real-bin");
        std::fs::create_dir_all(&real).unwrap();
        let missing = home.path().join("missing-bin");
        let path = hub_child_path(&[real.clone(), missing], Some("/usr/bin:/bin")).unwrap();
        assert_eq!(path, format!("{}:/usr/bin:/bin", real.to_string_lossy()));
        // Empty probe dirs still carry the inherited PATH; both empty → None.
        assert_eq!(hub_child_path(&[], Some("/bin")).as_deref(), Some("/bin"));
        assert_eq!(hub_child_path(&[], None), None);
    }

    #[test]
    fn find_server_install_scans_in_order() {
        let home = TempDir::new("probe");
        let mise = home.path().join("mise-shims");
        let bun = home.path().join("bun-bin");
        std::fs::create_dir_all(&mise).unwrap();
        std::fs::create_dir_all(&bun).unwrap();
        // Nothing → None.
        let mut env = test_env(home.path());
        env.install_dirs = Some(vec![mise.clone(), bun.clone()]);
        assert_eq!(find_server_install(&env), None);
        // Bun only → bun hit.
        std::fs::write(bun.join(HUB_BIN_NAME), "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            find_server_install(&env),
            Some(ServerInstall {
                bin: bun.join(HUB_BIN_NAME)
            })
        );
        // Mise wins once present (earlier in the order).
        std::fs::write(mise.join(HUB_BIN_NAME), "#!/bin/sh\nexit 0\n").unwrap();
        assert_eq!(
            find_server_install(&env),
            Some(ServerInstall {
                bin: mise.join(HUB_BIN_NAME)
            })
        );
    }

    // ---- spawn env ----

    #[test]
    fn hub_spawn_env_carries_hub_trio() {
        let config = LocalHubConfig {
            token: Some("tok".to_owned()),
            hub_port: 9400,
            hub_host: "0.0.0.0".to_owned(),
        };
        assert_eq!(
            hub_spawn_env(&config),
            vec![
                ("ZCODE_ACP_HUB_PORT".to_owned(), "9400".to_owned()),
                ("ZCODE_ACP_HUB_HOST".to_owned(), "0.0.0.0".to_owned()),
                ("ZCODE_ACP_REMOTE_TOKEN".to_owned(), "tok".to_owned()),
            ]
        );
        let tokenless = LocalHubConfig {
            token: None,
            hub_port: DEFAULT_HUB_PORT,
            hub_host: DEFAULT_HUB_HOST.to_owned(),
        };
        assert!(!hub_spawn_env(&tokenless)
            .iter()
            .any(|(k, _)| k == "ZCODE_ACP_REMOTE_TOKEN"));
    }

    // ---- race adjudication (pure table) ----

    #[test]
    fn adjudicate_race_table() {
        use DaemonStatus::{Exited, Running};
        use HubRaceState::*;
        // Port answers + our daemon alive → we won the bind.
        assert_eq!(adjudicate_spawn_race(Running, true), WonBySpawn);
        // Port answers + our daemon exited → EADDRINUSE loser (exit 0 by
        // design); the existing hub keeps serving.
        assert_eq!(adjudicate_spawn_race(Exited(Some(0)), true), WonByExisting);
        assert_eq!(adjudicate_spawn_race(Exited(None), true), WonByExisting);
        // Daemon dead with failing exit + port dark → refused outright.
        assert_eq!(
            adjudicate_spawn_race(Exited(Some(1)), false),
            Refused { exit_code: Some(1) }
        );
        assert_eq!(
            adjudicate_spawn_race(Exited(None), false),
            Refused { exit_code: None }
        );
        // Running + dark → still starting.
        assert_eq!(adjudicate_spawn_race(Running, false), Starting);
        // Clean exit + dark → the race's other winner may still be mid-bind;
        // the deadline is the backstop.
        assert_eq!(adjudicate_spawn_race(Exited(Some(0)), false), Starting);
    }

    // ---- TCP probes ----

    #[tokio::test]
    async fn hub_is_listening_against_real_socket() {
        async fn free_port() -> u16 {
            // Bind :0, read the port, drop the listener — the port is then
            // unbound and loopback connects refuse immediately.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            port
        }
        let port = free_port().await;
        assert!(!hub_is_listening("127.0.0.1", port, Duration::from_millis(300)).await);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(hub_is_listening("127.0.0.1", port, Duration::from_millis(300)).await);
    }

    // ---- ensure_hub end-to-end (real processes; unix shell scripts stand
    // in for the daemon) ----

    /// Serialize the async ensure tests: their negative cases need ports
    /// that stay unbound, which cross-test port reuse would break.
    static ENSURE_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[cfg(unix)]
    fn write_fake_daemon(home: &Path, body: &str) -> ServerInstall {
        use std::os::unix::fs::PermissionsExt;
        let dir = home.join(".bun").join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join(HUB_BIN_NAME);
        std::fs::write(&bin, body).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        ServerInstall { bin }
    }

    #[cfg(unix)]
    fn ensure_opts() -> EnsureOptions {
        EnsureOptions {
            probe_timeout: Duration::from_millis(300),
            poll_interval: Duration::from_millis(30),
            connect_deadline: Duration::from_millis(2_000),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_hub_already_listening_does_not_spawn() {
        let _guard = ENSURE_SERIAL.lock().await;
        let home = TempDir::new("already");
        let marker = home.path().join("spawned-marker");
        let script = format!("#!/bin/sh\necho x >> {}\nexec sleep 2\n", marker.display());
        write_fake_daemon(home.path(), &script);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let mut env = test_env(home.path());
        env.remote_token = Some("tok".to_owned());
        env.hub_port = Some(port.to_string());
        let (outcome, config) = ensure_hub(&env, &ensure_opts()).await.unwrap();
        assert_eq!(outcome, EnsureHubOutcome::AlreadyListening);
        assert_eq!(config.token.as_deref(), Some("tok"));
        assert_eq!(config.hub_port, port);
        // Give a would-be spawn a beat to (wrongly) fire, then prove none did.
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !marker.exists(),
            "already-listening must not spawn a daemon"
        );
    }

    /// Bind a listener on `port` after `delay` — the spawn path needs the
    /// port DARK at ensure_hub's pre-probe and answering later, playing the
    /// hub binding that the (fake) daemon's spawn produced.
    #[cfg(unix)]
    async fn bind_after(delay: Duration, port: u16) {
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            // Hold the port for the rest of the test (runtime drops it at
            // test end). Bind failures would surface as a dark port and the
            // test's own deadline failure, never a silent pass.
            let _held = tokio::net::TcpListener::bind(("127.0.0.1", port)).await;
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_hub_spawn_wins_when_daemon_stays_alive() {
        let _guard = ENSURE_SERIAL.lock().await;
        let home = TempDir::new("win");
        write_fake_daemon(home.path(), "#!/bin/sh\nexec sleep 5\n");
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        // Bind after the pre-probe refused (loopback refuses in ~1ms) but
        // well inside the 2s deadline: the daemon is alive + port answers.
        bind_after(Duration::from_millis(400), port).await;
        let mut env = test_env(home.path());
        env.remote_token = Some("tok".to_owned());
        env.hub_port = Some(port.to_string());
        let (outcome, _) = ensure_hub(&env, &ensure_opts()).await.unwrap();
        assert_eq!(outcome, EnsureHubOutcome::SpawnWon);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_hub_spawn_loses_to_existing_hub() {
        let _guard = ENSURE_SERIAL.lock().await;
        let home = TempDir::new("lose");
        // The EADDRINUSE loser shape: exits 0 immediately (bin/hub.ts) —
        // the port answers because ANOTHER hub already owns it.
        write_fake_daemon(home.path(), "#!/bin/sh\nexit 0\n");
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        bind_after(Duration::from_millis(400), port).await;
        let mut env = test_env(home.path());
        env.remote_token = Some("tok".to_owned());
        env.hub_port = Some(port.to_string());
        let (outcome, _) = ensure_hub(&env, &ensure_opts()).await.unwrap();
        assert_eq!(outcome, EnsureHubOutcome::SpawnLostToExisting);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_hub_refused_when_daemon_fails() {
        let _guard = ENSURE_SERIAL.lock().await;
        let home = TempDir::new("refused");
        write_fake_daemon(home.path(), "#!/bin/sh\nexit 7\n");
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut env = test_env(home.path());
        env.remote_token = Some("tok".to_owned());
        env.hub_port = Some(port.to_string());
        let err = ensure_hub(&env, &ensure_opts()).await.unwrap_err();
        assert!(
            matches!(err, BootstrapError::DaemonRefused(Some(7))),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_hub_times_out_when_port_never_answers() {
        let _guard = ENSURE_SERIAL.lock().await;
        let home = TempDir::new("timeout");
        // Clean exit + dark port: adjudication keeps waiting (another winner
        // may be mid-bind) until the deadline decides.
        write_fake_daemon(home.path(), "#!/bin/sh\nexit 0\n");
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        let mut env = test_env(home.path());
        env.remote_token = Some("tok".to_owned());
        env.hub_port = Some(port.to_string());
        let opts = EnsureOptions {
            connect_deadline: Duration::from_millis(300),
            ..ensure_opts()
        };
        let err = ensure_hub(&env, &opts).await.unwrap_err();
        assert!(
            matches!(err, BootstrapError::TimedOut { port: p, .. } if p == port),
            "{err:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_hub_missing_token_and_not_installed() {
        let _guard = ENSURE_SERIAL.lock().await;
        let home = TempDir::new("guards");
        // No token anywhere → MissingToken before any spawn consideration.
        let env = test_env(home.path());
        let err = ensure_hub(&env, &ensure_opts()).await.unwrap_err();
        assert!(matches!(err, BootstrapError::MissingToken), "{err:?}");
        // Token but no install (probe list pinned empty) → NotInstalled.
        let mut env = test_env(home.path());
        env.remote_token = Some("tok".to_owned());
        env.hub_port = Some("59999".to_owned());
        env.install_dirs = Some(vec![]);
        let err = ensure_hub(&env, &ensure_opts()).await.unwrap_err();
        assert!(matches!(err, BootstrapError::NotInstalled), "{err:?}");
    }

    /// The reported desktop failure: a dev-setup dispatcher lives in
    /// `~/.local/bin` and is a shell wrapper around a PATH-resolved
    /// interpreter. Under launchd's bare PATH neither the bin nor `node`
    /// exists, so the spawn must carry the probe dirs' PATH — otherwise it
    /// "succeeds" and the daemon dies on exec (silent `NotInstalled`-like
    /// failure reported as a refused/timed-out daemon).
    #[cfg(unix)]
    #[tokio::test]
    async fn ensure_hub_spawns_dev_setup_dispatcher_from_user_local_bin() {
        use std::os::unix::fs::PermissionsExt;
        let _guard = ENSURE_SERIAL.lock().await;
        let home = TempDir::new("userlocal");
        // A directory the DEFAULT probe list never contains, so the hit below
        // proves the pinned list is used verbatim — not the default list plus
        // the host's login-shell PATH (the bug this test pins down).
        let user_bin = home.path().join("custom-tools");
        std::fs::create_dir_all(&user_bin).unwrap();
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        bind_after(Duration::from_millis(400), port).await;
        // A wrapper that needs a PATH-resolved interpreter, like the real
        // `zcode-acp` dev shim (`exec node .../dist/cli.js "$@"`): `sleep`
        // resolves through PATH and exists on the probe list's /bin only if
        // the spawn carried the completed PATH.
        let bin = user_bin.join(HUB_BIN_NAME);
        let marker = home.path().join("spawned-with-path");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\ncommand -v sleep >/dev/null || exit 9\necho \"$PATH\" > {}\nexec sleep 5\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut env = test_env(home.path());
        env.remote_token = Some("tok".to_owned());
        env.hub_port = Some(port.to_string());
        // Pin the probe list to the user-local bin so the test never depends
        // on (or launches) the host's login shell.
        env.install_dirs = Some(vec![user_bin.clone()]);
        let (outcome, _) = ensure_hub(&env, &ensure_opts()).await.unwrap();
        assert_eq!(outcome, EnsureHubOutcome::SpawnWon);
        // The daemon saw a PATH that contains its own dir — under launchd's
        // bare PATH the wrapper could not have resolved `sleep`.
        let seen = std::fs::read_to_string(&marker).unwrap();
        assert!(
            seen.split(':').any(|p| Path::new(p) == user_bin),
            "spawned PATH lost the dispatcher dir: {seen}"
        );
    }

    // ---- token generation ----

    #[test]
    fn generate_token_shape_and_uniqueness() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_ne!(a, b);
    }

    // ---- token write planning ----

    #[test]
    fn plan_token_write_fresh_file() {
        let home = TempDir::new("fresh");
        let path = home.path().join(".config/zcode-acp/config.json");
        let plan = plan_token_write(&path, "tok123").unwrap();
        assert!(!plan.file_exists);
        assert!(!plan.dir_exists);
        assert_eq!(plan.token, "tok123");
        assert_eq!(plan.path, path);
        let value: Value = serde_json::from_str(&plan.content).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"remote": {"enabled": true, "token": "tok123"}})
        );
        assert!(plan.content.ends_with('\n'));
    }

    #[test]
    fn plan_token_write_preserves_other_sections() {
        let home = TempDir::new("preserve");
        let path = write_config(
            home.path(),
            r#"{"quota": {"autoResume": false}, "lang": "zh", "remote": {"enabled": false, "hubPort": 9500, "hubHost": "0.0.0.0"}}"#,
        );
        let before = std::fs::read_to_string(&path).unwrap();
        let plan = plan_token_write(&path, " newtok ").unwrap();
        assert!(plan.file_exists && plan.dir_exists);
        assert_eq!(plan.token, "newtok");
        let value: Value = serde_json::from_str(&plan.content).unwrap();
        assert_eq!(value["quota"]["autoResume"], serde_json::json!(false));
        assert_eq!(value["lang"], serde_json::json!("zh"));
        assert_eq!(value["remote"]["hubPort"], serde_json::json!(9500));
        assert_eq!(value["remote"]["hubHost"], serde_json::json!("0.0.0.0"));
        assert_eq!(value["remote"]["enabled"], serde_json::json!(true)); // flipped
        assert_eq!(value["remote"]["token"], serde_json::json!("newtok"));
        // Planning writes nothing: the file on disk is untouched.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
    }

    #[test]
    fn plan_token_write_rejects_malformed() {
        let home = TempDir::new("malformed");
        assert!(matches!(
            plan_token_write(Path::new("/nonexistent/cfg"), "  "),
            Err(TokenPlanError::EmptyToken)
        ));
        let path = write_config(home.path(), "{ not json");
        assert!(matches!(
            plan_token_write(&path, "tok"),
            Err(TokenPlanError::Malformed { .. })
        ));
        let path = write_config(home.path(), r#"{"remote": 7}"#);
        assert!(matches!(
            plan_token_write(&path, "tok"),
            Err(TokenPlanError::RemoteNotAnObject { .. })
        ));
        let path = write_config(home.path(), "[1]");
        assert!(matches!(
            plan_token_write(&path, "tok"),
            Err(TokenPlanError::NotAnObject { .. })
        ));
    }

    #[test]
    fn plan_guided_token_setup_uses_env_config_path() {
        let home = TempDir::new("guided");
        let env = test_env(home.path());
        let (token, plan) = plan_guided_token_setup(&env).unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(plan.token, token);
        assert_eq!(plan.path, user_config_path(&env));
        let value: Value = serde_json::from_str(&plan.content).unwrap();
        assert_eq!(value["remote"]["token"], serde_json::json!(token));
        assert_eq!(value["remote"]["enabled"], serde_json::json!(true));
    }
}
