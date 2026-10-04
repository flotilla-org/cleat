use cleat::cli;

fn main() {
    finish(cli::execute_discovered(cli::parse()));
}

fn finish(result: cli::ExecResult) {
    match result {
        cli::ExecResult::Ok(Some(output)) => println!("{output}"),
        cli::ExecResult::Ok(None) => {}
        cli::ExecResult::Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
        cli::ExecResult::Exit { code, message, output } => {
            if let Some(output) = output {
                println!("{output}");
            }
            if let Some(message) = message {
                eprintln!("{message}");
            }
            std::process::exit(code);
        }
    }
}
