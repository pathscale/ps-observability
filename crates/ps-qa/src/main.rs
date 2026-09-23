//! `ps-qa` command-line entrypoint.

mod app;
mod audit;
mod capture_analysis;
mod cli;
mod computed_style;
mod diagnostics;
mod inspector;
mod interaction;
mod layout_report;
mod paint_audit;
mod paint_color;
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
fn main() -> eyre::Result<()> {
    nagoya::block_on(runner::run())
}
