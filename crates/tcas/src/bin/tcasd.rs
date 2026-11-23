//! tcasd: one per user, started by `tcas` when needed (daemon-and-cli.md section 2).

fn main() {
    if let Err(e) = tcasd::server::run(tcas_sys::home::Home::from_env()) {
        eprintln!("tcasd: {e}");
        std::process::exit(1);
    }
}
