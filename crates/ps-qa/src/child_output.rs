//! Child stdout driven by nagoya readiness for the host's entire lifetime.

use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::ChildStdout;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};

use blitz_control_protocol::latest::{Flag, Once};
use futures::future::{Either, select};
use futures::io::{AsyncBufReadExt, AsyncRead, BufReader};
use nagoya::reactor::{Handle, Interest, Reactor, Registration};

// Deregister before closing stdout, so fd reuse cannot inherit this interest.
struct Pipe {
    registration: Registration,
    stdout: ChildStdout,
}

impl Pipe {
    fn new(stdout: ChildStdout, handle: &Handle) -> io::Result<Self> {
        let flags = rustix::fs::fcntl_getfl(&stdout)?;
        rustix::fs::fcntl_setfl(&stdout, flags | rustix::fs::OFlags::NONBLOCK)?;
        let registration = handle
            .register(stdout.as_raw_fd(), Interest::READABLE)
            .map_err(|error| io::Error::other(format!("host stdout registration: {error:?}")))?;
        Ok(Self { registration, stdout })
    }
}

impl AsyncRead for Pipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        if buffer.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let pipe = self.get_mut();
        if !pipe.registration.take_readable_or_park(cx.waker()) {
            return Poll::Pending;
        }
        loop {
            match pipe.stdout.read(buffer) {
                Ok(count) => {
                    // A short pipe read does not prove it empty. Keep reading
                    // until EAGAIN; that is the edge-triggered rearm boundary.
                    if count > 0 {
                        pipe.registration.mark_readable();
                    }
                    return Poll::Ready(Ok(count));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    pipe.registration.poll_readable(cx.waker());
                    return Poll::Pending;
                }
                Err(error) => return Poll::Ready(Err(error)),
            }
        }
    }
}

struct OutputWork {
    pipe: Mutex<Option<Pipe>>,
    descriptor: Arc<Once<Arc<PathBuf>>>,
    stopping: Arc<Flag>,
    suppress: bool,
}

pub(crate) struct HostOutput {
    work: Arc<OutputWork>,
    task: Option<nagoya::JoinHandle<Arc<OutputWork>>>,
}

fn reactor() -> io::Result<Handle> {
    static REACTOR: OnceLock<io::Result<Reactor>> = OnceLock::new();
    REACTOR
        .get_or_init(|| {
            Reactor::start().map_err(|error| io::Error::other(format!("host stdout reactor: {error:?}")))
        })
        .as_ref()
        .map(Reactor::handle)
        .map_err(|error| io::Error::new(error.kind(), error.to_string()))
}

impl HostOutput {
    pub(crate) fn start(stdout: ChildStdout, suppress: bool) -> io::Result<Self> {
        let work = Arc::new(OutputWork {
            pipe: Mutex::new(Some(Pipe::new(stdout, &reactor()?)?)),
            descriptor: Once::new(),
            stopping: Flag::new(),
            suppress,
        });
        let task = enqueue(Arc::clone(&work));
        Ok(Self { work, task: Some(task) })
    }

    pub(crate) async fn descriptor(&self) -> Option<Arc<PathBuf>> {
        self.work.descriptor.recv().await
    }
}

impl Drop for HostOutput {
    fn drop(&mut self) {
        self.work.stopping.raise();
        if let Some(task) = self.task.take() {
            drop(nagoya::block_on(task));
        }
    }
}

fn enqueue(work: Arc<OutputWork>) -> nagoya::JoinHandle<Arc<OutputWork>> {
    nagoya::spawn(drain(work))
}

async fn drain(work: Arc<OutputWork>) -> Arc<OutputWork> {
    let pipe = work.pipe.lock().unwrap_or_else(|error| error.into_inner()).take();
    if let Some(pipe) = pipe {
        let mut reader = BufReader::new(pipe);
        let mut line = String::new();
        let mut announced = false;

        /*
         * The descriptor is the first line that looks like one, not the first
         * line.
         *
         * The page shares this pipe. Anything it writes to the console arrives
         * on the host's stdout, and taking line one unconditionally meant a
         * single `console.log` during startup was read as a path -- reported as
         * "the host never announced a descriptor", which points at the host and
         * at the component, and is wrong about both.
         *
         * Found on @pathscale/ui's ThemeColorPicker, which logs once about CSP
         * blocking inline styles. It had never logged before because the code
         * that logs was unreachable without `localStorage`, so a harness change
         * unrelated to it made a component that works look like a component
         * that will not start. Every component that logs anything at startup
         * was one line away from the same thing.
         *
         * A descriptor is a `.json` path the host has already created by the
         * time it prints it, so both halves are checkable. Everything else is
         * forwarded as the diagnostic it is.
         */
        loop {
            let count = {
                let stopping = work.stopping.raised();
                let reading = reader.read_line(&mut line);
                futures::pin_mut!(stopping, reading);
                match select(stopping, reading).await {
                    Either::Left(_) => break,
                    Either::Right((Ok(count), _)) => count,
                    Either::Right((Err(_), _)) => break,
                }
            };
            if count == 0 {
                break;
            }
            let text = line.trim();
            if !announced && text.ends_with(".json") && std::path::Path::new(text).is_file() {
                announced = true;
                work.descriptor.send(Arc::new(PathBuf::from(text))).await;
            } else if !work.suppress && !text.is_empty() {
                eprintln!("host: {text}");
            }
            line.clear();
            nagoya::yield_now().await;
        }
        // Keep consuming stdout for the lifetime of the host. Closing the
        // pipe after the descriptor line would deliver EPIPE if a host emits
        // later diagnostics and could terminate an otherwise healthy sweep.
    }
    work.descriptor.abandon().await;
    work
}
