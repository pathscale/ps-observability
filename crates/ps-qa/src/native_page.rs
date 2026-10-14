//! Functional page checks against a real linked host's immutable observation.
//! This command adds no renderer dependency to ps-qa.

use std::path::Path;
use std::sync::Arc;

use eyre::{Result, bail, eyre};
use futures::io::AsyncReadExt;
use nagoya::process::{Command, Stdio};
use nagoya::reactor::{Reactor, block_on_with};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Observation {
    schema: String,
    backend: String,
    title: String,
    console: Vec<String>,
    errors: Vec<String>,
    native: Vec<NativeEntry>,
    texts: Vec<Text>,
    viewport: [u32; 2],
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeEntry {
    source_url: String,
    backend: String,
    entry_abi: String,
    native_entries: u64,
    host_calls: u64,
    code_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Text {
    node_id: u64,
    text: String,
    bounds: Option<[f64; 4]>,
    ink_pixels: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    id: String,
    expect: Expect,
}

#[derive(Deserialize)]
enum Expect {
    NativeEntry,
    Title(String),
    Console(String),
    VisibleText(String),
}

pub fn run(
    host: &Path,
    page: &Path,
    checks: &Path,
    engine: &str,
    plant: Option<&str>,
    report: &Path,
) -> Result<()> {
    if !matches!(engine, "boa" | "ekojit") {
        bail!("engine must be boa or ekojit");
    }
    if plant.is_some_and(|name| name != "host-text-append-noop") {
        bail!("unknown native-page plant");
    }
    if std::fs::metadata(checks)?.len() > 16_384 {
        bail!("native-page checks exceed 16 KiB");
    }
    let checks: Vec<Check> = ron::from_str(&std::fs::read_to_string(checks)?)?;
    if checks.is_empty() || checks.len() > 32 {
        bail!("native-page requires 1 to 32 checks");
    }
    let mut names = std::collections::HashSet::new();
    for check in &checks {
        if check.id.is_empty() || !names.insert(&check.id) {
            bail!("invalid or duplicate native-page check id");
        }
    }
    let reactor = Reactor::local().map_err(|error| eyre!(error.to_string()))?;
    let handle = reactor.handle();
    let result = block_on_with(&reactor, async {
        let mut command = Command::new(host);
        command
            .arg("--page")
            .arg(page)
            .arg("--engine")
            .arg(engine)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        if let Some(plant) = plant {
            command.arg("--plant").arg(plant);
        }
        let mut child = command.spawn(&handle)?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| eyre!("native host has no stdout pipe"))?;
        let mut bytes = Vec::new();
        stdout.take(65_537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65_536 {
            bail!("native host observation exceeds 64 KiB");
        }
        let status = child.wait().await?;
        if !status.success() {
            bail!("native host failed before producing an outcome: {status}");
        }
        let observation: Observation = serde_json::from_slice(&bytes)?;
        Ok::<_, eyre::Report>(Arc::new(observation))
    });
    reactor.shutdown();
    let observation = result?;
    if observation.schema != "eko-page-observation-v1" || observation.viewport.contains(&0) {
        bail!("invalid native page observation schema or viewport");
    }
    if !observation.errors.is_empty() {
        bail!("native page startup errors: {:?}", observation.errors);
    }
    let mut rows = Vec::new();
    let mut failures = 0;
    for check in checks {
        let passed = match &check.expect {
            Expect::NativeEntry => {
                observation.backend == "ekojit"
                    && !observation.native.is_empty()
                    && observation.native.iter().all(|entry| {
                        !entry.source_url.is_empty()
                            && entry.backend == "ekojit/ps-codegen"
                            && entry.entry_abi == "eko-page-host-v1"
                            && entry.native_entries == 1
                            && entry.host_calls > 0
                            && entry.code_bytes >= 20
                    })
            }
            Expect::Title(title) => observation.title == *title,
            Expect::Console(message) => observation.console.iter().any(|entry| entry == message),
            Expect::VisibleText(text) => observation.texts.iter().any(|node| {
                node.text == *text
                    && node.node_id > 0
                    && node.ink_pixels > 0
                    && node.bounds.is_some_and(|[x, y, width, height]| {
                        [x, y, width, height].iter().all(|value| value.is_finite())
                            && width > 0.0
                            && height > 0.0
                            && x + width > 0.0
                            && y + height > 0.0
                            && x < f64::from(observation.viewport[0])
                            && y < f64::from(observation.viewport[1])
                    })
            }),
        };
        failures += usize::from(!passed);
        rows.push(json!({ "id": check.id, "verdict": if passed { "pass" } else { "fail" } }));
    }
    let report_value = json!({ "backend": observation.backend, "checks": rows,
        "failures": failures, "native_entries": observation.native.iter().map(|entry| entry.native_entries).sum::<u64>() });
    blitz_control_protocol::guarded_writer::write_file(
        std::path::Path::new(&report),
        &serde_json::to_vec_pretty(&report_value)?,
    )?;
    println!(
        "{}",
        toon_format::encode_default(&report_value).map_err(|error| eyre!(error.to_string()))?
    );
    if failures != 0 {
        bail!("native page failed {failures} named check(s)");
    }
    Ok(())
}
