//! `heimdall-splash`: Splash's server in Rust. `serve` keeps `splash serve`'s command line for the options Heimdall
//! passes; `pull` prepares a model without serving it.
fn main() {
    std::process::exit(match splash::server::cli::main(std::env::args_os().skip(1).collect()) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error · {e:#}");
            1
        }
    });
}
