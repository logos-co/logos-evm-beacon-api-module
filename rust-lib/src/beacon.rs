//! Beacon API client core: per-chain provider lists, persisted, with failover.
//!
//! Serves the light-client routes the verified proxy reads, keyed by chainId. Pure Rust (no
//! Logos deps); every request goes through the fail-closed [`crate::proxy`] chokepoint.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::proxy::{build_client, validate_proxy_url, ProxyConfig};

/// 128 updates measured 7.3 MB; anything far past that is not a light-client answer.
pub const MAX_BODY: u64 = 32 * 1024 * 1024;
/// A head update older than this fails over: one public node ran two days behind.
pub const MAX_HEAD_LAG_SLOTS: u64 = 64;
const SLOT_SECS: u64 = 12;
/// `updates` grows with `count`: 128 measured 1.6 s on one provider and 283 s on another.
const SECS_PER_UPDATE: u64 = 2;
const MIN_ATTEMPT: Duration = Duration::from_millis(50);
const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);
const MAX_DEADLINE: Duration = Duration::from_secs(600);
/// A provider failing this many times in a row is tried last for `COOLDOWN`.
const COOLDOWN_AFTER: u64 = 3;
const COOLDOWN: Duration = Duration::from_secs(60);

fn default_true() -> bool {
    true
}

fn default_timeout() -> u64 {
    15
}

pub struct Network {
    pub chain_id: u64,
    pub name: &'static str,
    pub genesis_time: u64,
}

/// Genesis times read from each network's `/eth/v1/beacon/genesis`.
pub const NETWORKS: &[Network] = &[
    Network { chain_id: 1, name: "mainnet", genesis_time: 1_606_824_023 },
    Network { chain_id: 11_155_111, name: "sepolia", genesis_time: 1_655_733_600 },
    Network { chain_id: 560_048, name: "hoodi", genesis_time: 1_742_213_400 },
];

pub fn network(chain_id: u64) -> Option<&'static Network> {
    NETWORKS.iter().find(|n| n.chain_id == chain_id)
}

pub struct BuiltinProvider {
    pub chain_id: u64,
    pub id: &'static str,
    pub endpoint: &'static str,
}

/// Probed 2026-11-03: each serves all four light-client routes with the `Eth-Consensus-Version`
/// header. Plain-http ones are fallbacks; [`BeaconApi::finalized_header`] never uses them.
pub const DEFAULT_PROVIDERS: &[BuiltinProvider] = &[
    BuiltinProvider { chain_id: 1, id: "lodestar", endpoint: "https://lodestar-mainnet.chainsafe.io" },
    BuiltinProvider { chain_id: 1, id: "nimbus-testing", endpoint: "http://testing.mainnet.beacon-api.nimbus.team" },
    BuiltinProvider { chain_id: 1, id: "nimbus-unstable", endpoint: "http://unstable.mainnet.beacon-api.nimbus.team" },
    BuiltinProvider { chain_id: 11_155_111, id: "lodestar", endpoint: "https://lodestar-sepolia.chainsafe.io" },
    BuiltinProvider { chain_id: 11_155_111, id: "nimbus-unstable", endpoint: "http://unstable.sepolia.beacon-api.nimbus.team" },
    BuiltinProvider { chain_id: 560_048, id: "lodestar", endpoint: "https://lodestar-hoodi.chainsafe.io" },
    BuiltinProvider { chain_id: 560_048, id: "nimbus-testing", endpoint: "http://testing.hoodi.beacon-api.nimbus.team" },
];

/// Once-default endpoints dropped from stores that seeded them. publicnode sends no
/// `Eth-Consensus-Version`, which nimbus' light client needs to decode an update.
pub const RETIRED_DEFAULTS: &[(u64, &str)] = &[
    (1, "https://ethereum-beacon-api.publicnode.com"),
    (560_048, "https://ethereum-hoodi-beacon-api.publicnode.com"),
];

/// The response header nimbus' light-client REST client reads the fork from.
pub const CONSENSUS_VERSION: &str = "eth-consensus-version";

/// Who wrote a provider entry. Reporting only; a caller cannot claim `default`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConfigSource {
    #[default]
    External,
    #[serde(rename = "default")]
    Builtin,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub id: String,
    pub endpoint: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub source: ConfigSource,
}

