// The web UI is embedded from ../web/dist. rust-embed resolves that folder when
// this crate compiles, so it must exist even before the UI is first built, and
// the crate must rebuild when a new UI build lands there.
fn main() {
    std::fs::create_dir_all("../web/dist").expect("create ../web/dist");
    println!("cargo:rerun-if-changed=../web/dist");
}
