use std::{env, fs, path::Path};

use typify::{TypeSpace, TypeSpaceSettings};

fn main() {
    const INPUT_JSON: &str = "src/schema-2025-03-26.json";
    const OUTPUT_RUST: &str = "typify_generated.rs";

    // Tell cargo to re-run if the schema file changes
    println!("cargo:rerun-if-changed={}", INPUT_JSON);

    let content = std::fs::read_to_string(INPUT_JSON).unwrap();
    let schema = serde_json::from_str::<schemars::schema::RootSchema>(&content).unwrap();

    // The JSON Schema contains a lot of types that relate to the JSON-RPC protocol, which are not
    // needed since we have a separate crate that implements JSON-RPC in a Rust idiomatic way.  To
    // avoid slowing compilation down, we filter out those types here.
    //
    // TODO: Actually implement that filtering once we know which types we need and don't need

    let mut type_space = TypeSpace::new(TypeSpaceSettings::default().with_struct_builder(true));
    type_space.add_root_schema(schema).unwrap();

    let contents = prettyplease::unparse(&syn::parse2::<syn::File>(type_space.to_stream()).unwrap());

    let mut out_file = Path::new(&env::var("OUT_DIR").unwrap()).to_path_buf();
    out_file.push(OUTPUT_RUST);
    fs::write(out_file, contents).unwrap();
}
