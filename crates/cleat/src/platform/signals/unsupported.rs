pub fn signal_number(normalized: &str) -> Result<i32, String> {
    let signal = match normalized {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "KILL" => 9,
        "TERM" => 15,
        "STOP" => 19,
        "TSTP" => 20,
        "CONT" => 18,
        "USR1" => 10,
        "USR2" => 12,
        other => return Err(format!("unknown signal: {other}")),
    };
    Ok(signal)
}
