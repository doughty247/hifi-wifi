use nix::unistd::Uid;
use std::process::Command;

pub fn is_root() -> bool {
    Uid::effective().is_root()
}

/// Re-execute the current program with sudo if not running as root.
/// Exits the process after exec attempt.
pub fn require_root() -> ! {
    if !is_root() {
        let current_exe = std::env::current_exe()
            .unwrap_or_else(|_| std::path::PathBuf::from(std::env::args().next().unwrap()));
        
        let args: Vec<String> = std::env::args().skip(1).collect();
        
        let status = Command::new("sudo")
            .arg(current_exe)
            .args(&args)
            .status();
        
        let exit_code = match status {
            Ok(s) => s.code().unwrap_or(1),
            Err(_) => {
                eprintln!("Failed to execute sudo");
                1
            }
        };
        std::process::exit(exit_code);
    }
    std::process::exit(0); // Unreachable, but satisfies ! return type
}

