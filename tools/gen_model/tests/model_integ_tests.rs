use std::fs;
use std::path::{Path, PathBuf};

use anyhow::Result;
use gen_model::{generate_from_paths, load_csv_samples, parse_model, resolve_csv_paths};
use tempfile::TempDir;

fn model_path(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join(rel)
}

#[test]
fn valid_model_parses_and_generates() -> Result<()> {
    let model_path = model_path("models/valid_mixed.json");
    let model_text = fs::read_to_string(&model_path)?;
    let parsed = parse_model(&model_text)?;

    let csv_paths = resolve_csv_paths(&parsed, &model_path);
    assert_eq!(csv_paths.len(), 2);
    for path in &csv_paths {
        assert!(
            path.exists(),
            "expected CSV path to exist: {}",
            path.display()
        );
    }

    let output_dir = TempDir::new()?;
    let out_path = output_dir.path().join("model.rs");
    generate_from_paths(&model_path, &out_path)?;

    let generated = fs::read_to_string(&out_path)?;
    assert!(generated.contains("pub const DEVICE_NAME: &str = \"test_device\";"));
    assert!(generated.contains("CsvMode::Embedded"));
    assert!(generated.contains("CsvMode::HostStream"));
    Ok(())
}

#[test]
fn invalid_models_fail_validation() -> Result<()> {
    let invalid_cases = [
        (
            "models/invalid_duplicate_addr.json",
            "duplicate register addr",
        ),
        ("models/invalid_unknown_field.json", "unknown field"),
        ("models/invalid_enum.json", "unknown variant"),
        (
            "models/invalid_csv_rw.json",
            "uses CSV but is not read-only",
        ),
    ];

    for (model, expected) in invalid_cases {
        let model = fs::read_to_string(model_path(model))?;
        let err = parse_model(&model).unwrap_err();
        let rendered = format!("{err:#}");
        assert!(
            rendered.contains(expected),
            "model {model}: expected {expected:?}, got {rendered}"
        );
    }

    Ok(())
}

#[test]
fn csv_samples_load_expected_values() -> Result<()> {
    let embedded_path = model_path("models/csv/embedded.csv");
    let host_path = model_path("models/csv/host.csv");

    let embedded = load_csv_samples(&embedded_path)?;
    let host = load_csv_samples(&host_path)?;

    assert_eq!(embedded, vec![0xA0, 0xA1, 0xA2, 255]);
    assert_eq!(host, vec![10, 20, 30, 40]);
    Ok(())
}