impl Provider {
    pub fn https(&self) -> bool {
        self.endpoint.starts_with("https://")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainConfig {
    #[serde(default)]
    pub providers: Vec<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy: Option<String>,
    #[serde(default)]
    pub proxy_required: bool,
    #[serde(default = "default_timeout")]
    pub timeout_secs: u64,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self { providers: Vec::new(), proxy: None, proxy_required: false, timeout_secs: default_timeout() }
    }
}

/// A provider id is shaped like a DNS label, so it is safe in a URL or a log line.
pub fn valid_id(id: &str) -> bool {
    (1..=40).contains(&id.len())
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !id.starts_with('-')
        && !id.ends_with('-')
}

/// `http(s)://host[/path]`, trailing slash dropped. A path is kept: some providers mount the
/// API under one.
pub fn normalize_endpoint(endpoint: &str) -> Result<String, String> {
    let e = endpoint.trim().trim_end_matches('/');
    let u = url::Url::parse(e).map_err(|err| format!("endpoint '{e}': {err}"))?;
    if !matches!(u.scheme(), "http" | "https") {
        return Err(format!("endpoint '{e}' must be http or https"));
    }
    if u.host_str().is_none_or(str::is_empty) {
        return Err(format!("endpoint '{e}' has no host"));
    }
    if u.query().is_some() || u.fragment().is_some() {
        return Err(format!("endpoint '{e}' must not carry a query or fragment"));
    }
    Ok(e.to_string())
}

/// An id from the endpoint's host, e.g. `lodestar-mainnet-chainsafe-io`, made unique.
pub fn derive_id(endpoint: &str, taken: &[Provider]) -> String {
    let host = url::Url::parse(endpoint).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let mut base: String = host
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    base.truncate(36);
    let base = base.trim_matches('-').to_string();
    let base = if base.is_empty() { "provider".to_string() } else { base };
    let mut id = base.clone();
    let mut n = 2;
    while taken.iter().any(|p| p.id == id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

impl ChainConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(1..=120).contains(&self.timeout_secs) {
            return Err(format!("timeoutSecs must be 1..=120, got {}", self.timeout_secs));
        }
        if let Some(p) = self.proxy.as_deref().filter(|p| !p.trim().is_empty()) {
            validate_proxy_url(p.trim()).map_err(|e| e.to_string())?;
        }
        let mut ids = BTreeSet::new();
        let mut endpoints = BTreeSet::new();
        for p in &self.providers {
            if !valid_id(&p.id) {
                return Err(format!("provider id '{}' must be 1-40 of a-z, 0-9 and '-'", p.id));
            }
            if normalize_endpoint(&p.endpoint)? != p.endpoint {
                return Err(format!("provider '{}': endpoint must be normalized", p.id));
            }
            if !ids.insert(p.id.as_str()) {
                return Err(format!("duplicate provider id '{}'", p.id));
            }
            if !endpoints.insert(p.endpoint.as_str()) {
                return Err(format!("duplicate provider endpoint '{}'", p.endpoint));
            }
        }
        Ok(())
    }

    fn proxy_config(&self) -> ProxyConfig {
        ProxyConfig::new(self.proxy.clone(), self.proxy_required, 0)
    }
}

/// The four light-client routes, named as libverifproxy's beacon transport names them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LcRequest {
    Bootstrap { block_root: String },
    Updates { start_period: u64, count: u64 },
    Optimistic,
    Finality,
}

