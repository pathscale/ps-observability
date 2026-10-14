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
mod outcome;
mod page_targets;
mod paint_audit;
mod paint_color;
mod plan;
mod qa;
mod reach;
mod report;
mod runner;
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
    let _caps = app_caps::start([2147483648, 600, 1800, 512, 8, 32, 16777216, 100000, 100000]);
    app_caps::stage("crates/ps-qa/src/main.rs: running");
    nagoya::block_on(runner::run())
}
