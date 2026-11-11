//! Developer entry point: `cargo xtask <command>`.
//!
//! The alias lives in `.cargo/config.toml`, so every command below is one
//! `cargo` invocation away from a fresh checkout:
//!
//! | command | what it runs |
//! | --- | --- |
//! | `cargo xtask test [--layer l0\|l1\|l2\|all]` | unit, contract and in-process pipeline tests |
//! | `cargo xtask bpf` | the BPF object build with the pinned nightly toolchain |
//! | `cargo xtask e2e --legacy [--script <name>]` | the bash suites in `verification/` |
//! | `cargo xtask report` | the JUnit summary of the last `xtask test` run |
//! | `cargo xtask soak --minutes <n>` | placeholder for the L4 soak run |
//!
//! `cargo test` no longer needs `sudo`: the `runner = "sudo -E"` override that
//! used to apply to every test binary is gone. Privileges are only needed by the
//! eBPF loading path, which the e2e layer (not the unit layers) exercises.

use std::{
    env,
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};

/// Integration test files that belong to the in-process pipeline layer rather
/// than to the CLI contract layer.
const PIPELINE_TESTS: [&str; 1] = ["pipeline"];

/// Layers in the order a full run executes them: cheapest first, so a unit test
/// failure is reported before the slower layers start.
const ALL_LAYERS: [Layer; 3] = [Layer::L0, Layer::L1, Layer::L2];

fn main() -> ExitCode {
    match dispatch() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::from(2)
        }
    }
}

fn dispatch() -> Result<ExitCode, String> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let rest: Vec<String> = args.collect();
    match command.as_str() {
        "test" => test(&rest),
        "bpf" => bpf(&rest),
        "e2e" => e2e(&rest),
        "report" => report(),
        "soak" => soak(&rest),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            Ok(ExitCode::SUCCESS)
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    }
}

const USAGE: &str = "\
usage: cargo xtask <command>

commands:
  test [--layer l0|l1|l2|all]   run the test layers (default: all)
  bpf                           build the eBPF object; nightly is the default toolchain
  e2e --legacy [--script NAME]  run the bash suites in verification/ (needs /workspace)
  e2e --case ID                 run one L3 case (arrives with the e2e driver)
  report                        summarise the JUnit report of the last test run
  soak --minutes N              placeholder for the L4 soak run";

// ---------------------------------------------------------------- test layers

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layer {
    L0,
    L1,
    L2,
}

impl Layer {
    fn name(self) -> &'static str {
        match self {
            Self::L0 => "l0",
            Self::L1 => "l1",
            Self::L2 => "l2",
        }
    }

    fn describe(self) -> &'static str {
        match self {
            Self::L0 => "L0 unit tests",
            Self::L1 => "L1 contract tests",
            Self::L2 => "L2 in-process pipeline tests",
        }
    }
}

fn parse_layers(value: &str) -> Result<Vec<Layer>, String> {
    match value {
        "all" => Ok(ALL_LAYERS.to_vec()),
        "l0" => Ok(vec![Layer::L0]),
        "l1" => Ok(vec![Layer::L1]),
        "l2" => Ok(vec![Layer::L2]),
        other => Err(format!(
            "unknown layer `{other}`: expected l0, l1, l2 or all"
        )),
    }
}

fn test(args: &[String]) -> Result<ExitCode, String> {
    let layers = parse_layers(&flag_value(args, "--layer")?.unwrap_or_else(|| "all".into()))?;
    let nextest = nextest_available();

    for layer in layers {
        run_layer(layer, nextest)?;
    }

    if nextest {
        let junit = junit_path();
        if junit.is_file() {
            println!("xtask: JUnit report: {}", junit.display());
        }
    } else {
        println!("xtask: cargo-nextest not found, fell back to `cargo test`");
    }
    Ok(ExitCode::SUCCESS)
}

fn run_layer(layer: Layer, nextest: bool) -> Result<(), String> {
    match layer {
        Layer::L0 => {
            // `probe` and `probe-common` are the default members; naming them
            // explicitly keeps the layer identical when a test file is added to
            // another member later.
            let mut args = runner_args(nextest);
            args.extend([
                "-p".into(),
                "probe".into(),
                "-p".into(),
                "probe-common".into(),
            ]);
            args.push("--lib".into());
            run(cargo_command(&args), layer.describe())
        }
        Layer::L1 | Layer::L2 => {
            let files = integration_tests(layer)?;
            if files.is_empty() {
                println!(
                    "xtask: no {} test files under probe/tests yet — nothing to run",
                    layer.name()
                );
                return Ok(());
            }
            for file in files {
                let mut args = runner_args(nextest);
                args.extend(["-p".into(), "probe".into(), "--test".into(), file.clone()]);
                run(
                    cargo_command(&args),
                    &format!("{} ({file})", layer.describe()),
                )?;
            }
            Ok(())
        }
    }
}

