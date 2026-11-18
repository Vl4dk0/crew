fn main() {
    let code = match crew::cli::run_from_env() {
        Ok(code) => code,
        Err(err) => {
            crew::errln!("crew: {}", err.message);
            err.code
        }
    };
    std::process::exit(code);
}
