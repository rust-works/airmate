use std::process::ExitCode;

fn main() -> ExitCode {
    let code = airmate::cli::run(
        std::env::args_os(),
        &airmate::cli::common::Process::real(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    ExitCode::from(code)
}
