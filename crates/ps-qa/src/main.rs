//! `ps-qa` command-line entrypoint.

mod app;
mod audit;
mod capture_analysis;
mod child_output;
mod cli;
mod computed_style;
mod diagnostics;
mod gpu_buffer;
mod inspector;
mod interaction;
mod layout_report;
mod native_page;
mod outcome;
mod page_targets;
mod paint_audit;
mod paint_color;
mod plan;
mod qa;
mod reach;
mod report;
mod runner;
mod supervisor;
mod sweep;
mod target;
mod timing;

// nagoya, not tokio. The inspector connection is a nagoya socket now, and a
// tokio executor polling that future never sees the readiness the nagoya
// reactor owns: the request is written and the reply never arrives. One
// runtime drives the whole client.
#[path = "app_caps.rs"]
mod app_caps;

fn main() -> eyre::Result<()> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|arg| arg == "--supervise-host" || arg == "--supervise-native-host")
    {
        return supervisor::run();
    }
    if std::env::args_os().any(|argument| argument == "native-page") {
        let command = <cli::Cli as clap::Parser>::parse().command;
        if let cli::Command::NativePage {
            host,
            page,
            checks,
            engine,
            plant,
            report,
        } = command
        {
            return native_page::run(&host, &page, &checks, &engine, plant.as_deref(), &report);
        }
    }
    let caps = app_caps::start([2147483648, 600, 1800, 512, 8, 32, 16777216, 100000, 100000]);
    let _monitor = app_caps::monitor(caps);
    app_caps::stage("crates/ps-qa/src/main.rs: running");
    nagoya::block_on(runner::run())
}
