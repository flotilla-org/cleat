use cleat::{cli, server::SessionService};

fn main() {
    let cli = cli::parse();
    let service = if let Some(root) = cli.runtime_root.clone() {
        Ok(SessionService::new(cleat::runtime::RuntimeLayout::new(root)))
    } else {
        SessionService::discover()
    };
    let service = match service {
        Ok(service) => service,
        Err(err) => {
            eprintln!("{err}");
            std::process::exit(1);
        }
    };
    match cli::execute(cli, &service) {
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