impl LcRequest {
    /// From the transport's `(endpoint, params)` pair. The library sends `block_root` without
    /// `0x`, which nimbus answers with 400, so it is added here.
    pub fn parse(endpoint: &str, params: &Value) -> Result<Self, String> {
        let uint = |k: &str| -> Result<u64, String> {
            match params.get(k) {
                Some(Value::Number(n)) => n.as_u64().ok_or_else(|| format!("{k} must be a non-negative integer")),
                Some(Value::String(s)) => s.parse().map_err(|_| format!("{k} must be a non-negative integer")),
                _ => Err(format!("{endpoint} needs {k}")),
            }
        };
        match endpoint {
            "getLightClientBootstrap" => {
                let raw = params.get("block_root").and_then(Value::as_str).unwrap_or("").trim();
                let hex = raw.strip_prefix("0x").unwrap_or(raw);
                if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(format!("block_root must be 32 bytes of hex, got '{raw}'"));
                }
                Ok(Self::Bootstrap { block_root: format!("0x{}", hex.to_ascii_lowercase()) })
            }
            "getLightClientUpdatesByRange" => {
                let count = uint("count")?;
                if !(1..=128).contains(&count) {
                    return Err(format!("count must be 1..=128, got {count}"));
                }
                Ok(Self::Updates { start_period: uint("start_period")?, count })
            }
            "getLightClientOptimisticUpdate" => Ok(Self::Optimistic),
            "getLightClientFinalityUpdate" => Ok(Self::Finality),
            other => Err(format!("unknown light-client endpoint '{other}'")),
        }
    }

    pub fn path(&self) -> String {
        const BASE: &str = "/eth/v1/beacon/light_client";
        match self {
            Self::Bootstrap { block_root } => format!("{BASE}/bootstrap/{block_root}"),
            Self::Updates { start_period, count } => {
                format!("{BASE}/updates?start_period={start_period}&count={count}")
            }
            Self::Optimistic => format!("{BASE}/optimistic_update"),
            Self::Finality => format!("{BASE}/finality_update"),
        }
    }

    fn attempt_timeout(&self, timeout_secs: u64) -> Duration {
        match self {
            Self::Updates { count, .. } => Duration::from_secs(timeout_secs + SECS_PER_UPDATE * count),
            _ => Duration::from_secs(timeout_secs),
        }
    }

    fn is_head(&self) -> bool {
        matches!(self, Self::Optimistic | Self::Finality)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Attempt {
    pub provider: String,
    pub error: String,
}

#[derive(Debug, thiserror::Error)]
pub enum BeaconError {
    #[error("no beacon configuration for chain {0}")]
    UnknownChain(u64),
    #[error("chain {0} has no enabled beacon provider")]
    NoProviders(u64),
    #[error("chain {chain_id} has no provider '{id}'")]
    UnknownProvider { chain_id: u64, id: String },
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Proxy(String),
    #[error("every provider failed: {}", summarize(.attempts))]
    AllFailed { attempts: Vec<Attempt>, deadline: bool },
}

fn summarize(attempts: &[Attempt]) -> String {
    attempts.iter().map(|a| format!("{}: {}", a.provider, a.error)).collect::<Vec<_>>().join("; ")
}

impl BeaconError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownChain(_) => "unknown_chain",
            Self::NoProviders(_) => "no_providers",
            Self::UnknownProvider { .. } => "unknown_provider",
            Self::BadRequest(_) => "bad_request",
            Self::Proxy(_) => "proxy",
            Self::AllFailed { deadline: true, .. } => "timeout",
            Self::AllFailed { .. } => "unavailable",
        }
    }

    pub fn to_json(&self) -> Value {
        let mut v = json!({ "ok": false, "code": self.code(), "error": self.to_string() });
        if let Self::AllFailed { attempts, .. } = self {
            v["attempts"] = json!(attempts);
        }
        v
    }
}

/// Per-provider outcomes since the module loaded. Never persisted.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Health {
    pub served: u64,
    pub failures: u64,
    pub consecutive_failures: u64,
    pub last_ok_at: u64,
    pub last_error: String,
    pub last_error_at: u64,
    pub last_latency_ms: u64,
    #[serde(skip)]
    cooling_until: Option<Instant>,
}

#[derive(Default)]
pub struct HealthBook(Mutex<HashMap<(u64, String), Health>>);

impl HealthBook {
    pub fn get(&self, chain_id: u64, id: &str) -> Health {
        self.0.lock().map(|m| m.get(&(chain_id, id.to_string())).cloned().unwrap_or_default()).unwrap_or_default()
    }

    fn cooling(&self, chain_id: u64, id: &str) -> bool {
        self.get(chain_id, id).cooling_until.is_some_and(|t| Instant::now() < t)
    }

    fn record(&self, chain_id: u64, id: &str, outcome: Result<Duration, &str>) {
        let Ok(mut m) = self.0.lock() else { return };
        let h = m.entry((chain_id, id.to_string())).or_default();
        match outcome {
            Ok(latency) => {
                h.served += 1;
                h.consecutive_failures = 0;
                h.cooling_until = None;
                h.last_ok_at = unix_now();
                h.last_latency_ms = latency.as_millis() as u64;
            }
            Err(e) => {
                h.failures += 1;
                h.consecutive_failures += 1;
                h.last_error = e.to_string();
                h.last_error_at = unix_now();
                if h.consecutive_failures >= COOLDOWN_AFTER {
                    h.cooling_until = Some(Instant::now() + COOLDOWN);
                }
            }
        }
    }

