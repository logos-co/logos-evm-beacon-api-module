//! Logos module glue for `beacon_api_module` (rust-first authoring).
//!
//! The builder derives the `.lidl` from the `BeaconApiModule` trait below. Structured values
//! cross as JSON strings: `{ "ok": true, ... }` / `{ "ok": false, "code", "error" }`.
//!
//! `concurrency: "multi"`: a light-client fetch can take minutes (`updates` with a large count),
//! so calls run concurrently. Config lives behind a `RwLock`, and a fetch only holds it long
//! enough to copy its chain's config: no lock is held across network I/O.

use std::sync::RwLock;

use logos_rust_sdk::LogosModuleSDK;

use serde_json::{json, Value};

use crate::beacon::{deadline, BeaconApi, BeaconError, HealthBook, LcRequest, Plan};

pub trait BeaconApiModule: Send + Sync + 'static {
    /// Seed the built-in providers, per provider and only where absent; a default the user
    /// removed is never re-added. Any consumer may call it, any number of times.
    /// `{ ok, applied, seeded: { "<chainId>": [id, ...] } }`.
    fn init_defaults(&self) -> String;
    /// No network I/O. `{ ok, state: unready|unconfigured|configured, chains: [...] }`.
    fn config_status(&self) -> String;
    /// `{ ok, config: { providers, proxy?, proxyRequired, timeoutSecs } }`.
    fn get_beacon_config(&self, chain_id: i64) -> String;
    /// Patch `proxy` / `proxyRequired` / `timeoutSecs`, and replace `providers` when given.
    /// Omitted keeps; `"proxy": null` clears. Creates the chain if absent.
    fn set_beacon_config(&self, chain_id: i64, config_json: String) -> String;
    fn remove_beacon_config(&self, chain_id: i64) -> String;
    /// `{ ok, chainId, network, providers: [{ id, endpoint, enabled, source, https, health }] }`.
    fn list_providers(&self, chain_id: i64) -> String;
    /// `provider_json`: `{ endpoint, id?, enabled? }`. An omitted id is derived from the host.
    fn add_provider(&self, chain_id: i64, provider_json: String) -> String;
    fn remove_provider(&self, chain_id: i64, provider_id: String) -> String;
    fn set_provider_enabled(&self, chain_id: i64, provider_id: String, enabled: bool) -> String;
    /// Live check of one provider: whether it serves the light-client API and its slot lag.
    fn provider_health(&self, chain_id: i64, provider_id: String) -> String;

    /// One light-client request, named as libverifproxy's beacon transport names it:
    /// `getLightClientBootstrap` `{block_root}`, `getLightClientUpdatesByRange`
    /// `{start_period, count}`, `getLightClientOptimisticUpdate`, `getLightClientFinalityUpdate`.
    /// `provider` (may be empty) is tried first; the others follow on any failure.
    /// `deadline_ms` bounds the whole failover (0 = 30 s, capped at 600 s).
    /// `{ ok, provider, body, attempts }`; `body` is the provider's JSON text, verbatim.
    fn lc_request(&self, chain_id: i64, endpoint: String, params_json: String, provider: String,
                  deadline_ms: i64) -> String;
    /// The finalized header from https providers only. A starting point to show a user, not a
    /// trust anchor. `{ ok, root, slot, provider, endpoint }`.
    fn finalized_header(&self, chain_id: i64) -> String;
    /// A GET under `/eth/v1/` or `/eth/v2/`, for diagnostics. `{ ok, provider, result }`.
    fn raw_get(&self, chain_id: i64, path: String, provider: String) -> String;

    fn on_context_ready(&self, _ctx: &RustModuleContext) {}
}

pub trait BeaconApiModuleEvents {
    /// One chain's stored record was added, removed or altered. Re-read `get_beacon_config`.
    fn beacon_config_changed(&self, chain_id: i64);
}

