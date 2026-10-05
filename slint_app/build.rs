use std::path::PathBuf;

fn main() {
    slint_build::compile("ui/app.slint").unwrap();

    let wasm_path_str = std::env::var("PROGRAM")
        .expect("PROGRAM env var must point to a .wasm file");
    let wasm_path = PathBuf::from(&wasm_path_str);
    assert!(
        wasm_path.exists(),
        "WASM file not found: {}",
        wasm_path.display()
    );
    println!("cargo:rerun-if-env-changed=PROGRAM");
    println!("cargo:rerun-if-changed={}", wasm_path.display());

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest = PathBuf::from(&out_dir).join("embedded_wasm.rs");
    let wasm_abs = std::fs::canonicalize(&wasm_path).expect("failed to resolve WASM path");
    std::fs::write(
        &dest,
        format!("static WASM: &[u8] = include_bytes!({:?});\n", wasm_abs),
    )
    .expect("failed to write embedded WASM");

    let metadata_path_str = std::env::var("PROGRAM_METADATA")
        .expect("PROGRAM_METADATA env var must point to a metadata.json file");
    let metadata_path = PathBuf::from(&metadata_path_str);
    assert!(
        metadata_path.exists(),
        "metadata JSON not found: {}",
        metadata_path.display()
    );
    println!("cargo:rerun-if-env-changed=PROGRAM_METADATA");
    println!("cargo:rerun-if-changed={}", metadata_path.display());

    let metadata_raw = std::fs::read_to_string(&metadata_path)
        .expect("failed to read metadata.json");
    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&metadata_raw) {
        if let Some(name) = val.get("name").and_then(|n| n.as_str()) {
            println!("cargo:warning=metadata: name = \"{name}\"");
        }
        if let Some(kinds) = val.get("kinds").and_then(|k| k.as_object()) {
            println!("cargo:warning=metadata: {} kind(s) defined", kinds.len());
            for (name, info) in kinds {
                let field_count = info
                    .get("values")
                    .and_then(|v| v.as_object())
                    .map(|o| o.len())
                    .unwrap_or(0);
                let desc = info
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("");
                println!(
                    "cargo:warning=  kind \"{name}\": {field_count} field(s){}",
                    if desc.is_empty() {
                        String::new()
                    } else {
                        format!(" - {desc}")
                    }
                );
            }
        }
    }

    let embedded_path = PathBuf::from(&out_dir).join("embedded_metadata.rs");
    let escaped = metadata_path
        .to_string_lossy()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let content = format!(
        r#"pub const METADATA_JSON: &str = include_str!("{escaped}");"#
    );
    std::fs::write(&embedded_path, content).expect("failed to write embedded metadata");
}
