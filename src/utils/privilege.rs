use nix::unistd::Uid;

pub fn is_root() -> bool {
    Uid::effective().is_root()
}
