//! Application resource caps, compiled into each binary (never a library).
//!
//! Every cap is a parameter: `--max-<name>` / `--warn-<name>` on the command line,
//! stripped before the application parses its own arguments. Defaults come from the
//! binary's `start` call. Resident size is measured, never virtual. Each cap warns
//! once at its warn threshold (default 50%) naming the current stage, and the process
//! exits naming the cap when the hard value is passed. No `unsafe` here: rustix and
//! libproc provide the system calls.

// Each binary uses a different subset of these helpers.
#![allow(dead_code)]

use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

const NAMES: [&str; 9] = [
    "rss",
    "cpu-secs",
    "wall-secs",
    "open-files",
    "procs",
    "threads",
    "input-bytes",
    "iterations",
    "cache-entries",
];
static TURNS: AtomicU64 = AtomicU64::new(0);
static OWNED: Mutex<Vec<u32>> = Mutex::new(Vec::new());
static STATE: OnceLock<Arc<Caps>> = OnceLock::new();

pub struct Caps {
    hard: [u64; 9],
    warn: [u64; 9],
    warned: [AtomicBool; 9],
    args: Arc<[OsString]>,
    stage: Mutex<Arc<str>>,
    started: Instant,
}

fn fail(message: impl std::fmt::Display) -> ! {
    eprintln!("CAP_ERROR {message}");
    std::process::exit(78);
}

fn number(value: &str) -> u64 {
    let (digits, scale) = match value.as_bytes().last().copied() {
        Some(b'K' | b'k') => (&value[..value.len() - 1], 1u64 << 10),
        Some(b'M' | b'm') => (&value[..value.len() - 1], 1u64 << 20),
        Some(b'G' | b'g') => (&value[..value.len() - 1], 1u64 << 30),
        _ => (value, 1),
    };
    digits
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .filter(|n| *n > 0 && *n < u64::MAX)
        .unwrap_or_else(|| fail(format!("CAP_BAD_PARAMETER value={value:?}")))
}

pub fn start(defaults: [u64; 9]) -> Arc<Caps> {
    let mut hard = defaults;
    let mut supplied = [None; 9];
    let mut args = Vec::new();
    let mut raw = std::env::args_os();
    args.push(raw.next().unwrap_or_default());
    let mut positional = false;
    let mut help = false;
    while let Some(arg) = raw.next() {
        if positional {
            args.push(arg);
            continue;
        }
        if arg == "--" {
            positional = true;
            args.push(arg);
            continue;
        }
        let text = arg.to_string_lossy();
        if text == "--caps-help" {
            help = true;
            continue;
        }
        let (flag, inline) = text
            .split_once('=')
            .map_or((text.as_ref(), None), |(k, v)| (k, Some(v)));
        let cap = flag
            .strip_prefix("--max-")
            .map(|s| (s, false))
            .or_else(|| flag.strip_prefix("--warn-").map(|s| (s, true)));
        let matched = cap.and_then(|(name, warning)| {
            NAMES
                .iter()
                .position(|n| *n == name)
                .map(|index| (index, warning))
        });
        if let Some((index, warning)) = matched {
            let value = inline.map(str::to_owned).unwrap_or_else(|| {
                raw.next()
                    .and_then(|s| s.into_string().ok())
                    .unwrap_or_else(|| fail(format!("CAP_BAD_PARAMETER flag={flag}")))
            });
            if warning {
                supplied[index] = Some(number(&value));
            } else {
                hard[index] = number(&value);
            }
            continue;
        }
        args.push(arg);
    }
    let warn = std::array::from_fn(|i| supplied[i].unwrap_or((hard[i] / 2).max(1)));
    for i in 0..9 {
        if warn[i] > hard[i] {
            fail(format!(
                "CAP_BAD_PARAMETER warn-{}={} max={}",
                NAMES[i], warn[i], hard[i]
            ));
        }
        if help {
            println!(
                "--max-{} {} --warn-{} {}",
                NAMES[i], hard[i], NAMES[i], warn[i]
            );
        }
    }
    if help {
        std::process::exit(0);
    }
    let caps = Arc::new(Caps {
        hard,
        warn,
        warned: std::array::from_fn(|_| AtomicBool::new(false)),
        args: args.into(),
        stage: Mutex::new(Arc::from("startup")),
        started: Instant::now(),
    });
    STATE
        .set(Arc::clone(&caps))
        .unwrap_or_else(|_| fail("CAP_ALREADY_STARTED"));
    caps.tick();
    #[cfg(unix)]
    install_kernel_limits(&hard);
    let monitor = Arc::clone(&caps);
    let runtime = nagoya::runtime::Runtime::new(1);
    drop(runtime.spawn(async move {
        loop {
            nagoya::sleep(Duration::from_millis(250)).await;
            monitor.tick();
        }
    }));
    caps
}

