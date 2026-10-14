//! Application-only caps. Environment config preserves the original CLI.

#![allow(dead_code)]

use std::ffi::OsString;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use std::future::Future;

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
        .filter(|n| *n > 0 && *n < u64::MAX - 1)
        .unwrap_or_else(|| fail(format!("CAP_BAD_PARAMETER value={value:?}")))
}

/// Configure this binary with APP_CAP_MAX_<NAME> and APP_CAP_WARN_<NAME>.
/// Hyphens become underscores. No config means the legacy invocation is uncapped.
/// APP_CAP_PROFILE=default explicitly selects the defaults supplied by main.
pub fn start(defaults: [u64; 9]) -> Arc<Caps> {
    let profile = std::env::var_os("APP_CAP_PROFILE");
    if profile.as_deref().is_some_and(|s| s != "default") {
        fail("CAP_BAD_PARAMETER APP_CAP_PROFILE requires default");
    }
    let mut hard = [u64::MAX - 1; 9];
    let mut warn = [u64::MAX - 1; 9];
    for (i, name) in NAMES.iter().enumerate() {
        let key = name.replace('-', "_").to_uppercase();
        let maximum = std::env::var(format!("APP_CAP_MAX_{key}"));
        let warning = std::env::var(format!("APP_CAP_WARN_{key}"));
        if let Ok(value) = maximum {
            hard[i] = number(&value);
        } else if profile.is_some() {
            hard[i] = defaults[i];
        } else if warning.is_ok() {
            fail(format!("CAP_BAD_PARAMETER warn-{name} needs max-{name}"));
        }
        if hard[i] != u64::MAX - 1 {
            warn[i] = warning.map(|value| number(&value)).unwrap_or((hard[i] / 2).max(1));
            if warn[i] > hard[i] {
                fail(format!("CAP_BAD_PARAMETER warn-{name} exceeds max-{name}"));
            }
        }
    }
    let caps = Arc::new(Caps {
        hard,
        warn,
        warned: std::array::from_fn(|_| AtomicBool::new(false)),
        args: std::env::args_os().collect::<Vec<_>>().into(),
        stage: Mutex::new(Arc::from("startup")),
        started: Instant::now(),
    });
    STATE.set(Arc::clone(&caps)).unwrap_or_else(|_| fail("CAP_ALREADY_STARTED"));
    #[cfg(unix)]
    install_kernel_limits(&hard);
    caps.tick();
    caps
}

pub struct Monitor {
    stopping: Arc<blitz_control_protocol::latest::Flag>,
    task: Option<nagoya::JoinHandle<Arc<Caps>>>,
}

pub fn monitor(caps: Arc<Caps>) -> Option<Monitor> {
    if caps.hard[..6].iter().all(|value| *value == u64::MAX - 1) {
        return None;
    }
    let stopping = Arc::new(blitz_control_protocol::latest::Flag::new());
    let task = enqueue(Arc::clone(&caps), Arc::clone(&stopping));
    Some(Monitor { stopping, task: Some(task) })
}

fn enqueue(
    caps: Arc<Caps>,
    stopping: Arc<blitz_control_protocol::latest::Flag>,
) -> nagoya::JoinHandle<Arc<Caps>> {
    nagoya::spawn(async move {
        use std::task::Poll;

        let mut signal = std::pin::pin!(stopping.raised());
        let mut deadline = std::pin::pin!(nagoya::time::sleep(Duration::from_secs(1)));
        std::future::poll_fn(|cx| {
            if signal.as_mut().poll(cx).is_ready() {
                return Poll::Ready(());
            }
            if deadline.as_mut().poll(cx).is_ready() {
                caps.tick();
                deadline.set(nagoya::time::sleep(Duration::from_secs(1)));
                let _ = deadline.as_mut().poll(cx);
            }
            Poll::Pending
        })
        .await;
        caps
    })
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stopping.raise();
        if let Some(task) = self.task.take() {
            drop(nagoya::block_on(task));
        }
    }
}

