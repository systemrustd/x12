#[cfg(any(target_os = "linux", target_os = "freebsd"))]
use std::env;
use std::process::ExitCode;

#[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
fn main() -> ExitCode {
    eprintln!("yserver: KMS backend requires Linux or FreeBSD");
    ExitCode::FAILURE
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let opts = match yserver::launch::parse_args(env::args().skip(1)) {
        Ok(o) => o,
        Err(err) => {
            eprintln!("yserver: {err}");
            eprintln!(
                "usage: yserver [:N | N] [vtN] [-seat NAME] [-auth FILE] \
                 [-displayfd N] [-nolisten PROTO] [-novtswitch] [--version]"
            );
            return ExitCode::FAILURE;
        }
    };

    if opts.show_version {
        println!("{}", yserver::version::line());
        return ExitCode::SUCCESS;
    }

    match yserver::run(opts) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("yserver: {err}");
            ExitCode::FAILURE
        }
    }
}
