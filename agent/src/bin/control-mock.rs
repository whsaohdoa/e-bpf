//! Runs the in-process mock control plane for manual testing.
//!
//! **This is not production code.** It exists so a human can point a real agent
//! at a control plane without a control-center deployment:
//!
//! ```text
//! cargo run -p agent --features mock --bin control-mock -- \
//!     --listen 127.0.0.1:9090 --config mock.json --print-certs /tmp/mock-certs
//! ```
//!
//! `--config` is optional and takes a JSON object with any subset of
//! [`MockConfig`](agent::mock::MockConfig)'s fields; unknown fields are
//! ignored. `--print-certs` is optional and defaults to a per-process directory
//! under the OS temporary directory; it receives the CA and the client
//! certificate the mock accepts.

use std::net::SocketAddr;
use std::path::PathBuf;

use agent::mock::{MockConfig, MockControl};

const USAGE: &str = "\
usage: control-mock [--listen ADDR] [--config FILE.json] [--print-certs DIR]

  --listen ADDR      address to serve on (default 127.0.0.1:9090)
  --config FILE      JSON file with any subset of MockConfig's fields
  --print-certs DIR  where to write ca.pem/server/client PEM files
  -h, --help         print this help

Not production code: no persistence, no authorization, no resource limits.";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut listen: SocketAddr = SocketAddr::from(([127, 0, 0, 1], 9090));
    let mut config: Option<PathBuf> = None;
    let mut print_certs: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = next_value(&mut args, "--listen")?.parse()?,
            "--config" => config = Some(PathBuf::from(next_value(&mut args, "--config")?)),
            "--print-certs" => {
                print_certs = Some(PathBuf::from(next_value(&mut args, "--print-certs")?));
            }
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?}\n\n{USAGE}"),
        }
    }

    let config = match config {
        Some(path) => MockConfig::from_json_file(&path)?,
        None => MockConfig::default(),
    };
    let cert_dir = print_certs.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("control-mock-{}", std::process::id()))
    });

    MockControl::serve(config, listen, &cert_dir).await
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> anyhow::Result<String> {
    args.next()
        .ok_or_else(|| anyhow::anyhow!("{flag} needs a value\n\n{USAGE}"))
}
