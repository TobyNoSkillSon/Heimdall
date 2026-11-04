fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (code, out) = heimdall_emergency::run(&args, None);
    // The approval hook answers nothing for a session Heimdall did not start: no output at all, not a blank line.
    if out.is_empty() {} else if code == 0 { println!("{out}") } else { eprintln!("{out}") }
    std::process::exit(code);
}
