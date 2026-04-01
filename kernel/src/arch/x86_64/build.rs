use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

fn run_command(program: &str, args: &[String], cwd: &Path) {
    let output = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap_or_else(|e| panic!("failed to execute {program}: {e}"));
    if !output.status.success() {
        panic!(
            "{program} failed\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let src = manifest_dir.join("src").join("arch").join("x86_64").join("trampoline.S");
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let obj = out_dir.join("ap_trampoline.o");
    let bin = out_dir.join("ap_trampoline.bin");

    println!("cargo:rerun-if-changed={}", src.display());

    run_command(
        "llvm-mc",
        &[
            "-triple".into(),
            "x86_64-unknown-none".into(),
            "-filetype=obj".into(),
            "-o".into(),
            obj.display().to_string(),
            src.display().to_string(),
        ],
        &manifest_dir,
    );

    run_command(
        "llvm-objcopy",
        &[
            "-O".into(),
            "binary".into(),
            "--only-section=.text".into(),
            obj.display().to_string(),
            bin.display().to_string(),
        ],
        &manifest_dir,
    );

    let size = std::fs::metadata(&bin)
        .unwrap_or_else(|e| panic!("failed to read trampoline binary metadata: {e}"))
        .len();
    assert!(
        size <= 4096,
        "trampoline binary is too large: {size} bytes (> 4096)"
    );
}
