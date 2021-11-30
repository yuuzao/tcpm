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

pub fn segment_valid(start: u32, x: u32, end: u32) -> bool {
    le(start, x) && lt(x, end)
}

pub fn lt(lhs: u32, rhs: u32) -> bool {
    lhs.wrapping_sub(rhs) > (1 << 31)
}

pub fn le(lhs: u32, rhs: u32) -> bool {
    lt(lhs, rhs) || lhs == rhs
}
