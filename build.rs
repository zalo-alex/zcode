fn main() {
    // build id shown by --version: commit from CI (ZCODE_COMMIT) or git, else empty
    let commit = std::env::var("ZCODE_COMMIT").ok().filter(|s| !s.is_empty()).or_else(|| {
        std::process::Command::new("git").args(["rev-parse", "--short", "HEAD"]).output().ok()
            .filter(|o| o.status.success()).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }).unwrap_or_default();
    println!("cargo:rustc-env=ZCODE_BUILD={}", if commit.is_empty() { String::new() } else { format!(" ({commit})") });
    println!("cargo:rustc-env=ZCODE_DEFAULT_REPO={}", std::env::var("ZCODE_DEFAULT_REPO").unwrap_or_else(|_| "zalo-alex/zcode".into()));
    println!("cargo:rerun-if-env-changed=ZCODE_COMMIT");
    println!("cargo:rerun-if-env-changed=ZCODE_DEFAULT_REPO");
}
