use std::env;
use std::path::PathBuf;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    let (input, output) = match args.len() {
        1 => (
            PathBuf::from("models/device_model.json"),
            PathBuf::from("firmware/src/model.rs"),
        ),
        3 => (PathBuf::from(&args[1]), PathBuf::from(&args[2])),
        _ => {
            eprintln!(
                "usage: gen_model [<input_json> <output_rs>]\n       defaults: models/device_model.json firmware/src/model.rs"
            );
            std::process::exit(2);
        }
    };

    gen_model::generate_from_paths(&input, &output)
        .with_context(|| format!("generating {} from {}", output.display(), input.display()))?;

    println!("generated {}", output.display());
    Ok(())
}
