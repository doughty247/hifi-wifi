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

    let status = cmd.status();

    match status {
        Ok(s) if s.success() => {
            println!("cargo:warning=Successfully compiled src/bpf/game_bypass.c to BPF bytecode");
        }
        _ => {
            panic!("Error: clang compilation failed or clang is missing. Clang and LLVM are mandatory for v3.1.0 to compile the eBPF game bypass program.");
        }
    }
}
