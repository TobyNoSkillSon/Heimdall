//! Lab probe: runs one `computer` call on this Mac with the tool switched on, as Heimdall would.
//!   heimdall-computer-probe '{"action":"find","app":"ComputerLab"}' [--clicks]
//! For the lab test app only (lab/computer); never point it at the user's apps.
fn main() {
    let mut args = std::env::args().skip(1);
    let Some(json) = args.next() else {
        eprintln!("usage: heimdall-computer-probe '<arguments as JSON>' [--clicks]");
        std::process::exit(2);
    };
    let clicks = args.any(|a| a == "--clicks");
    let value: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("bad JSON: {e}");
            std::process::exit(2);
        }
    };
    heimdall_computer::gate::set_enabled(true);
    heimdall_computer::gate::set_clicks(clicks);
    // The lab app holds no private text: Seeing counts as on; no words of the user's.
    match heimdall_computer::run(&value, &[], true) {
        Ok(line) => println!("{line}"),
        Err(e) => {
            println!("{e}");
            std::process::exit(1);
        }
    }
}
