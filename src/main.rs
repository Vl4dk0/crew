fn main() {
    let code = match crew::cli::run_from_env() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("crew: {}", err.message);
            err.code
        }
    };
    std::process::exit(code);
}