/// Kernel backstops. The 250 ms sampler names the CPU cap first; the kernel
/// limit sits two seconds above it and kills the process if the sampler cannot.
#[cfg(unix)]
fn install_kernel_limits(hard: &[u64; 9]) {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    for (resource, requested, margin) in
        [(Resource::Cpu, hard[1], 2), (Resource::Nofile, hard[3], 0)]
    {
        let inherited = getrlimit(resource);
        let soft = requested.saturating_add(margin);
        if inherited.maximum.is_some_and(|max| soft > max) {
            fail("CAP_INHERITED_HARD_LIMIT");
        }
        let maximum = if margin > 0 {
            Some(soft.saturating_add(1))
                .map(|limit| inherited.maximum.map_or(limit, |max| limit.min(max)))
        } else {
            inherited.maximum
        };
        setrlimit(
            resource,
            Rlimit {
                current: Some(soft),
                maximum,
            },
        )
        .unwrap_or_else(|e| fail(format!("CAP_KERNEL_LIMIT {e}")));
    }
}

#[cfg(not(test))]
pub fn current() -> &'static Arc<Caps> {
    STATE.get().unwrap_or_else(|| fail("CAP_NOT_STARTED"))
}
// Unit tests never run main(), so they get an uncapped instance instead of an exit.
#[cfg(test)]
pub fn current() -> &'static Arc<Caps> {
    STATE.get_or_init(|| {
        Arc::new(Caps {
            hard: [u64::MAX - 1; 9],
            warn: [u64::MAX - 1; 9],
            warned: Default::default(),
            args: Arc::from(Vec::new()),
            stage: Mutex::new(Arc::from("test")),
            started: Instant::now(),
        })
    })
}
pub fn args_os() -> impl Iterator<Item = OsString> {
    current().args.iter().cloned()
}
pub fn stage(value: impl Into<Arc<str>>) {
    *current()
        .stage
        .lock()
        .unwrap_or_else(|_| fail("CAP_STAGE_LOCK")) = value.into();
}
pub fn hard(name: &str) -> u64 {
    current().hard[index(name)]
}
pub fn check(name: &str, value: u64) {
    current().check(index(name), value);
}
fn index(name: &str) -> usize {
    NAMES
        .iter()
        .position(|n| *n == name)
        .unwrap_or_else(|| fail("CAP_UNKNOWN_BUDGET"))
}

impl Caps {
    fn check(&self, index: usize, value: u64) {
        if value >= self.warn[index] && !self.warned[index].swap(true, Ordering::Relaxed) {
            let stage = self.stage.lock().unwrap_or_else(|_| fail("CAP_STAGE_LOCK"));
            eprintln!(
                "CAP_WARN_{} current={} threshold={} stage={stage:?}",
                NAMES[index].replace('-', "_").to_uppercase(),
                value,
                self.warn[index]
            );
        }
        if value > self.hard[index] {
            let stage = self.stage.lock().unwrap_or_else(|_| fail("CAP_STAGE_LOCK"));
            stop_owned();
            fail(format!(
                "CAP_{}_EXCEEDED current={} threshold={} stage={stage:?}",
                NAMES[index].replace('-', "_").to_uppercase(),
                value,
                self.hard[index]
            ));
        }
    }
    fn tick(&self) {
        let sample = sample(self.hard[3], self.hard[4])
            .unwrap_or_else(|e| fail(format!("CAP_METRICS_UNAVAILABLE {e}")));
        self.check(0, sample.rss);
        self.check(2, self.started.elapsed().as_secs());
        self.check(3, sample.files);
        self.check(4, sample.procs);
        self.check(5, sample.threads);
        #[cfg(unix)]
        self.check(
            1,
            rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime).tv_sec as u64,
        );
    }
}

