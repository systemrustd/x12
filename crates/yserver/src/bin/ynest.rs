use std::env;
use std::process::ExitCode;

fn main() -> ExitCode {
    let display = env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<u16>().ok())
        .unwrap_or(99);

    match yserver_core::nested::run(display) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("ynest: {err}");
            ExitCode::FAILURE
        }
    }
}
