//! `anchor-proxy --config proxy.toml`: serve the free tier on the loopback,
//! behind a TLS-terminating reverse proxy. Refuses to start unless every
//! provider account is cleared by a terms ADR and the registry validates.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anchor_hub::http::{ServerConfig, serve_on};
use anchor_inference::Registry;
use anchor_proxy::config::ProxyConfig;
use anchor_proxy::server::{Proxy, system_clock};
use anchor_proxy::store::Store;
use anchor_proxy::upstream::HttpUpstream;

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match args.as_slice() {
        [flag, path] if flag == "--config" => PathBuf::from(path),
        _ => return Err("usage: anchor-proxy --config proxy.toml".into()),
    };
    let base = config
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let text =
        std::fs::read_to_string(&config).map_err(|e| format!("{}: {e}", config.display()))?;
    let cfg = ProxyConfig::from_toml(&text).map_err(|e| e.to_string())?;
    let reg_path = base.join(&cfg.registry);
    let reg_text =
        std::fs::read_to_string(&reg_path).map_err(|e| format!("{}: {e}", reg_path.display()))?;
    let registry = Registry::from_toml(&reg_text).map_err(|e| e.to_string())?;
    let keys: BTreeMap<String, String> = cfg
        .accounts
        .iter()
        .filter_map(|a| {
            std::env::var(&a.key_env)
                .ok()
                .map(|v| (a.key_env.clone(), v))
        })
        .collect();
    let store = Store::open(&base.join(&cfg.database)).map_err(|e| e.to_string())?;
    let port = cfg.port;
    let proxy = Proxy::new(
        cfg,
        registry,
        &base,
        store,
        &keys,
        Arc::new(HttpUpstream::new(Duration::from_secs(300))),
        system_clock(),
    )?;
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| e.to_string())?;
    let server = ServerConfig {
        workers: 32,
        backlog: 256,
        max_body_bytes: 8 << 20,
        // Provider calls can take minutes; the response is written after.
        write_timeout: Duration::from_secs(30),
        ..ServerConfig::default()
    };
    eprintln!("anchor-proxy: serving on 127.0.0.1:{port}");
    serve_on(listener, Arc::new(proxy), server)
        .map_err(|e| e.to_string())?
        .wait();
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("anchor-proxy: {e}");
        std::process::exit(1);
    }
}