/// Kernel backstops leave time for the timer to report CPU exhaustion by name.
#[cfg(unix)]
fn install_kernel_limits(hard: &[u64; 9]) {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    for (resource, requested, margin) in
        [(Resource::Nofile, hard[3], 0)]
    {
        if requested == u64::MAX - 1 { continue; }
        let inherited = getrlimit(resource);
        let soft = requested.saturating_add(margin);
        if inherited.maximum.is_some_and(|max| soft > max) {
            continue;
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
    current().tick();
}
pub fn hard(name: &str) -> u64 {
    current().hard[index(name)]
}
pub fn check(name: &str, value: u64) {
    current().tick();
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
        if self.hard[index] == u64::MAX - 1 { return; }
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
        if self.hard[..6].iter().all(|value| *value == u64::MAX - 1) { return; }
        let sample = sample(self.hard[3].min(65_536), self.hard[4].min(65_536))
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
fn sample(_max_files: u64, _max_procs: u64) -> std::io::Result<Sample> {
    use libproc::file_info::ListFDs;
    use libproc::proc_pid::{listpidinfo, pidinfo};
    use libproc::processes::{ProcFilter, pids_by_type};
    use libproc::task_info::TaskInfo;
    let pid = std::process::id();
    let info = pidinfo::<TaskInfo>(pid as i32, 0).map_err(std::io::Error::other)?;
    let descriptor_limit = rustix::process::getrlimit(rustix::process::Resource::Nofile).current.unwrap_or(65_536);
    let slots = descriptor_limit.min(current().hard[3]).saturating_add(1);
    let bytes = slots.saturating_mul(std::mem::size_of::<libproc::file_info::ProcFDInfo>() as u64);
    if bytes > i32::MAX as u64 { return Err(std::io::Error::other("descriptor census out of range")); }
    current().check(0, info.pti_resident_size.saturating_add(bytes));
    let files = listpidinfo::<ListFDs>(pid as i32, slots as usize)
        .map_err(std::io::Error::other)?
        .len() as u64;
    let procs = pids_by_type(ProcFilter::ByParentProcess { ppid: pid })?
        .into_iter()
        .filter(|p| *p > 0)
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
    read_limited(file, hard("input-bytes"))
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

/// Read into a fixed stack block, validate growth, then reserve exactly the accepted bytes.
/// The extra overflow byte stays on the stack and never enters the output allocation.
pub fn read_limited(mut reader: impl std::io::Read, maximum: u64) -> std::io::Result<Vec<u8>> {
    let maximum = maximum.min(hard("input-bytes"));
    let mut bytes = Vec::new();
    let mut block = [0u8; 8192];
    loop {
        let remaining = maximum.saturating_sub(bytes.len() as u64);
        let size = remaining.saturating_add(1).min(block.len() as u64) as usize;
        let count = match reader.read(&mut block[..size]) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 { return Ok(bytes); }
        let next = (bytes.len() as u64).saturating_add(count as u64);
        check("input-bytes", next);
        if next > maximum {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "input byte budget exceeded"));
        }
        reserve_bytes(count as u64);
        bytes.try_reserve_exact(count).map_err(std::io::Error::other)?;
        bytes.extend_from_slice(&block[..count]);
    }
}
pub fn read_stdin() -> std::io::Result<String> {
    String::from_utf8(read_limited(std::io::stdin().lock(), hard("input-bytes"))?)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
pub fn reserve_bytes(additional: u64) {
    if hard("rss") == u64::MAX - 1 { return; }
    let rss = sample(current().hard[3].min(65_536), current().hard[4].min(65_536))
        .unwrap_or_else(|e| fail(format!("CAP_METRICS_UNAVAILABLE {e}"))).rss;
    check("rss", rss.saturating_add(additional));
}

pub fn args() -> impl Iterator<Item = String> {
    args_os().map(|s| s.into_string().unwrap_or_else(|_| fail("CAP_NON_UTF8_ARGUMENT")))
}
