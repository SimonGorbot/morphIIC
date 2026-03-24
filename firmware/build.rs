use std::{env, fs, fs::File, io::Write, path::PathBuf};

fn main() {
    let manifest_dir =
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("missing CARGO_MANIFEST_DIR"));
    let workspace_dir = manifest_dir
        .parent()
        .expect("firmware should be inside workspace root");

    let model_json = workspace_dir.join("models").join("device_model.json");
    let model_rs = manifest_dir.join("src").join("model.rs");

    gen_model::generate_from_paths(&model_json, &model_rs).unwrap_or_else(|err| {
        panic!(
            "failed to generate {} from {}: {err:#}",
            model_rs.display(),
            model_json.display()
        )
    });

    let model_text = fs::read_to_string(&model_json)
        .unwrap_or_else(|err| panic!("failed to read {}: {err:#}", model_json.display()));
    let parsed_model = gen_model::parse_model(&model_text)
        .unwrap_or_else(|err| panic!("failed to parse {}: {err:#}", model_json.display()));

    println!("cargo:rerun-if-changed={}", model_json.display());
    for csv_path in gen_model::resolve_csv_paths(&parsed_model, &model_json) {
        println!("cargo:rerun-if-changed={}", csv_path.display());
    }
    println!(
        "cargo:rerun-if-changed={}",
        workspace_dir.join("tools/gen_model/src/lib.rs").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        workspace_dir.join("tools/gen_model/src/main.rs").display()
    );

    let out = PathBuf::from(env::var_os("OUT_DIR").expect("missing OUT_DIR"));
    File::create(out.join("memory.x"))
        .expect("create memory.x copy")
        .write_all(include_bytes!("memory.x"))
        .expect("write memory.x copy");
    println!("cargo:rustc-link-search={}", out.display());

    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
}
