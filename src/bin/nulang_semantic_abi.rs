fn main() {
    match nulang::semantic_abi::value_layout_manifest_json_pretty() {
        Ok(json) => println!("{json}"),
        Err(error) => {
            eprintln!("failed to serialize Nulang semantic ABI: {error}");
            std::process::exit(1);
        }
    }
}