    pub fn forget(&self, chain_id: u64, id: &str) {
        if let Ok(mut m) = self.0.lock() {
            m.remove(&(chain_id, id.to_string()));
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The slot an update was signed in, from either light-client update shape.
fn update_slot(v: &Value) -> Option<u64> {
    let data = v.get("data")?;
    let slot = data
        .get("signature_slot")
        .or_else(|| data.pointer("/attested_header/beacon/slot"))?;
    match slot {
        Value::String(s) => s.parse().ok(),
        Value::Number(n) => n.as_u64(),
        _ => None,
    }
}

/// How many slots behind the wall clock `slot` is, or `None` for a network we have no genesis for.
pub fn lag_slots(chain_id: u64, slot: u64, now: u64) -> Option<u64> {
    let genesis = network(chain_id)?.genesis_time;
    let wall = now.saturating_sub(genesis) / SLOT_SECS;
    Some(wall.saturating_sub(slot))
}

/// A served answer: which provider gave it, and the body verbatim.
#[derive(Clone, Debug)]
pub struct Fetched {
    pub provider: String,
    pub body: String,
    pub attempts: Vec<Attempt>,
}

/// A config snapshot to fetch with, so no lock is held across network I/O.
#[derive(Clone, Debug)]
pub struct Plan {
    pub chain_id: u64,
    pub cfg: ChainConfig,
}

impl Plan {
    /// Pinned first, then the rest in configured order; cooling providers go last.
    fn order(&self, health: &HealthBook, pinned: Option<&str>) -> Vec<&Provider> {
        let enabled: Vec<&Provider> = self.cfg.providers.iter().filter(|p| p.enabled).collect();
        let (mut first, mut rest): (Vec<&Provider>, Vec<&Provider>) =
            enabled.into_iter().partition(|p| Some(p.id.as_str()) == pinned);
        rest.sort_by_key(|p| health.cooling(self.chain_id, &p.id));
        first.append(&mut rest);
        first
    }

    fn client(&self) -> Result<reqwest::blocking::Client, BeaconError> {
        build_client(&self.cfg.proxy_config()).map_err(|e| BeaconError::Proxy(e.to_string()))
    }

    /// A light-client request with failover. `pinned` is tried first when it names an enabled
    /// provider; any other answer than a usable 200 moves on to the next one.
    pub fn fetch(
        &self,
        health: &HealthBook,
        req: &LcRequest,
        pinned: Option<&str>,
        deadline: Duration,
    ) -> Result<Fetched, BeaconError> {
        let order = self.order(health, pinned);
        if order.is_empty() {
            return Err(BeaconError::NoProviders(self.chain_id));
        }
        let client = self.client()?;
        let t0 = Instant::now();
        let mut attempts = Vec::new();
        for p in order {
            let left = deadline.saturating_sub(t0.elapsed());
            if left < MIN_ATTEMPT {
                return Err(BeaconError::AllFailed { attempts, deadline: true });
            }
            let started = Instant::now();
            let outcome = get(&client, &p.endpoint, &req.path(), req.attempt_timeout(self.cfg.timeout_secs).min(left))
                .and_then(|body| self.check(req, body));
            match outcome {
                Ok(body) => {
                    health.record(self.chain_id, &p.id, Ok(started.elapsed()));
                    return Ok(Fetched { provider: p.id.clone(), body, attempts });
                }
                Err(e) => {
                    health.record(self.chain_id, &p.id, Err(e.as_str()));
                    attempts.push(Attempt { provider: p.id.clone(), error: e });
                }
            }
        }
        let deadline_hit = deadline.saturating_sub(t0.elapsed()) < MIN_ATTEMPT;
        Err(BeaconError::AllFailed { attempts, deadline: deadline_hit })
    }

    /// Catches what a 200 can still hide: an HTML page from a captive portal on a plain-http
    /// provider, or a head update from a node that stopped following the chain.
    fn check(&self, req: &LcRequest, body: String) -> Result<String, String> {
        let first = body.trim_start().as_bytes().first().copied();
        if !matches!(first, Some(b'{') | Some(b'[')) {
            return Err("answered 200 with a body that is not JSON".into());
        }
        if req.is_head() {
            let v: Value = serde_json::from_str(&body).map_err(|e| format!("malformed update: {e}"))?;
            let slot = update_slot(&v).ok_or("update carries no slot")?;
            if let Some(lag) = lag_slots(self.chain_id, slot, unix_now()) {
                if lag > MAX_HEAD_LAG_SLOTS {
                    return Err(format!("stale: slot {slot} is {lag} slots behind the clock"));
                }
            }
        }
        Ok(body)
    }

    /// One provider, no failover: whether it serves the light-client API and how far behind it is.
    /// Serving it includes the fork header: nimbus' light client reads the fork from nowhere else.
    pub fn check_provider(&self, health: &HealthBook, id: &str) -> Result<Value, BeaconError> {
        let p = self
            .cfg
            .providers
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| BeaconError::UnknownProvider { chain_id: self.chain_id, id: id.to_string() })?;
        let client = self.client()?;
        let req = LcRequest::Optimistic;
        let t0 = Instant::now();
        let raw = get_with_fork(&client, &p.endpoint, &req.path(), Duration::from_secs(self.cfg.timeout_secs));
        let latency = t0.elapsed().as_millis() as u64;
        let mut out = json!({ "ok": true, "chainId": self.chain_id, "provider": p.id, "endpoint": p.endpoint,
                              "https": p.https(), "latencyMs": latency });
        let slot =
            raw.as_ref().ok().and_then(|(_, b)| serde_json::from_str::<Value>(b).ok()).and_then(|v| update_slot(&v));
        match (&raw, slot) {
            (Ok((Some(fork), _)), Some(slot)) => {
                let lag = lag_slots(self.chain_id, slot, unix_now());
                let stale = lag.is_some_and(|l| l > MAX_HEAD_LAG_SLOTS);
                out["servesLightClient"] = json!(true);
                out["consensusVersion"] = json!(fork);
                out["slot"] = json!(slot);
                out["lagSlots"] = json!(lag);
                out["healthy"] = json!(!stale);
                if stale {
                    out["error"] = json!(format!("{} slots behind the clock", lag.unwrap_or(0)));
                }
            }
            (Ok((None, _)), Some(_)) => {
                out["servesLightClient"] = json!(false);
                out["healthy"] = json!(false);
                out["error"] = json!("no Eth-Consensus-Version header, which the verified proxy's light client needs");
            }
            (Ok(_), None) => {
                out["servesLightClient"] = json!(false);
                out["healthy"] = json!(false);
                out["error"] = json!("answered, but not with a light-client update");
            }
            (Err(e), _) => {
                out["servesLightClient"] = json!(false);
                out["healthy"] = json!(false);
                out["error"] = json!(e);
            }
        }
        let healthy = out["healthy"].as_bool() == Some(true);
        let why = out["error"].as_str().unwrap_or("").to_string();
        health.record(self.chain_id, &p.id, if healthy { Ok(t0.elapsed()) } else { Err(why.as_str()) });
        Ok(out)
    }

    /// The finalized header from the first https provider that answers. Over plain http anyone
    /// on the path could substitute a root, and a root is what verification starts from.
    pub fn finalized_header(&self, health: &HealthBook) -> Result<Value, BeaconError> {
        let order: Vec<&Provider> = self.order(health, None).into_iter().filter(|p| p.https()).collect();
        if order.is_empty() {
            return Err(BeaconError::NoProviders(self.chain_id));
        }
        let client = self.client()?;
        let mut attempts = Vec::new();
        for p in order {
            let parsed = get(&client, &p.endpoint, "/eth/v1/beacon/headers/finalized", Duration::from_secs(self.cfg.timeout_secs))
                .and_then(|b| serde_json::from_str::<Value>(&b).map_err(|e| format!("malformed header: {e}")))
                .and_then(|v| {
                    let root = v.pointer("/data/root").and_then(Value::as_str).map(str::to_string);
                    let slot = v.pointer("/data/header/message/slot").and_then(Value::as_str).map(str::to_string);
                    match (root, slot) {
                        (Some(r), Some(s)) if r.len() == 66 && r.starts_with("0x") => Ok((r, s)),
                        _ => Err("answer carries no data.root".to_string()),
                    }
                });
            match parsed {
                Ok((root, slot)) => {
                    return Ok(json!({ "ok": true, "chainId": self.chain_id, "root": root, "slot": slot,
                                      "provider": p.id, "endpoint": p.endpoint }));
                }
                Err(e) => attempts.push(Attempt { provider: p.id.clone(), error: e }),
            }
        }
        Err(BeaconError::AllFailed { attempts, deadline: false })
    }

    /// A GET under `/eth/v1/` or `/eth/v2/`, for diagnostics. No failover when pinned.
    pub fn raw_get(&self, health: &HealthBook, path: &str, pinned: Option<&str>) -> Result<Value, BeaconError> {
        if !(path.starts_with("/eth/v1/") || path.starts_with("/eth/v2/")) || path.contains("..") {
            return Err(BeaconError::BadRequest(format!("path must start with /eth/v1/ or /eth/v2/: '{path}'")));
        }
        let order: Vec<&Provider> = match pinned {
            Some(id) => vec![self.cfg.providers.iter().find(|p| p.id == id).ok_or_else(|| {
                BeaconError::UnknownProvider { chain_id: self.chain_id, id: id.to_string() }
            })?],
            None => self.order(health, None),
        };
        if order.is_empty() {
            return Err(BeaconError::NoProviders(self.chain_id));
        }
        let client = self.client()?;
        let mut attempts = Vec::new();
        for p in order {
            match get(&client, &p.endpoint, path, Duration::from_secs(self.cfg.timeout_secs)) {
                Ok(body) => {
                    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::String(body));
                    return Ok(json!({ "ok": true, "provider": p.id, "result": parsed }));
                }
                Err(e) => attempts.push(Attempt { provider: p.id.clone(), error: e }),
            }
        }
        Err(BeaconError::AllFailed { attempts, deadline: false })
    }
}

/// One GET. Anything but a 200 whose body fits [`MAX_BODY`] is an error naming why.
fn get(client: &reqwest::blocking::Client, base: &str, path: &str, timeout: Duration) -> Result<String, String> {
    get_with_fork(client, base, path, timeout).map(|(_, body)| body)
}

/// [`get`], plus the [`CONSENSUS_VERSION`] header when the answer carried a non-empty one.
fn get_with_fork(
    client: &reqwest::blocking::Client,
    base: &str,
    path: &str,
    timeout: Duration,
) -> Result<(Option<String>, String), String> {
    let resp = client
        .get(format!("{base}{path}"))
        .header(reqwest::header::ACCEPT, "application/json")
        .timeout(timeout)
        .send()
        .map_err(|e| describe(&e, timeout))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let fork = resp
        .headers()
        .get(CONSENSUS_VERSION)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let mut body = String::new();
    resp.take(MAX_BODY + 1)
        .read_to_string(&mut body)
        .map_err(|e| if e.to_string().contains("timed out") { format!("timeout after {}ms", timeout.as_millis()) } else { format!("reading body: {e}") })?;
    if body.len() as u64 > MAX_BODY {
        return Err(format!("body larger than {MAX_BODY} bytes"));
    }
    Ok((fork, body))
}

fn describe(e: &reqwest::Error, timeout: Duration) -> String {
    use std::error::Error;
    if e.is_timeout() {
        return format!("timeout after {}ms", timeout.as_millis());
    }
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(cause) = src {
        s.push_str(&format!(": {cause}"));
        src = cause.source();
    }
    s
}

/// A caller's deadline in ms; 0 or less means the default, and it is capped.
pub fn deadline(ms: i64) -> Duration {
    if ms <= 0 {
        DEFAULT_DEADLINE
    } else {
        Duration::from_millis(ms as u64).min(MAX_DEADLINE)
    }
}

/// The persisted map of chainId → [`ChainConfig`].
pub struct BeaconApi {
    chains: BTreeMap<u64, ChainConfig>,
    store_path: Option<PathBuf>,
    registry_path: Option<PathBuf>,
    /// `"<chainId>/<id>"` of every default provider ever offered, so a removed one stays removed.
    offered: BTreeSet<String>,
}

impl BeaconApi {
    pub fn new() -> Self {
        Self { chains: BTreeMap::new(), store_path: None, registry_path: None, offered: BTreeSet::new() }
    }

