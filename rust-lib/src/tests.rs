use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::beacon::*;

type Route = dyn Fn(&str) -> (u16, String, u64) + Send + Sync;

/// A plain-http stand-in for one provider: `route(path) -> (status, body, delay_ms)`.
struct Server {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
}

fn serve(route: impl Fn(&str) -> (u16, String, u64) + Send + Sync + 'static) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(Mutex::new(Vec::new()));
    let route: Arc<Route> = Arc::new(route);
    let seen = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let (route, seen) = (route.clone(), seen.clone());
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match s.read(&mut chunk) {
                        Ok(0) | Err(_) => return,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf);
                let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                seen.lock().unwrap().push(path.clone());
                let (status, body, delay) = route(&path);
                std::thread::sleep(Duration::from_millis(delay));
                let _ = write!(
                    s,
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    Server { base, hits }
}

impl Server {
    fn hits(&self) -> Vec<String> {
        self.hits.lock().unwrap().clone()
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// An optimistic update for mainnet signed `behind` slots ago.
fn head_update(behind: u64) -> String {
    let slot = (now() - network(1).unwrap().genesis_time) / 12 - behind;
    json!({ "version": "fulu", "data": { "attested_header": { "beacon": { "slot": (slot - 1).to_string() } },
                                         "signature_slot": slot.to_string() } })
    .to_string()
}

fn provider(id: &str, endpoint: &str) -> Provider {
    Provider { id: id.into(), endpoint: endpoint.into(), enabled: true, source: ConfigSource::External }
}

fn plan(chain_id: u64, providers: Vec<Provider>) -> Plan {
    Plan { chain_id, cfg: ChainConfig { providers, timeout_secs: 5, ..ChainConfig::default() } }
}

#[test]
fn bootstrap_root_gains_0x_and_is_checked() {
    let root = "AB".repeat(32);
    let r = LcRequest::parse("getLightClientBootstrap", &json!({ "block_root": root })).unwrap();
    assert_eq!(r.path(), format!("/eth/v1/beacon/light_client/bootstrap/0x{}", "ab".repeat(32)));
    let same = LcRequest::parse("getLightClientBootstrap", &json!({ "block_root": format!("0x{root}") })).unwrap();
    assert_eq!(r, same);
    assert!(LcRequest::parse("getLightClientBootstrap", &json!({ "block_root": "0x1234" })).is_err());
    assert!(LcRequest::parse("getLightClientBootstrap", &json!({})).is_err());
}

#[test]
fn updates_take_numbers_or_strings_and_bound_count() {
    let r = LcRequest::parse("getLightClientUpdatesByRange", &json!({ "start_period": 1900, "count": 4 })).unwrap();
    assert_eq!(r.path(), "/eth/v1/beacon/light_client/updates?start_period=1900&count=4");
    let s = LcRequest::parse("getLightClientUpdatesByRange", &json!({ "start_period": "1900", "count": "4" })).unwrap();
    assert_eq!(r, s);
    assert!(LcRequest::parse("getLightClientUpdatesByRange", &json!({ "start_period": 1, "count": 0 })).is_err());
    assert!(LcRequest::parse("getLightClientUpdatesByRange", &json!({ "start_period": 1, "count": 129 })).is_err());
    assert!(LcRequest::parse("getLightClientUpdatesByRange", &json!({ "count": 1 })).is_err());
}

#[test]
fn head_routes_and_unknown_endpoint() {
    assert_eq!(
        LcRequest::parse("getLightClientOptimisticUpdate", &json!({})).unwrap().path(),
        "/eth/v1/beacon/light_client/optimistic_update"
    );
    assert_eq!(
        LcRequest::parse("getLightClientFinalityUpdate", &Value::Null).unwrap().path(),
        "/eth/v1/beacon/light_client/finality_update"
    );
    assert!(LcRequest::parse("getBlock", &json!({})).is_err());
}

#[test]
fn defaults_seed_once_per_provider() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("beacon.json");
    let mut api = BeaconApi::with_store(path.clone());
    let seeded = api.init_defaults();
    assert_eq!(seeded[&11_155_111], vec!["lodestar", "nimbus-unstable"]);
    assert!(api.chain(1).unwrap().providers.iter().all(|p| p.source == ConfigSource::Builtin));
    assert!(api.init_defaults().is_empty(), "a second call writes nothing");

    api.remove_provider(1, "publicnode").unwrap();
    let mut reopened = BeaconApi::with_store(path);
    assert!(reopened.init_defaults().is_empty(), "a removed default stays removed");
    assert!(reopened.chain(1).unwrap().providers.iter().all(|p| p.id != "publicnode"));
}

#[test]
fn defaults_never_clobber_a_callers_entry() {
    let mut api = BeaconApi::new();
    api.add_provider(1, &json!({ "id": "mine", "endpoint": "https://lodestar-mainnet.chainsafe.io/" })).unwrap();
    api.init_defaults();
    let ids: Vec<_> = api.chain(1).unwrap().providers.iter().map(|p| p.id.clone()).collect();
    assert_eq!(ids, vec!["mine", "publicnode", "nimbus-testing", "nimbus-unstable"]);
}

#[test]
fn patch_keeps_omitted_fields_and_null_clears_proxy() {
    let mut api = BeaconApi::new();
    api.set_chain_config(1, &json!({ "proxy": "socks5h://127.0.0.1:9050", "proxyRequired": true })).unwrap();
    api.set_chain_config(1, &json!({ "timeoutSecs": 20 })).unwrap();
    let c = api.chain(1).unwrap();
    assert_eq!((c.proxy.as_deref(), c.proxy_required, c.timeout_secs), (Some("socks5h://127.0.0.1:9050"), true, 20));
    api.set_chain_config(1, &json!({ "proxy": null, "proxyRequired": false })).unwrap();
    assert_eq!(api.chain(1).unwrap().proxy, None);
}

#[test]
fn bad_configs_are_refused_and_change_nothing() {
    let mut api = BeaconApi::new();
    api.set_chain_config(1, &json!({ "providers": [{ "id": "a", "endpoint": "https://a.example" }] })).unwrap();
    for bad in [
        json!({ "providers": [{ "id": "a", "endpoint": "https://a.example" }, { "id": "a", "endpoint": "https://b.example" }] }),
        json!({ "providers": [{ "id": "x", "endpoint": "https://a.example" }, { "id": "y", "endpoint": "https://a.example/" }] }),
        json!({ "providers": [{ "id": "Not_Valid", "endpoint": "https://a.example" }] }),
        json!({ "providers": [{ "endpoint": "ftp://a.example" }] }),
        json!({ "providers": [{ "endpoint": "https://a.example?key=1" }] }),
        json!({ "timeoutSecs": 0 }),
        json!({ "proxy": "ftp://127.0.0.1:21" }),
        json!([]),
    ] {
        assert!(api.set_chain_config(1, &bad).is_err(), "accepted {bad}");
    }
    assert_eq!(api.chain(1).unwrap().providers, vec![provider("a", "https://a.example")]);
}

#[test]
fn added_providers_get_a_host_id_and_no_duplicates() {
    let mut api = BeaconApi::new();
    let p = api.add_provider(5, &json!({ "endpoint": "https://beacon.example.org/rest/" })).unwrap();
    assert_eq!((p.id.as_str(), p.endpoint.as_str()), ("beacon-example-org", "https://beacon.example.org/rest"));
    let q = api.add_provider(5, &json!({ "endpoint": "https://beacon.example.org" })).unwrap();
    assert_eq!(q.id, "beacon-example-org-2");
    assert!(api.add_provider(5, &json!({ "endpoint": "https://beacon.example.org/rest" })).is_err());
    assert_eq!(api.add_provider(5, &json!({ "endpoint": "https://x.example", "source": "default" })).unwrap().source,
               ConfigSource::External);
}

#[test]
fn pinned_provider_is_tried_first() {
    let a = serve(|_| (200, head_update(0), 0));
    let b = serve(|_| (200, head_update(0), 0));
    let p = plan(1, vec![provider("a", &a.base), provider("b", &b.base)]);
    let got = p.fetch(&HealthBook::default(), &LcRequest::Optimistic, Some("b"), Duration::from_secs(5)).unwrap();
    assert_eq!(got.provider, "b");
    assert!(a.hits().is_empty());
}

#[test]
fn a_failing_pinned_provider_fails_over() {
    let a = serve(|_| (404, "{\"message\":\"no light client data\"}".into(), 0));
    let b = serve(|_| (200, "[]".into(), 0));
    let p = plan(11_155_111, vec![provider("a", &a.base), provider("b", &b.base)]);
    let req = LcRequest::Updates { start_period: 7, count: 2 };
    let health = HealthBook::default();
    let got = p.fetch(&health, &req, Some("a"), Duration::from_secs(5)).unwrap();
    assert_eq!((got.provider.as_str(), got.body.as_str()), ("b", "[]"));
    assert_eq!(got.attempts, vec![Attempt { provider: "a".into(), error: "HTTP 404".into() }]);
    assert_eq!(a.hits(), vec!["/eth/v1/beacon/light_client/updates?start_period=7&count=2"]);
    assert_eq!(health.get(11_155_111, "a").consecutive_failures, 1);
    assert_eq!(health.get(11_155_111, "b").served, 1);
}

#[test]
fn a_stale_head_update_fails_over() {
    let lagging = serve(|_| (200, head_update(MAX_HEAD_LAG_SLOTS + 50), 0));
    let fresh = serve(|_| (200, head_update(1), 0));
    let p = plan(1, vec![provider("lagging", &lagging.base), provider("fresh", &fresh.base)]);
    let got = p.fetch(&HealthBook::default(), &LcRequest::Finality, None, Duration::from_secs(5)).unwrap();
    assert_eq!(got.provider, "fresh");
    assert!(got.attempts[0].error.starts_with("stale:"), "{:?}", got.attempts);
}

#[test]
fn a_200_that_is_not_json_fails_over() {
    let portal = serve(|_| (200, "<html>sign in to the wifi</html>".into(), 0));
    let good = serve(|_| (200, "{\"data\":{}}".into(), 0));
    let p = plan(1, vec![provider("portal", &portal.base), provider("good", &good.base)]);
    let req = LcRequest::Bootstrap { block_root: format!("0x{}", "00".repeat(32)) };
    let got = p.fetch(&HealthBook::default(), &req, None, Duration::from_secs(5)).unwrap();
    assert_eq!(got.provider, "good");
}

#[test]
fn every_failure_is_named_with_a_code() {
    let a = serve(|_| (500, "{}".into(), 0));
    let b = serve(|_| (404, "{}".into(), 0));
    let p = plan(1, vec![provider("a", &a.base), provider("b", &b.base)]);
    let err = p.fetch(&HealthBook::default(), &LcRequest::Optimistic, None, Duration::from_secs(5)).unwrap_err();
    assert_eq!(err.code(), "unavailable");
    assert_eq!(err.to_json()["attempts"][1]["error"], "HTTP 404");
    assert_eq!(err.to_string(), "every provider failed: a: HTTP 500; b: HTTP 404");
}

#[test]
fn the_deadline_bounds_the_whole_failover() {
    let slow = serve(|_| (200, head_update(0), 3000));
    let never = serve(|_| (200, head_update(0), 0));
    let p = plan(1, vec![provider("slow", &slow.base), provider("never", &never.base)]);
    let t0 = std::time::Instant::now();
    let err = p.fetch(&HealthBook::default(), &LcRequest::Optimistic, None, Duration::from_millis(400)).unwrap_err();
    assert!(t0.elapsed() < Duration::from_millis(2500), "took {:?}", t0.elapsed());
    assert_eq!(err.code(), "timeout");
    assert!(never.hits().is_empty());
}

#[test]
fn a_provider_that_keeps_failing_is_tried_last() {
    let bad = serve(|_| (503, "{}".into(), 0));
    let good = serve(|_| (200, head_update(0), 0));
    let p = plan(1, vec![provider("bad", &bad.base), provider("good", &good.base)]);
    let health = HealthBook::default();
    for _ in 0..3 {
        p.fetch(&health, &LcRequest::Optimistic, None, Duration::from_secs(5)).unwrap();
    }
    assert_eq!(bad.hits().len(), 3);
    let got = p.fetch(&health, &LcRequest::Optimistic, None, Duration::from_secs(5)).unwrap();
    assert_eq!((got.provider.as_str(), got.attempts.len()), ("good", 0));
    assert_eq!(bad.hits().len(), 3, "cooling: not tried while another provider answers");
}

#[test]
fn disabled_providers_are_skipped_and_none_left_is_an_error() {
    let a = serve(|_| (200, head_update(0), 0));
    let mut off = provider("a", &a.base);
    off.enabled = false;
    let err = plan(1, vec![off]).fetch(&HealthBook::default(), &LcRequest::Optimistic, Some("a"), Duration::from_secs(5));
    assert_eq!(err.unwrap_err().code(), "no_providers");
    assert!(a.hits().is_empty());
}

#[test]
fn proxy_required_without_a_proxy_sends_nothing() {
    let a = serve(|_| (200, head_update(0), 0));
    let mut p = plan(1, vec![provider("a", &a.base)]);
    p.cfg.proxy_required = true;
    let err = p.fetch(&HealthBook::default(), &LcRequest::Optimistic, None, Duration::from_secs(5)).unwrap_err();
    assert_eq!(err.code(), "proxy");
    assert!(a.hits().is_empty());
}

#[test]
fn the_finalized_root_never_comes_over_plain_http() {
    let a = serve(|_| (200, json!({ "data": { "root": format!("0x{}", "11".repeat(32)) } }).to_string(), 0));
    let p = plan(1, vec![provider("a", &a.base)]);
    assert_eq!(p.finalized_header(&HealthBook::default()).unwrap_err().code(), "no_providers");
    assert!(a.hits().is_empty());
}

#[test]
fn provider_health_reports_lag() {
    let lagging = serve(|_| (200, head_update(15_800), 0));
    let p = plan(1, vec![provider("lagging", &lagging.base)]);
    let v = p.check_provider(&HealthBook::default(), "lagging").unwrap();
    assert_eq!((v["servesLightClient"].as_bool(), v["healthy"].as_bool()), (Some(true), Some(false)));
    assert!(v["lagSlots"].as_u64().unwrap() >= 15_800);
    let missing = serve(|_| (404, "{}".into(), 0));
    let p = plan(1, vec![provider("missing", &missing.base)]);
    let v = p.check_provider(&HealthBook::default(), "missing").unwrap();
    assert_eq!((v["servesLightClient"].as_bool(), v["error"].as_str()), (Some(false), Some("HTTP 404")));
}

#[test]
fn raw_get_stays_under_the_beacon_api() {
    let a = serve(|_| (200, "{\"data\":{\"genesis_time\":\"1\"}}".into(), 0));
    let p = plan(1, vec![provider("a", &a.base)]);
    let v = p.raw_get(&HealthBook::default(), "/eth/v1/beacon/genesis", Some("a")).unwrap();
    assert_eq!(v["result"]["data"]["genesis_time"], "1");
    for bad in ["/admin", "/eth/v1/../x", "eth/v1/beacon/genesis"] {
        assert_eq!(p.raw_get(&HealthBook::default(), bad, None).unwrap_err().code(), "bad_request");
    }
}

#[test]
fn lag_is_measured_from_genesis() {
    let g = network(11_155_111).unwrap().genesis_time;
    assert_eq!(lag_slots(11_155_111, 100, g + 12 * 110), Some(10));
    assert_eq!(lag_slots(11_155_111, 200, g + 12 * 110), Some(0));
    assert_eq!(lag_slots(42, 1, g), None);
}

#[test]
fn deadlines_default_and_cap() {
    assert_eq!(deadline(0), Duration::from_secs(30));
    assert_eq!(deadline(-5), Duration::from_secs(30));
    assert_eq!(deadline(1500), Duration::from_millis(1500));
    assert_eq!(deadline(i64::MAX), Duration::from_secs(600));
}

#[test]
fn ids_are_dns_labels() {
    for ok in ["a", "publicnode", "nimbus-testing", "x1"] {
        assert!(valid_id(ok), "{ok}");
    }
    for bad in ["", "-a", "a-", "A", "a_b", "a.b", &"a".repeat(41)] {
        assert!(!valid_id(bad), "{bad}");
    }
}

#[test]
fn only_proxy_rs_builds_a_client() {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(src).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().unwrap() == "proxy.rs" || path.file_name().unwrap() == "tests.rs" {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("Client::builder") && !text.contains("Client::new"), "{}", path.display());
    }
}
