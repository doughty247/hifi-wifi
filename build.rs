use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=src/bpf/game_bypass.c");

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let dest_path = Path::new(&out_dir).join("game_bypass.o");

    let mut cmd = Command::new("clang");
    cmd.args([
        "-O2",
        "-target",
        "bpf",
        "-c",
        "src/bpf/game_bypass.c",
        "-o",
        dest_path.to_str().unwrap(),
    ]);

    // Manually pass include paths since -target bpf strips host search directories
    if Path::new("/home/linuxbrew/.linuxbrew/include").exists() {
        cmd.arg("-I/home/linuxbrew/.linuxbrew/include");
    }
    if Path::new("/usr/include").exists() {
        cmd.arg("-I/usr/include");
    }
    // Debian/Ubuntu/Fedora-style multiarch dirs hold asm/types.h
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    for triple in [
        format!("/usr/include/{}-linux-gnu", arch),
        format!("/usr/include/{}-linux-gnu", std::env::consts::ARCH),
    ] {
        if Path::new(&triple).exists() {
            cmd.arg(format!("-I{}", triple));
        }
    }

    match cmd.status() {
        Ok(s) if s.success() => {
            println!("cargo:warning=Successfully compiled src/bpf/game_bypass.c to BPF bytecode");
        }
        _ => {
            // An empty object makes the runtime fall back to legacy tc u32 filters.
            println!("cargo:warning=clang failed or is missing; building without eBPF bytecode (legacy tc fallback only)");
            std::fs::write(&dest_path, []).expect("failed to write empty BPF object");
        }
    }
}