/// Test files in `probe/tests`, split by layer: `pipeline` drives the in-process
/// pipeline, everything else asserts the CLI contract.
fn integration_tests(layer: Layer) -> Result<Vec<String>, String> {
    let dir = workspace_root().join("probe/tests");
    let entries = std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;
    let mut names = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|e| format!("read {}: {e}", dir.display()))?
            .path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        // Files starting with `_` are shared helpers, not test targets.
        if stem.starts_with('_') {
            continue;
        }
        let is_pipeline = PIPELINE_TESTS.contains(&stem);
        let matches = match layer {
            Layer::L2 => is_pipeline,
            Layer::L1 => !is_pipeline,
            Layer::L0 => false,
        };
        if matches {
            names.push(stem.to_owned());
        }
    }
    names.sort();
    Ok(names)
}

/// `cargo nextest run --profile ci` when nextest is installed, plain
/// `cargo test` otherwise. The `ci` profile only adds the JUnit report.
fn runner_args(nextest: bool) -> Vec<String> {
    if nextest {
        vec![
            "nextest".into(),
            "run".into(),
            "--profile".into(),
            "ci".into(),
        ]
    } else {
        vec!["test".into()]
    }
}

fn nextest_available() -> bool {
    on_path("cargo-nextest")
}

fn on_path(name: &str) -> bool {
    let Some(path) = env::var_os("PATH") else {
        return false;
    };
    env::split_paths(&path).any(|dir| dir.join(name).is_file())
}

fn junit_path() -> PathBuf {
    workspace_root().join("target/nextest/ci/junit.xml")
}

// --------------------------------------------------------------------- e2e

/// The bash suites hardcode `/workspace/{probe,verification}`, which is exactly
/// how `docker-compose.yml` mounts the repository into the dev container.
const E2E_WORKSPACE: &str = "/workspace";

const LEGACY_SCRIPTS: [&str; 4] = [
    "run_e2e.sh",
    "run_e2e_ipv6.sh",
    "run_aggregation.sh",
    "run_perf.sh",
];

fn e2e(args: &[String]) -> Result<ExitCode, String> {
    let legacy = args.iter().any(|a| a == "--legacy");
    let case = flag_value(args, "--case")?;
    let script = flag_value(args, "--script")?;

    if !legacy {
        return match case {
            Some(case) => Err(format!(
                "case `{case}` is not runnable yet: the L3 driver arrives with the e2e harness \
                 (TEST-PLAN §6). Run the current bash suites with `cargo xtask e2e --legacy`."
            )),
            None => Err(
                "nothing selected: pass `--legacy` for the bash suites in verification/, \
                 or `--case <id>` once the L3 driver exists"
                    .into(),
            ),
        };
    }

    let workspace = Path::new(E2E_WORKSPACE);
    if !workspace.join("probe").is_dir() {
        return Err(format!(
            "the legacy suites hardcode `{E2E_WORKSPACE}/probe` and `{E2E_WORKSPACE}/verification`.\n\
             Run this command inside the dev container, where docker-compose mounts the \
             repository at `{E2E_WORKSPACE}`:\n  docker compose exec aya-dev cargo xtask e2e --legacy"
        ));
    }

    let mut ran = 0;
    for name in LEGACY_SCRIPTS {
        if let Some(filter) = &script
            && !name.contains(filter.as_str())
        {
            continue;
        }
        let path = workspace.join("verification").join(name);
        if !path.is_file() {
            return Err(format!("missing script {}", path.display()));
        }
        let mut command = Command::new("bash");
        command.arg(&path).current_dir(workspace);
        run(command, name)?;
        ran += 1;
    }
    if ran == 0 {
        return Err(format!(
            "no script matched `--script {}`",
            script.unwrap_or_default()
        ));
    }
    Ok(ExitCode::SUCCESS)
}

// --------------------------------------------------------------------- bpf

