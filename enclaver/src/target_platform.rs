
pub enum TargetPlatform {
    AMD64,
    ARM64,
}

impl TargetPlatform {
    pub fn to_str(&self) -> &str {
        match self {
            Self::AMD64 => "linux/amd64",
            Self::ARM64 => "linux/arm64",
        }
    }
}