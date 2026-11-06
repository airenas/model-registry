use model_registry::commands::Registry;
use std::io;
use std::process::ExitCode;

fn main() -> ExitCode {
    match Registry::default_commands().dispatch(std::env::args_os(), &mut io::stdout()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => match e.downcast_ref::<clap::Error>() {
            // prints help/usage (stdout) or parse errors (stderr) with proper exit code
            Some(ce) => {
                let _ = ce.print();
                ExitCode::from(ce.exit_code() as u8)
            }
            None => {
                eprintln!("{} {e}", model_registry::commands::error_prefix());
                ExitCode::FAILURE
            }
        },
    }
}
