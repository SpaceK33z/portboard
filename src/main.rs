use portboard::portboard_cli::run_portboard_cli;

fn main() {
    let arguments = std::env::args().skip(1).collect::<Vec<_>>();
    if let Err(error) = run_portboard_cli(&arguments) {
        eprintln!("{error:#}");
        std::process::exit(1);
    }
}
