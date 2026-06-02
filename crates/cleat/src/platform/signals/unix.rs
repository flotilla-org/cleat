pub fn signal_number(normalized: &str) -> Result<i32, String> {
    let signal = match normalized {
        "HUP" => libc::SIGHUP,
        "INT" => libc::SIGINT,
        "QUIT" => libc::SIGQUIT,
        "KILL" => libc::SIGKILL,
        "TERM" => libc::SIGTERM,
        "STOP" => libc::SIGSTOP,
        "TSTP" => libc::SIGTSTP,
        "CONT" => libc::SIGCONT,
        "USR1" => libc::SIGUSR1,
        "USR2" => libc::SIGUSR2,
        other => return Err(format!("unknown signal: {other}")),
    };
    Ok(signal)
}
