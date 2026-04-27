use std::{env, process::ExitCode};

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let display = env::args()
        .nth(1)
        .and_then(|arg| arg.parse::<u16>().ok())
        .unwrap_or(99);

    match yserver_core::nested::run(display) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("ynest: {err}");
            ExitCode::FAILURE
        }
    }
}
