//! Serve one Blitz document over the inspection socket, with no window.

fn main() {
    if let Err(error) = qa_inspect_host::serve() {
        eprintln!("qa-inspect-host: {error}");
        std::process::exit(1);
    }
}
