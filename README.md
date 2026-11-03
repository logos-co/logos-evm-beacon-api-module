# beacon_api_module

A proxyable, fail-closed Ethereum **Beacon API** client for Logos, the consensus-layer
twin of [`eth_rpc_module`](https://github.com/logos-co/logos-evm-eth-rpc-module).

It is the one place where consensus-layer providers are configured, per chain. The
verified proxy takes the enabled providers' URLs from it when it starts and fetches from
them with its own HTTP client, so this module's failover and proxy do not apply there; it
refuses to start on a chain set to go through a proxy. Apps that call `lc_request` share
one provider list, one proxy policy and one failover.

## What it does

- **Provider lists per chainId** (1, 11155111, 560048, or any other), persisted in the
  module's own data directory. Each provider has a stable `id` (a DNS label) and an
  `endpoint`; ids stay put across edits.
- **Failover.** A light-client request goes to the pinned provider first, then to the
  others on any failure: a transport error, a non-200, a body that is not JSON, or a head
  update more than 64 slots behind the clock. A provider that failed three times in a row
  is tried last for a minute.
- **Fail-closed proxying.** `proxy` (e.g. `socks5h://127.0.0.1:9050`) and `proxyRequired`
  per chain, through the same inlined `logos-net-proxy` chokepoint as `eth_rpc_module`.
- **Defaults.** `init_defaults()` seeds providers that were probed to serve all four
  light-client routes. Plain-http ones are fallbacks behind the https ones; they are safe
  for light-client data, which the light client verifies, but `finalized_header` never
  uses them, because a root over plain http could be swapped on the way.

| Chain | Default providers |
|---|---|
| mainnet (1) | publicnode, lodestar (chainsafe), nimbus-testing (http), nimbus-unstable (http) |
| sepolia (11155111) | lodestar (chainsafe), nimbus-unstable (http) |
| hoodi (560048) | publicnode, lodestar (chainsafe), nimbus-testing (http) |

## Methods

Every method answers a JSON string: `{ "ok": true, ... }` or
`{ "ok": false, "code": "...", "error": "..." }`.

| Method | |
|---|---|
| `init_defaults()` | Seed defaults per provider, only where absent; a removed default stays removed. |
| `config_status()` | No network I/O: `unready` / `unconfigured` / `configured`, per chain. |
| `get_beacon_config(chain_id)` | `{ providers, proxy?, proxyRequired, timeoutSecs }`. |
| `set_beacon_config(chain_id, json)` | Patch `proxy` / `proxyRequired` / `timeoutSecs`; replace `providers` when given. Omitted keeps, `"proxy": null` clears. |
| `remove_beacon_config(chain_id)` | |
| `list_providers(chain_id)` | Providers with their in-memory health (served, failures, last error, latency). |
| `add_provider(chain_id, json)` | `{ endpoint, id?, enabled? }`; an omitted id is derived from the host. |
| `remove_provider(chain_id, id)` / `set_provider_enabled(chain_id, id, bool)` | |
| `provider_health(chain_id, id)` | Live check: does it serve the light-client API, and how many slots behind is it. |
| `lc_request(chain_id, endpoint, params_json, provider, deadline_ms)` | One light-client request, named as libverifproxy names it (`getLightClientBootstrap`, `getLightClientUpdatesByRange`, `getLightClientOptimisticUpdate`, `getLightClientFinalityUpdate`). Answers `{ ok, provider, body, attempts }`, `body` verbatim. |
| `finalized_header(chain_id)` | The finalized root from https providers only. A starting point to show a user, not a trust anchor. |
| `raw_get(chain_id, path, provider)` | A GET under `/eth/v1/` or `/eth/v2/`, for diagnostics. |

Event: `beacon_config_changed(chain_id)` whenever a chain's stored record changes.

```bash
logosctl call beacon_api_module init_defaults
logosctl call beacon_api_module list_providers 11155111
logosctl call beacon_api_module provider_health 11155111 lodestar
logosctl call beacon_api_module add_provider 1 'str:{"endpoint":"https://my-beacon.example"}'
logosctl call beacon_api_module lc_request 11155111 getLightClientOptimisticUpdate '{}' '' 0
```

## Trust

The module and its providers can withhold or delay light-client data, but cannot forge
it: the light client checks sync-committee signatures from its trusted root onwards. So
a provider is untrusted by construction, and failover is about availability only.

## Layout

- `rust-lib/src/beacon.rs` — the core: config store, failover, staleness check. No Logos code.
- `rust-lib/src/proxy.rs` — the fail-closed client constructor (inlined `logos-net-proxy`).
- `rust-lib/src/glue.rs` — the Logos glue; the builder derives the `.lidl` from its trait.

```bash
cd rust-lib && cargo test --no-default-features   # core tests, against local mock providers
nix build .#lgx                                     # the installable package
```

`cargo test` needs `../logos-rust-sdk-src` to resolve the optional SDK path dependency;
link it to the source of the `logos-rust-sdk` input pinned by `logos-module-builder`.
