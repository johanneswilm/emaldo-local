// Rebuild when the embedded web assets change. `include_bytes!` compiles the
// file contents into the binary, but cargo does not treat an orphan content
// change in `static/` as a rebuild trigger on its own; these directives do.
fn main() {
    println!("cargo:rerun-if-changed=static/index.html");
    println!("cargo:rerun-if-changed=static/manifest.webmanifest");
    println!("cargo:rerun-if-changed=static/sw.js");
    println!("cargo:rerun-if-changed=static/icon.svg");
}