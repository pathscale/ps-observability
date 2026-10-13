use std::ffi::OsString;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const NAMES: [&str; 9] = ["rss", "cpu-secs", "wall-secs", "open-files", "procs", "threads", "input-bytes", "iterations", "cache-entries"];
static CPU_HIT: AtomicBool = AtomicBool::new(false);
static TURNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static OWNED: Mutex<Vec<i32>> = Mutex::new(Vec::new());
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
        Some(b'K' | b'k') => (&value[..value.len()-1], 1u64 << 10),
        Some(b'M' | b'm') => (&value[..value.len()-1], 1u64 << 20),
        Some(b'G' | b'g') => (&value[..value.len()-1], 1u64 << 30),
        _ => (value, 1),
    };
    digits.parse::<u64>().ok().and_then(|n| n.checked_mul(scale))
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
        if positional { args.push(arg); continue; }
        if arg == "--" { positional = true; args.push(arg); continue; }
        let text = arg.to_string_lossy();
        if text == "--caps-help" { help = true; continue; }
        let (flag, inline) = text.split_once('=').map_or((text.as_ref(), None), |(k,v)| (k,Some(v)));
        let cap = flag.strip_prefix("--max-").map(|s| (s,false))
            .or_else(|| flag.strip_prefix("--warn-").map(|s| (s,true)));
        if let Some((name, warning)) = cap {
            if let Some(index) = NAMES.iter().position(|n| *n == name) {
                let value = inline.map(str::to_owned).unwrap_or_else(|| {
                    raw.next().and_then(|s| s.into_string().ok())
                        .unwrap_or_else(|| fail(format!("CAP_BAD_PARAMETER flag={flag}")))
                });
                if warning { supplied[index] = Some(number(&value)); }
                else { hard[index] = number(&value); }
                continue;
            }
        }
        args.push(arg);
    }
    let warn = std::array::from_fn(|i| supplied[i].unwrap_or((hard[i] / 2).max(1)));
    for i in 0..9 {
        if warn[i] > hard[i] { fail(format!("CAP_BAD_PARAMETER warn-{}={} max={}", NAMES[i], warn[i], hard[i])); }
        if help { println!("--max-{} {} --warn-{} {}", NAMES[i], hard[i], NAMES[i], warn[i]); }
    }
    if help { std::process::exit(0); }
    let caps = Arc::new(Caps {
        hard, warn, warned: std::array::from_fn(|_| AtomicBool::new(false)),
        args: args.into(), stage: Mutex::new(Arc::from("startup")), started: Instant::now(),
    });
    STATE.set(Arc::clone(&caps)).unwrap_or_else(|_| fail("CAP_ALREADY_STARTED"));
    caps.tick();
    #[cfg(unix)]
    unsafe {
        let mut inherited: libc::rlimit = std::mem::zeroed();
        for (resource, requested) in [(libc::RLIMIT_CPU, hard[1]), (libc::RLIMIT_NOFILE, hard[3])] {
            if libc::getrlimit(resource, &mut inherited) != 0 { fail(std::io::Error::last_os_error()); }
            let soft = requested as libc::rlim_t;
            if soft > inherited.rlim_max { fail("CAP_INHERITED_HARD_LIMIT"); }
            let hard = if resource == libc::RLIMIT_CPU { soft.saturating_add(1).min(inherited.rlim_max) } else { inherited.rlim_max };
            let limit = libc::rlimit { rlim_cur: soft, rlim_max: hard };
            if libc::setrlimit(resource, &limit) != 0 { fail(std::io::Error::last_os_error()); }
        }
        if libc::signal(libc::SIGXCPU, cpu_stop as *const () as libc::sighandler_t) == libc::SIG_ERR {
            fail("CAP_CPU_SIGNAL_SETUP");
        }
    }
    let monitor = Arc::clone(&caps);
    let runtime = nagoya::runtime::Runtime::new(1);
    drop(runtime.spawn(async move {
        loop { nagoya::sleep(Duration::from_millis(250)).await; monitor.tick(); }
    }));
    caps
}

#[cfg(unix)]
extern "C" fn cpu_stop(_: libc::c_int) {
    const MESSAGE: &[u8] = b"CAP_MAX_CPU_SECS: source=RLIMIT_CPU\n";
    unsafe { libc::write(2, MESSAGE.as_ptr().cast(), MESSAGE.len()); }
    CPU_HIT.store(true, Ordering::Relaxed);
}

pub fn current() -> &'static Arc<Caps> { STATE.get().unwrap_or_else(|| fail("CAP_NOT_STARTED")) }
pub fn args_os() -> std::vec::IntoIter<OsString> { current().args.to_vec().into_iter() }
pub fn args() -> std::vec::IntoIter<String> {
    args_os().map(|s| s.into_string().unwrap_or_else(|_| fail("CAP_NON_UTF8_ARGUMENT"))).collect::<Vec<_>>().into_iter()
}
pub fn stage(value: impl Into<Arc<str>>) {
    *current().stage.lock().unwrap_or_else(|_| fail("CAP_STAGE_LOCK")) = value.into();
}
pub fn hard(name: &str) -> u64 {
    current().hard[NAMES.iter().position(|n| *n == name).unwrap_or_else(|| fail("CAP_UNKNOWN_BUDGET"))]
}
pub fn check(name: &str, value: u64) {
    current().check(NAMES.iter().position(|n| *n == name).unwrap_or_else(|| fail("CAP_UNKNOWN_BUDGET")), value);
}

