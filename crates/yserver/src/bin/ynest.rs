use std::{env, process::ExitCode};

const DEFAULT_DISPLAY: u16 = 99;
const DEFAULT_WIDTH: u16 = 800;
const DEFAULT_HEIGHT: u16 = 600;

#[derive(Debug, PartialEq, Eq)]
struct Args {
    display: u16,
    width: u16,
    height: u16,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            display: DEFAULT_DISPLAY,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
        }
    }
}

fn parse_args<I: IntoIterator<Item = String>>(args: I) -> Result<Args, String> {
    let mut out = Args::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--geometry=") {
            (out.width, out.height) = parse_geometry(value)?;
        } else if arg == "--geometry" {
            let value = iter
                .next()
                .ok_or_else(|| "--geometry requires WxH argument".to_string())?;
            (out.width, out.height) = parse_geometry(&value)?;
        } else if let Ok(n) = arg.parse::<u16>() {
            out.display = n;
        } else {
            return Err(format!("unrecognized argument: {arg}"));
        }
    }
    Ok(out)
}

fn parse_geometry(s: &str) -> Result<(u16, u16), String> {
    let (w, h) = s
        .split_once('x')
        .ok_or_else(|| format!("--geometry expects WxH (e.g. 1024x768), got {s:?}"))?;
    let w: u16 = w
        .parse()
        .map_err(|_| format!("--geometry width is not a u16: {w:?}"))?;
    let h: u16 = h
        .parse()
        .map_err(|_| format!("--geometry height is not a u16: {h:?}"))?;
    if w == 0 || h == 0 {
        return Err(format!(
            "--geometry dimensions must be non-zero, got {w}x{h}"
        ));
    }
    Ok((w, h))
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let args = match parse_args(env::args().skip(1)) {
        Ok(a) => a,
        Err(err) => {
            eprintln!("ynest: {err}");
            eprintln!("usage: ynest [<display>] [--geometry WxH]");
            return ExitCode::FAILURE;
        }
    };

    match yserver_core::nested::run(args.display, args.width, args.height) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            log::error!("ynest: {err}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Args, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_args_is_default() {
        assert_eq!(parse(&[]).unwrap(), Args::default());
    }

    #[test]
    fn positional_display() {
        let a = parse(&["42"]).unwrap();
        assert_eq!(a.display, 42);
        assert_eq!(a.width, DEFAULT_WIDTH);
        assert_eq!(a.height, DEFAULT_HEIGHT);
    }

    #[test]
    fn geometry_separate_args() {
        let a = parse(&["--geometry", "1024x768"]).unwrap();
        assert_eq!(a.width, 1024);
        assert_eq!(a.height, 768);
        assert_eq!(a.display, DEFAULT_DISPLAY);
    }

    #[test]
    fn geometry_eq_form() {
        let a = parse(&["--geometry=1280x800"]).unwrap();
        assert_eq!(a.width, 1280);
        assert_eq!(a.height, 800);
    }

    #[test]
    fn display_and_geometry() {
        let a = parse(&["7", "--geometry", "1920x1080"]).unwrap();
        assert_eq!(a.display, 7);
        assert_eq!(a.width, 1920);
        assert_eq!(a.height, 1080);
    }

    #[test]
    fn missing_geometry_value_errors() {
        assert!(parse(&["--geometry"]).is_err());
    }

    #[test]
    fn malformed_geometry_errors() {
        assert!(parse(&["--geometry", "1024"]).is_err());
        assert!(parse(&["--geometry", "1024x"]).is_err());
        assert!(parse(&["--geometry", "ax768"]).is_err());
    }

    #[test]
    fn zero_geometry_errors() {
        assert!(parse(&["--geometry", "0x768"]).is_err());
        assert!(parse(&["--geometry", "1024x0"]).is_err());
    }

    #[test]
    fn unknown_arg_errors() {
        assert!(parse(&["--bogus"]).is_err());
    }
}
