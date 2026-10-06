fn main() {
    let result = std::panic::catch_unwind(macpro_fand::runtime::entry);
    std::process::exit(match result {
        Ok(code) => code,
        Err(_) => {
            eprintln!("macpro-fand: panic; safety cleanup attempted, fans left at caps");
            1
        }
    });
}
