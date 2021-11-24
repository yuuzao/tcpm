use simplelog::*;

pub fn logging(level: &str) {
    let level = match level {
        "info" => LevelFilter::Info,
        _ => LevelFilter::Debug,
    };
    TermLogger::init(
        level,
        Config::default(),
        TerminalMode::Mixed,
        ColorChoice::Auto,
    )
    .unwrap();
}

pub fn is_between_wrapped(start: u32, x: u32, end: u32) -> bool {
    wrapping_lt(start, x) && wrapping_lt(x, end)
}

pub fn wrapping_lt(lhs: u32, rhs: u32) -> bool {
    lhs.wrapping_sub(rhs) > (1 << 31)
}