struct Sample {
    rss: u64,
    threads: u64,
    files: u64,
    procs: u64,
}

#[cfg(target_os = "macos")]
fn sample(max_files: u64, max_procs: u64) -> std::io::Result<Sample> {
    use libproc::file_info::ListFDs;
    use libproc::proc_pid::{listpidinfo, pidinfo};
    use libproc::processes::{ProcFilter, pids_by_type};
    use libproc::task_info::TaskInfo;
    let pid = std::process::id();
    let info = pidinfo::<TaskInfo>(pid as i32, 0).map_err(std::io::Error::other)?;
    // One slot past the cap so an overflow is still observed.
    let files = listpidinfo::<ListFDs>(pid as i32, max_files.saturating_add(1) as usize)
        .map_err(std::io::Error::other)?
        .len() as u64;
    let procs = pids_by_type(ProcFilter::ByParentProcess { ppid: pid })?
        .into_iter()
        .filter(|p| *p > 0)
        .take(max_procs.saturating_add(1) as usize)
        .count() as u64;
    Ok(Sample {
        rss: info.pti_resident_size,
        threads: info.pti_threadnum as u64,
        files,
        procs,
    })
}

#[cfg(target_os = "linux")]
fn sample(_max_files: u64, _max_procs: u64) -> std::io::Result<Sample> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let field = |key: &str| -> std::io::Result<u64> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| std::io::Error::other("missing process metric"))
    };
    let mut children = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir("/proc/self/task")? {
        let path = entry?.path().join("children");
        for pid in std::fs::read_to_string(path)?.split_whitespace() {
            children.insert(pid.to_owned());
        }
    }
    Ok(Sample {
        rss: field("VmRSS:")? * 1024,
        threads: field("Threads:")?,
        files: std::fs::read_dir("/proc/self/fd")?
            .count()
            .saturating_sub(1) as u64,
        procs: children.len() as u64,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample(_max_files: u64, _max_procs: u64) -> std::io::Result<Sample> {
    Err(std::io::Error::other(
        "application resident caps require macOS or Linux",
    ))
}

pub fn read_bytes(path: impl AsRef<Path>) -> std::io::Result<Vec<u8>> {
    let path = path.as_ref();
    stage(Arc::<str>::from(format!("read {}", path.display())));
    let file = std::fs::File::open(path)?;
    check("input-bytes", file.metadata()?.len());
    let mut bytes = Vec::new();
    file.take(hard("input-bytes") + 1).read_to_end(&mut bytes)?;
    check("input-bytes", bytes.len() as u64);
    Ok(bytes)
}
pub fn read_string(path: impl AsRef<Path>) -> std::io::Result<String> {
    String::from_utf8(read_bytes(path)?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

pub fn turn(value: impl Into<Arc<str>>) {
    stage(value);
    check(
        "iterations",
        TURNS.fetch_add(1, Ordering::Relaxed).saturating_add(1),
    );
}
pub fn child(pid: u32) {
    OWNED.lock().unwrap_or_else(|e| e.into_inner()).push(pid);
}
pub fn reaped(pid: u32) {
    OWNED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|p| *p != pid);
}
fn stop_owned() {
    #[cfg(unix)]
    for pid in OWNED.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        // Registered children lead their own process group.
        if let Some(group) = rustix::process::Pid::from_raw(*pid as i32) {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}
