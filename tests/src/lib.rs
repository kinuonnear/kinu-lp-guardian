// Integration tests live in tests/; build the wasm first: ./scripts/build.sh

/// The Rhea DCL the sandbox tests run against: the fixture of the tested build (DCL 2.4.0,
/// FvsWeo3Z…), or the wasm at `DCL_WASM` to check a new Rhea build before it reaches Guardian users:
///   DCL_WASM=/path/to/new-dcl.wasm cargo test --test shared
pub fn dcl_wasm() -> Vec<u8> {
    match std::env::var("DCL_WASM") {
        Ok(path) => std::fs::read(&path).unwrap_or_else(|e| panic!("DCL_WASM={path}: {e}")),
        Err(_) => std::fs::read(format!("{}/fixtures/dcl-2.4.0.wasm", env!("CARGO_MANIFEST_DIR"))).expect("tests/fixtures/dcl-2.4.0.wasm"),
    }
}