include!(concat!(env!("CARGO_MANIFEST_DIR"), "/generated/provider_gen.rs"));

#[derive(Default)]
struct BeaconApiModuleImpl {
    api: RwLock<Option<BeaconApi>>,
    health: HealthBook,
}

fn err(e: impl std::fmt::Display) -> String {
    json!({ "ok": false, "error": e.to_string() }).to_string()
}

fn unready() -> String {
    json!({ "ok": false, "state": "unready", "code": "unready",
            "error": "beacon_api not initialized (context not ready)" })
    .to_string()
}

fn chain(chain_id: i64) -> Result<u64, String> {
    u64::try_from(chain_id).map_err(|_| format!("chain id must be positive, got {chain_id}"))
}

fn pinned(provider: &str) -> Option<&str> {
    Some(provider.trim()).filter(|p| !p.is_empty())
}

impl BeaconApiModuleImpl {
    fn read(&self, f: impl FnOnce(&BeaconApi) -> String) -> String {
        match self.api.read() {
            Ok(g) => g.as_ref().map(f).unwrap_or_else(unready),
            Err(_) => err("beacon_api lock poisoned"),
        }
    }

    /// The chain's config, copied out under the read lock.
    fn plan(&self, chain_id: i64) -> Result<Plan, String> {
        let id = chain(chain_id)?;
        let g = self.api.read().map_err(|_| err("beacon_api lock poisoned"))?;
        let api = g.as_ref().ok_or_else(unready)?;
        api.plan(id).map_err(|e| e.to_json().to_string())
    }

    /// Run a mutator and emit `beacon_config_changed` when the stored record moved.
    fn mutate(&self, chain_id: i64, f: impl FnOnce(&mut BeaconApi, u64) -> Value) -> String {
        let id = match chain(chain_id) {
            Ok(id) => id,
            Err(e) => return err(e),
        };
        let (out, changed) = {
            let mut g = match self.api.write() {
                Ok(g) => g,
                Err(_) => return err("beacon_api lock poisoned"),
            };
            let Some(api) = g.as_mut() else { return unready() };
            let before = api.chain(id).cloned();
            let out = f(api, id);
            (out, before.as_ref() != api.chain(id))
        };
        if changed {
            emit_beacon_config_changed(chain_id);
        }
        out.to_string()
    }
}

fn result(r: Result<Value, BeaconError>) -> String {
    match r {
        Ok(v) => v.to_string(),
        Err(e) => e.to_json().to_string(),
    }
}

impl BeaconApiModule for BeaconApiModuleImpl {
    fn on_context_ready(&self, ctx: &RustModuleContext) {
        let path = std::path::Path::new(&ctx.instance_persistence_path).join("beacon.json");
        *self.api.write().unwrap() = Some(BeaconApi::with_store(path));
    }

    fn init_defaults(&self) -> String {
        let seeded = {
            let mut g = match self.api.write() {
                Ok(g) => g,
                Err(_) => return err("beacon_api lock poisoned"),
            };
            match g.as_mut() {
                Some(api) => api.init_defaults(),
                None => return unready(),
            }
        };
        for id in seeded.keys() {
            emit_beacon_config_changed(*id as i64);
        }
        let map: serde_json::Map<String, Value> = seeded.iter().map(|(k, v)| (k.to_string(), json!(v))).collect();
        json!({ "ok": true, "applied": !seeded.is_empty(), "seeded": map }).to_string()
    }

    fn config_status(&self) -> String {
        self.read(|api| api.config_status().to_string())
    }

    fn get_beacon_config(&self, chain_id: i64) -> String {
        let id = match chain(chain_id) {
            Ok(id) => id,
            Err(e) => return err(e),
        };
        self.read(|api| match api.chain(id) {
            Some(c) => json!({ "ok": true, "chainId": id, "config": c }).to_string(),
            None => BeaconError::UnknownChain(id).to_json().to_string(),
        })
    }

