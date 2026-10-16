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
    initial_ns: Option<u64>,
    counts: Option<NativeCounts>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct NativeCounts {
    compilations_before: u64,
    compilations_after: u64,
    native_entries_before: u64,
    native_entries_after: u64,
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

#[derive(Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
struct Text {
    node_id: u64,
    parent_id: Option<String>,
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
    if engine != "ekojit" {
        bail!("native-page requires the ekojit engine");
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
    let result = block_on_with(
        &reactor,
        nagoya::timeout(std::time::Duration::from_secs(60), async {
            let mut command = Command::new(std::env::current_exe()?);
            command
                .arg("--supervise-native-host")
                .arg(host)
                .arg("--page")
                .arg(page)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .kill_on_drop(true);
            if let Some(plant) = plant {
                command.arg("--plant").arg(plant);
            }
            {
                use std::os::unix::process::CommandExt;
                command.as_std_mut().process_group(0);
            }
            let mut child = command.spawn(&handle)?;
            struct Group(u32);
            impl Drop for Group {
                fn drop(&mut self) {
                    crate::supervisor::kill_group(self.0);
                }
            }
            let _group = Group(child.id().ok_or_else(|| eyre!("native host has no pid"))?);
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
        }),
    );
    reactor.shutdown();
    let observation =
        result.map_err(|_| eyre!("native host exceeded its 60 second deadline"))??;
    if observation.schema != "eko-page-observation-v1" || observation.viewport.contains(&0) {
        bail!("invalid native page observation schema or viewport");
    }
    if observation.backend != engine {
        bail!("native page observation does not match the requested engine");
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
    let (native_entries, native_host_calls, native_code_bytes) = observation
        .native
        .iter()
        .try_fold((0_u64, 0_u64, 0_usize), |(entries, calls, bytes), entry| {
            Ok::<_, eyre::Report>((
                entries
                    .checked_add(entry.native_entries)
                    .ok_or_else(|| eyre!("native entry count overflow"))?,
                calls
                    .checked_add(entry.host_calls)
                    .ok_or_else(|| eyre!("native host call count overflow"))?,
                bytes
                    .checked_add(entry.code_bytes)
                    .ok_or_else(|| eyre!("native code byte count overflow"))?,
            ))
        })?;
    let report_value = json!({ "backend": observation.backend, "checks": rows,
        "failures": failures,
        "native_entries": native_entries,
        "native_host_calls": native_host_calls,
        "native_code_bytes": native_code_bytes,
        "initial_ns": observation.initial_ns,
        "counts": observation.counts,
        "texts": observation.texts,
        "native": observation.native.iter().map(|entry| json!({
            "source_url": &entry.source_url,
            "backend": &entry.backend,
            "entry_abi": &entry.entry_abi,
            "native_entries": entry.native_entries,
            "host_calls": entry.host_calls,
            "code_bytes": entry.code_bytes,
        })).collect::<Vec<_>>() });
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
