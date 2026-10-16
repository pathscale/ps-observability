//! A hosted process group whose lifetime follows the runner's stdin pipe.

use std::os::fd::AsRawFd;
use std::task::Poll;

pub(crate) fn kill_group(pid: u32) {
    #[cfg(unix)]
    if let Some(group) = rustix::process::Pid::from_raw(pid as i32) {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
}

pub(crate) fn run() -> eyre::Result<()> {
    use futures::future::{Either, select};
    use nagoya::process::{Command, Stdio};
    use nagoya::reactor::{Interest, Reactor, block_on_with};

    // The runner creates this process as the group leader. The host inherits
    // that group, so both the runner and this supervisor can stop descendants.
    // Fail closed before spawning if invoked outside that contract.
    if rustix::process::getpgrp() != rustix::process::getpid() {
        eyre::bail!("host supervisor must lead its process group");
    }
    struct Group;
    impl Drop for Group {
        fn drop(&mut self) {
            kill_group(std::process::id());
        }
    }
    let _group = Group;
    let mut arguments = std::env::args_os().skip(1);
    let native =
        arguments.next().as_deref() == Some(std::ffi::OsStr::new("--supervise-native-host"));
    let host = arguments
        .next()
        .ok_or_else(|| eyre::eyre!("host supervisor requires an executable"))?;
    let reactor = Reactor::local().map_err(|error| eyre::eyre!("{error:?}"))?;
    let handle = reactor.handle();
    let stdin = std::io::stdin();
    let flags = rustix::fs::fcntl_getfl(&stdin)?;
    rustix::fs::fcntl_setfl(&stdin, flags | rustix::fs::OFlags::NONBLOCK)?;
    let registration = handle
        .register(stdin.as_raw_fd(), Interest::READABLE)
        .map_err(|error| eyre::eyre!("{error:?}"))?;
    block_on_with(&reactor, async {
        let mut command = Command::new(host);
        command
            .args(arguments)
            .stdin(Stdio::null())
            .kill_on_drop(true);
        let mut child = command.spawn(&handle)?;
        let parent_closed = std::future::poll_fn(|cx| {
            if !registration.take_readable_or_park(cx.waker()) {
                return Poll::Pending;
            }
            let mut buffer = [0; 64];
            loop {
                match rustix::io::read(&stdin, &mut buffer) {
                    Ok(0) => return Poll::Ready(()),
                    Ok(_) => continue,
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(rustix::io::Errno::AGAIN) => {
                        registration.poll_readable(cx.waker());
                        return Poll::Pending;
                    }
                    Err(_) => return Poll::Ready(()),
                }
            }
        });
        futures::pin_mut!(parent_closed);
        let lost_parent = match select(parent_closed, Box::pin(child.wait())).await {
            Either::Left(((), waiting)) => {
                drop(waiting);
                true
            }
            Either::Right((status, _)) => {
                if native {
                    // The caller needs the native host's exit status. Its
                    // group guard removes helpers after reading the outcome.
                    std::process::exit(status?.code().unwrap_or(1));
                }
                false
            }
        };
        if lost_parent {
            child.start_kill()?;
            child.wait().await?;
        }
        Ok::<_, eyre::Report>(())
    })?;
    // Group's destructor also removes helpers after an ordinary host exit.
    Ok(())
}