fn bpf(args: &[String]) -> Result<ExitCode, String> {
    let toolchain = flag_value(args, "--toolchain")?.unwrap_or_else(|| "nightly".into());
    // `cargo +<toolchain>` only works when cargo *is* the rustup shim. Inside a
    // cargo subcommand `CARGO` points at the concrete toolchain binary, so the
    // toolchain is selected through rustup explicitly.
    if !on_path("rustup") {
        return Err(format!(
            "rustup not found: `{toolchain}` is required for `-Z build-std=core` and the \
             `bpfel-unknown-none` target"
        ));
    }
    let mut command = Command::new("rustup");
    command
        .current_dir(workspace_root())
        .args(["run", &toolchain, "cargo"])
        .args(["build", "-p", "probe-ebpf"])
        .args(["--target", "bpfel-unknown-none"])
        .args(["-Z", "build-std=core"]);
    run(
        command,
        &format!("BPF object build with toolchain `{toolchain}`"),
    )?;
    Ok(ExitCode::SUCCESS)
}

// ------------------------------------------------------------------ report

fn report() -> Result<ExitCode, String> {
    let path = junit_path();
    let xml = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "read {}: {e}\nRun `cargo xtask test --layer all` first.",
            path.display()
        )
    })?;

    let cases = xml.matches("<testcase").count();
    let failures = xml.matches("<failure").count();
    let errors = xml.matches("<error").count();
    let skipped = xml.matches("<skipped").count();
    let passed = cases.saturating_sub(failures + errors + skipped);

    println!("xtask: {}", path.display());
    println!("  test cases: {cases}");
    println!("  passed:     {passed}");
    println!("  failed:     {failures}");
    println!("  errored:    {errors}");
    println!("  skipped:    {skipped}");

    for (name, status) in case_results(&xml) {
        if status != "pass" {
            println!("  {status:<7} {name}");
        }
    }

    if failures + errors > 0 {
        return Err(format!("{} test case(s) did not pass", failures + errors));
    }
    Ok(ExitCode::SUCCESS)
}

/// Flat `(name, status)` pairs from the JUnit XML. The report is written by
/// nextest, so the subset of JUnit this needs is small and stable.
fn case_results(xml: &str) -> Vec<(String, &'static str)> {
    let mut results = Vec::new();
    for case in xml.split("<testcase").skip(1) {
        let Some(name) = attribute(case, "name=\"").or_else(|| attribute(case, "classname=\""))
        else {
            continue;
        };
        let status = if case.contains("<failure") || case.contains("<error") {
            "fail"
        } else if case.contains("<skipped") {
            "skip"
        } else {
            "pass"
        };
        results.push((name, status));
    }
    results
}

/// Value of the first `key="value"` attribute in `fragment`.
fn attribute(fragment: &str, key: &str) -> Option<String> {
    let start = fragment.find(key)? + key.len();
    let rest = &fragment[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

// -------------------------------------------------------------------- soak

fn soak(args: &[String]) -> Result<ExitCode, String> {
    let minutes = flag_value(args, "--minutes")?.unwrap_or_else(|| "30".into());
    let minutes: u64 = minutes
        .parse()
        .map_err(|_| format!("`--minutes` expects a whole number, got `{minutes}`"))?;
    if minutes == 0 {
        return Err("`--minutes` must be greater than 0".into());
    }
    eprintln!(
        "xtask: soak for {minutes} minute(s) is not implemented yet: the L4 driver and its \
         nightly schedule land in stage S5. The command exists so the interface is stable."
    );
    Ok(ExitCode::from(3))
}

// ----------------------------------------------------------------- plumbing

fn workspace_root() -> PathBuf {
    // `CARGO_MANIFEST_DIR` is `<workspace root>/xtask`.
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .expect("xtask lives one level below the workspace root")
        .to_path_buf()
}

fn cargo() -> String {
    env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn cargo_command(args: &[String]) -> Command {
    let mut command = Command::new(cargo());
    command.current_dir(workspace_root()).args(args);
    command
}

/// Runs `command` from the workspace root and turns a non-zero exit into an
/// error that names the step, so CI output says which layer failed.
fn run(mut command: Command, label: &str) -> Result<(), String> {
    println!("xtask: {label}");
    let status = command
        .status()
        .map_err(|e| format!("{label}: cannot start cargo: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{label} failed ({status})"))
    }
}

fn flag_value(args: &[String], flag: &str) -> Result<Option<String>, String> {
    let with_equals = format!("{flag}=");
    let mut found = None;
    let mut arguments = args.iter();
    while let Some(arg) = arguments.next() {
        if arg == flag {
            let value = arguments
                .next()
                .ok_or_else(|| format!("`{flag}` needs a value"))?;
            found = Some(value.clone());
        } else if let Some(value) = arg.strip_prefix(&with_equals) {
            found = Some(value.to_owned());
        }
    }
    Ok(found)
}
