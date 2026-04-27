use std::process::ExitCode;

fn main() -> ExitCode {
    eprintln!("yserver: standalone DRM/KMS mode is not implemented yet; use ynest");
    ExitCode::FAILURE
}
