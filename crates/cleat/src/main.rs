use cleat::{cli, server::SessionService};

fn main() {
    let cli = cli::parse();
    let service = if let Some(root) = cli.runtime_root.clone() {
        SessionService::new(cleat::runtime::RuntimeLayout::new(root))
    } else {
        SessionService::discover()
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
