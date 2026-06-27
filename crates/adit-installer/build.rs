use std::{
    env, fs,
    path::{Path, PathBuf},
};

fn main() {
    println!("cargo:rerun-if-env-changed=ADIT_APP_EXE");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is set by Cargo"));
    let embedded_exe = out_dir.join("adit-app.exe");

    match env::var_os("ADIT_APP_EXE") {
        Some(path) => copy_app(Path::new(&path), &embedded_exe),
        None => fs::write(&embedded_exe, []).expect("placeholder installer payload is written"),
    }
}

fn copy_app(source: &Path, destination: &Path) {
    if !source.is_file() {
        panic!(
            "ADIT_APP_EXE does not point to a file: {}",
            source.display()
        );
    }

    fs::copy(source, destination).unwrap_or_else(|error| {
        panic!(
            "failed to embed {} into installer payload: {error}",
            source.display()
        )
    });
}