    fn set_beacon_config(&self, chain_id: i64, config_json: String) -> String {
        let patch: Value = match serde_json::from_str(&config_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.mutate(chain_id, |api, id| match api.set_chain_config(id, &patch) {
            Ok(()) => json!({ "ok": true, "chainId": id, "config": api.chain(id) }),
            Err(e) => json!({ "ok": false, "code": "bad_config", "error": e }),
        })
    }

    fn remove_beacon_config(&self, chain_id: i64) -> String {
        self.mutate(chain_id, |api, id| json!({ "ok": true, "removed": api.remove_chain(id) }))
    }

    fn list_providers(&self, chain_id: i64) -> String {
        let id = match chain(chain_id) {
            Ok(id) => id,
            Err(e) => return err(e),
        };
        self.read(|api| result(api.list_providers(id, &self.health)))
    }

    fn add_provider(&self, chain_id: i64, provider_json: String) -> String {
        let item: Value = match serde_json::from_str(&provider_json) {
            Ok(v) => v,
            Err(e) => return err(e),
        };
        self.mutate(chain_id, |api, id| match api.add_provider(id, &item) {
            Ok(p) => json!({ "ok": true, "chainId": id, "provider": p }),
            Err(e) => json!({ "ok": false, "code": "bad_config", "error": e }),
        })
    }

    fn remove_provider(&self, chain_id: i64, provider_id: String) -> String {
        let out = self.mutate(chain_id, |api, id| match api.remove_provider(id, &provider_id) {
            Ok(()) => json!({ "ok": true, "chainId": id, "removed": provider_id }),
            Err(e) => e.to_json(),
        });
        if let Ok(id) = chain(chain_id) {
            self.health.forget(id, &provider_id);
        }
        out
    }

    fn set_provider_enabled(&self, chain_id: i64, provider_id: String, enabled: bool) -> String {
        self.mutate(chain_id, |api, id| match api.set_provider_enabled(id, &provider_id, enabled) {
            Ok(_) => json!({ "ok": true, "chainId": id, "provider": provider_id, "enabled": enabled }),
            Err(e) => e.to_json(),
        })
    }

    fn provider_health(&self, chain_id: i64, provider_id: String) -> String {
        match self.plan(chain_id) {
            Ok(plan) => result(plan.check_provider(&self.health, &provider_id)),
            Err(e) => e,
        }
    }

    fn lc_request(&self, chain_id: i64, endpoint: String, params_json: String, provider: String,
                  deadline_ms: i64) -> String {
        let params: Value = if params_json.trim().is_empty() {
            Value::Null
        } else {
            match serde_json::from_str(&params_json) {
                Ok(v) => v,
                Err(e) => return BeaconError::BadRequest(format!("params: {e}")).to_json().to_string(),
            }
        };
        let req = match LcRequest::parse(&endpoint, &params) {
            Ok(r) => r,
            Err(e) => return BeaconError::BadRequest(e).to_json().to_string(),
        };
        let plan = match self.plan(chain_id) {
            Ok(p) => p,
            Err(e) => return e,
        };
        match plan.fetch(&self.health, &req, pinned(&provider), deadline(deadline_ms)) {
            Ok(f) => json!({ "ok": true, "provider": f.provider, "body": f.body, "attempts": f.attempts }).to_string(),
            Err(e) => e.to_json().to_string(),
        }
    }

    fn finalized_header(&self, chain_id: i64) -> String {
        match self.plan(chain_id) {
            Ok(plan) => result(plan.finalized_header(&self.health)),
            Err(e) => e,
        }
    }

    fn raw_get(&self, chain_id: i64, path: String, provider: String) -> String {
        match self.plan(chain_id) {
            Ok(plan) => result(plan.raw_get(&self.health, &path, pinned(&provider))),
            Err(e) => e,
        }
    }
}

#[no_mangle]
pub extern "Rust" fn logos_module_install() {
    install::<BeaconApiModuleImpl>();
}