    pub fn with_store(path: PathBuf) -> Self {
        let mut s = Self::new();
        s.registry_path = path.parent().map(|p| p.join("registry.json"));
        s.store_path = Some(path);
        s.load();
        s
    }

    fn load(&mut self) {
        if let Some(text) = self.store_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            if let Ok(m) = serde_json::from_str::<BTreeMap<String, ChainConfig>>(&text) {
                self.chains = m.into_iter().filter_map(|(k, v)| k.parse().ok().map(|id| (id, v))).collect();
            }
        }
        if self.drop_retired_defaults() {
            self.persist();
        }
        if let Some(text) = self.registry_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()) {
            if let Some(ids) = serde_json::from_str::<Value>(&text).ok().and_then(|v| v.get("offeredDefaults").cloned()) {
                self.offered = serde_json::from_value(ids).unwrap_or_default();
            }
        }
    }

    /// Seeded entries of [`RETIRED_DEFAULTS`] go; a caller's own entry for the same URL stays.
    fn drop_retired_defaults(&mut self) -> bool {
        let mut changed = false;
        for (chain_id, chain) in self.chains.iter_mut() {
            let before = chain.providers.len();
            chain.providers.retain(|p| {
                p.source != ConfigSource::Builtin || !RETIRED_DEFAULTS.contains(&(*chain_id, p.endpoint.as_str()))
            });
            changed |= chain.providers.len() != before;
        }
        changed
    }

    fn persist(&self) {
        let Some(p) = &self.store_path else { return };
        let m: BTreeMap<String, &ChainConfig> = self.chains.iter().map(|(k, v)| (k.to_string(), v)).collect();
        write_json(p, &json!(m));
    }

    fn persist_registry(&self) {
        let Some(p) = &self.registry_path else { return };
        write_json(p, &json!({ "offeredDefaults": self.offered }));
    }

    pub fn chain(&self, chain_id: u64) -> Option<&ChainConfig> {
        self.chains.get(&chain_id)
    }

    pub fn chain_ids(&self) -> Vec<u64> {
        self.chains.keys().copied().collect()
    }

    pub fn plan(&self, chain_id: u64) -> Result<Plan, BeaconError> {
        let cfg = self.chains.get(&chain_id).ok_or(BeaconError::UnknownChain(chain_id))?;
        Ok(Plan { chain_id, cfg: cfg.clone() })
    }

    /// Seed [`DEFAULT_PROVIDERS`] per provider, only where absent and never twice per device.
    /// Returns the ids each chain gained.
    pub fn init_defaults(&mut self) -> BTreeMap<u64, Vec<String>> {
        let mut seeded: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        for b in DEFAULT_PROVIDERS {
            let key = format!("{}/{}", b.chain_id, b.id);
            if self.offered.contains(&key) {
                continue;
            }
            let chain = self.chains.entry(b.chain_id).or_default();
            let clash = chain.providers.iter().any(|p| p.id == b.id || p.endpoint == b.endpoint);
            if !clash {
                chain.providers.push(Provider {
                    id: b.id.to_string(),
                    endpoint: b.endpoint.to_string(),
                    enabled: true,
                    source: ConfigSource::Builtin,
                });
                seeded.entry(b.chain_id).or_default().push(b.id.to_string());
            }
        }
        if !seeded.is_empty() {
            self.persist();
        }
        let before = self.offered.len();
        self.offered.extend(DEFAULT_PROVIDERS.iter().map(|b| format!("{}/{}", b.chain_id, b.id)));
        if self.offered.len() != before {
            self.persist_registry();
        }
        seeded
    }

    /// Patch a chain's transport and, when given, replace its provider list. Omitted keeps;
    /// `"proxy": null` clears. Creates the chain if absent.
    pub fn set_chain_config(&mut self, chain_id: u64, patch: &Value) -> Result<(), String> {
        let obj = patch.as_object().ok_or("config must be a JSON object")?;
        let mut next = self.chains.get(&chain_id).cloned().unwrap_or_default();
        if let Some(v) = obj.get("proxy") {
            next.proxy = match v {
                Value::Null => None,
                Value::String(s) if s.trim().is_empty() => None,
                Value::String(s) => Some(s.trim().to_string()),
                _ => return Err("proxy must be a string or null".into()),
            };
        }
        if let Some(v) = obj.get("proxyRequired") {
            next.proxy_required = v.as_bool().ok_or("proxyRequired must be a boolean")?;
        }
        if let Some(v) = obj.get("timeoutSecs") {
            next.timeout_secs = v.as_u64().ok_or("timeoutSecs must be a positive integer")?;
        }
        if let Some(v) = obj.get("providers") {
            let list = v.as_array().ok_or("providers must be an array")?;
            next.providers = Vec::new();
            for item in list {
                let p = provider_from_caller(item, &next.providers)?;
                next.providers.push(p);
            }
        }
        next.validate()?;
        self.chains.insert(chain_id, next);
        self.persist();
        Ok(())
    }

    pub fn remove_chain(&mut self, chain_id: u64) -> bool {
        let removed = self.chains.remove(&chain_id).is_some();
        if removed {
            self.persist();
        }
        removed
    }

    /// Append a provider, creating the chain if absent. Returns the stored entry.
    pub fn add_provider(&mut self, chain_id: u64, item: &Value) -> Result<Provider, String> {
        let mut next = self.chains.get(&chain_id).cloned().unwrap_or_default();
        let p = provider_from_caller(item, &next.providers)?;
        next.providers.push(p.clone());
        next.validate()?;
        self.chains.insert(chain_id, next);
        self.persist();
        Ok(p)
    }

    pub fn remove_provider(&mut self, chain_id: u64, id: &str) -> Result<(), BeaconError> {
        let chain = self.chains.get_mut(&chain_id).ok_or(BeaconError::UnknownChain(chain_id))?;
        let before = chain.providers.len();
        chain.providers.retain(|p| p.id != id);
        if chain.providers.len() == before {
            return Err(BeaconError::UnknownProvider { chain_id, id: id.to_string() });
        }
        self.persist();
        Ok(())
    }

    /// Returns whether anything changed.
    pub fn set_provider_enabled(&mut self, chain_id: u64, id: &str, enabled: bool) -> Result<bool, BeaconError> {
        let chain = self.chains.get_mut(&chain_id).ok_or(BeaconError::UnknownChain(chain_id))?;
        let p = chain
            .providers
            .iter_mut()
            .find(|p| p.id == id)
            .ok_or_else(|| BeaconError::UnknownProvider { chain_id, id: id.to_string() })?;
        if p.enabled == enabled {
            return Ok(false);
        }
        p.enabled = enabled;
        self.persist();
        Ok(true)
    }

    pub fn list_providers(&self, chain_id: u64, health: &HealthBook) -> Result<Value, BeaconError> {
        let chain = self.chains.get(&chain_id).ok_or(BeaconError::UnknownChain(chain_id))?;
        let providers: Vec<Value> = chain
            .providers
            .iter()
            .map(|p| {
                json!({ "id": p.id, "endpoint": p.endpoint, "enabled": p.enabled, "source": p.source,
                        "https": p.https(), "health": health.get(chain_id, &p.id) })
            })
            .collect();
        Ok(json!({ "ok": true, "chainId": chain_id, "network": network(chain_id).map(|n| n.name),
                   "providers": providers }))
    }

    /// No network I/O: whether anything is configured, per chain.
    pub fn config_status(&self) -> Value {
        let chains: Vec<Value> = self
            .chains
            .iter()
            .map(|(id, c)| {
                json!({ "chainId": id, "network": network(*id).map(|n| n.name),
                        "providers": c.providers.len(),
                        "enabled": c.providers.iter().filter(|p| p.enabled).count() })
            })
            .collect();
        let state = if self.chains.is_empty() { "unconfigured" } else { "configured" };
        json!({ "ok": true, "state": state, "chains": chains })
    }
}

impl Default for BeaconApi {
    fn default() -> Self {
        Self::new()
    }
}

/// A provider as a caller sent it. `source` is always external; an omitted id is derived.
fn provider_from_caller(item: &Value, taken: &[Provider]) -> Result<Provider, String> {
    let endpoint = item.get("endpoint").and_then(Value::as_str).ok_or("provider needs an endpoint")?;
    let endpoint = normalize_endpoint(endpoint)?;
    let id = match item.get("id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => id.to_string(),
        None => derive_id(&endpoint, taken),
    };
    let enabled = match item.get("enabled") {
        None | Some(Value::Null) => true,
        Some(v) => v.as_bool().ok_or("enabled must be a boolean")?,
    };
    Ok(Provider { id, endpoint, enabled, source: ConfigSource::External })
}

fn write_json(path: &PathBuf, v: &Value) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_string_pretty(v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}
