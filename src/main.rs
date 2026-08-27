//! Serve one Blitz document over the inspection socket, with no window.

fn main() {
    if let Err(error) = qa_headless_host::serve() {
        eprintln!("qa-headless-host: {error}");
        std::process::exit(1);
    }
}
