//! Runs the in-process mock control plane for manual testing.
//!
//! **This is not production code.** It exists so a human can point a real agent
//! at a control plane without a control-center deployment:
//!
//! ```text
//! cargo run -p agent --features mock --bin control-mock -- \
//!     --listen 127.0.0.1:9090 --certs .tmp/certs
//! ```
//!
//! With no `--config` the mock publishes the contract's §6.4 document B (one
//! `r-8080` rule) as version 1 at startup, so an agent pointed at it applies a
//! document and acknowledges it without any further setup.
//!
//! `--config` is optional and takes a JSON object with any subset of
//! [`MockConfig`](agent::mock::MockConfig)'s fields; unknown fields are
//! ignored. `--certs` (alias `--print-certs`) is optional and defaults to a
//! per-process directory under the OS temporary directory; it receives the CA
//! and the client certificate the mock accepts.
//!
//! `--admin-listen` additionally starts the plaintext HTTP admin surface
//! (`GET /health`, `POST /publish`, `GET /acks`, `GET /state`) that
//! `verification/run_control_demo.sh` drives. It is loopback-only scaffolding:
//! never expose it.

use std::{net::SocketAddr, path::PathBuf};

use agent::mock::{MockConfig, MockControl};

/// Contract `docs/Control-Agent-API-v1.md` §6.4, document B: 10000 ms window,
/// 2000 ms grace, /24 and /64 prefixes, one `r-8080` BLOCK rule. 63 bytes, the
/// exact vector whose sha256 the contract pins.
const DOCUMENT_B: &str = "080110904e18d00f201828403a3108011206722d38303830180a20012a1f080410061a0b\
31302e302e312e302f3234220831302e302e322e352a02903f3001";

const USAGE: &str = "\
usage: control-mock [--listen ADDR] [--config FILE.json] [--certs DIR]
                    [--admin-listen ADDR]

  --listen ADDR        address to serve gRPC on (default 127.0.0.1:9090)
  --config FILE        JSON file with any subset of MockConfig's fields
  --certs DIR          where to write ca.pem/agent.pem/agent.key
  --print-certs DIR    same as --certs
  --admin-listen ADDR  serve the HTTP admin surface on ADDR (off by default)
  -h, --help           print this help

Not production code: no persistence, no authorization, no resource limits.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut listen: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 9090));
    let mut config: Option<PathBuf> = None;
    let mut print_certs: Option<PathBuf> = None;
    let mut admin_listen: Option<SocketAddr> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = next_value(&mut args, "--listen")?.parse()?,
            "--config" => config = Some(PathBuf::from(next_value(&mut args, "--config")?)),
            "--certs" | "--print-certs" => {
                print_certs = Some(PathBuf::from(next_value(&mut args, &arg)?));
            }
            "--admin-listen" => {
                admin_listen = Some(next_value(&mut args, "--admin-listen")?.parse()?)
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    let mut config = match config {
        Some(path) => MockConfig::from_json_file(&path)?,
        None => MockConfig::default(),
    };
    // The zero-configuration case is the README walkthrough: publish something
    // real so the agent logs an ack instead of idling at "no change".
    if config.initial_document_hex.is_none() {
        config.initial_document_hex = Some(DOCUMENT_B.to_owned());
    }
    if config.initial_version.is_none() {
        config.initial_version = Some(config.desired_version.max(1));
    }
    if let Some(addr) = admin_listen {
        config.admin_listen = Some(addr);
    }
    let cert_dir = print_certs.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("control-mock-{}", std::process::id()))
    });

    MockControl::serve(config, listen, &cert_dir).await
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value\n\n{USAGE}"))
}
