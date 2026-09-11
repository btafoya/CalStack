fn main() {
    // sqlx::migrate! embeds this directory at compile time but cargo doesn't
    // know to watch it; without this, adding/editing a migration file builds
    // a stale binary until something forces a rebuild.
    println!("cargo:rerun-if-changed=../../migrations");
}
