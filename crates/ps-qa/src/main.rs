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

#[tokio::main(flavor = "current_thread")]
async fn main() -> eyre::Result<()> {
    runner::run().await
}