impl Caps {
    fn check(&self, index: usize, value: u64) {
        if value >= self.warn[index] && !self.warned[index].swap(true, Ordering::Relaxed) {
            let stage = self.stage.lock().unwrap_or_else(|_| fail("CAP_STAGE_LOCK"));
            eprintln!("CAP_WARN_{} current={} threshold={} stage={stage:?}", NAMES[index].replace('-', "_").to_uppercase(), value, self.warn[index]);
        }
        if value > self.hard[index] || (index == 1 && CPU_HIT.load(Ordering::Relaxed)) {
            let stage = self.stage.lock().unwrap_or_else(|_| fail("CAP_STAGE_LOCK"));
            stop_owned();
            fail(format!("CAP_{}_EXCEEDED current={} threshold={} stage={stage:?}", NAMES[index].replace('-', "_").to_uppercase(), value, self.hard[index]));
        }
    }
    fn tick(&self) {
        let (rss, threads, files, procs) = sample().unwrap_or_else(|e| fail(format!("CAP_METRICS_UNAVAILABLE {e}")));
        self.check(0, rss);
        self.check(2, self.started.elapsed().as_secs());
        self.check(3, files);
        self.check(4, procs);
        self.check(5, threads);
        #[cfg(unix)]
        unsafe {
            let mut usage: libc::rusage = std::mem::zeroed();
            if libc::getrusage(libc::RUSAGE_SELF, &mut usage) != 0 { fail("CAP_CPU_METRICS_UNAVAILABLE"); }
            self.check(1, (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as u64);
        }
    }
}

#[cfg(target_os = "macos")]
fn sample() -> std::io::Result<(u64,u64,u64,u64)> {
    unsafe {
        let mut info: libc::proc_taskinfo = std::mem::zeroed();
        let size = std::mem::size_of_val(&info) as libc::c_int;
        let pid = libc::getpid();
        if libc::proc_pidinfo(pid, libc::PROC_PIDTASKINFO, 0, (&mut info as *mut libc::proc_taskinfo).cast(), size) != size {
            return Err(std::io::Error::last_os_error());
        }
        let fds = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0);
        if fds <= 0 { return Err(std::io::Error::last_os_error()); }
        let mut slots: Vec<libc::proc_fdinfo> = Vec::with_capacity(fds as usize / std::mem::size_of::<libc::proc_fdinfo>() + 16);
        let fds = libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, slots.as_mut_ptr().cast(), (slots.capacity() * std::mem::size_of::<libc::proc_fdinfo>()) as i32);
        if fds <= 0 { return Err(std::io::Error::last_os_error()); }
        // A bounded child buffer includes one overflow slot.
        let capacity = current().hard[4].min(65_536) as usize + 1;
        let mut children = vec![0 as libc::pid_t; capacity];
        let count = libc::proc_listchildpids(pid, children.as_mut_ptr().cast(), (children.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int);
        if count < 0 { return Err(std::io::Error::last_os_error()); }
        Ok((info.pti_resident_size, info.pti_threadnum as u64,
            fds as u64 / std::mem::size_of::<libc::proc_fdinfo>() as u64,
            children[..(count as usize / std::mem::size_of::<libc::pid_t>()).min(children.len())].iter().filter(|p| **p > 0).count() as u64))
    }
}

#[cfg(target_os = "linux")]
fn sample() -> std::io::Result<(u64,u64,u64,u64)> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    let field = |key: &str| -> std::io::Result<u64> {
        status.lines().find_map(|l| l.strip_prefix(key)).and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok()).ok_or_else(|| std::io::Error::other("missing process metric"))
    };
    let mut children = std::collections::BTreeSet::new();
    for entry in std::fs::read_dir("/proc/self/task")? {
        let path = entry?.path().join("children");
        for pid in std::fs::read_to_string(path)?.split_whitespace() { children.insert(pid.to_owned()); }
    }
    Ok((field("VmRSS:")? * 1024, field("Threads:")?, std::fs::read_dir("/proc/self/fd")?.count().saturating_sub(1) as u64, children.len() as u64))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample() -> std::io::Result<(u64,u64,u64,u64)> {
    Err(std::io::Error::other("application resident caps require macOS or Linux"))
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
    String::from_utf8(read_bytes(path)?).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}
pub fn read_stdin() -> std::io::Result<String> {
    stage("read stdin");
    let mut bytes = Vec::new();
    std::io::stdin().take(hard("input-bytes") + 1).read_to_end(&mut bytes)?;
    check("input-bytes", bytes.len() as u64);
    String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

pub fn turn(value: impl Into<Arc<str>>) { stage(value); check("iterations", TURNS.fetch_add(1, Ordering::Relaxed).saturating_add(1)); }
pub fn child(pid: u32) { OWNED.lock().unwrap_or_else(|e| e.into_inner()).push(pid as i32); }
pub fn reaped(pid: u32) { OWNED.lock().unwrap_or_else(|e| e.into_inner()).retain(|p| *p != pid as i32); }
fn stop_owned() {
    #[cfg(unix)]
    for pid in OWNED.lock().unwrap_or_else(|e| e.into_inner()).iter() { unsafe { libc::kill(-*pid, libc::SIGKILL); } }
}
