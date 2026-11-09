use std::path::PathBuf;

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
    embed_guest();
}

/// Tier 2 guest: a static musl binary embedded in zcode and extracted at first use. Built here with a nested
/// cargo (own target dir) unless ZCODE_GUEST_BIN points at a prebuilt one; ZCODE_SKIP_GUEST=1 embeds nothing.
fn embed_guest() {
    println!("cargo:rerun-if-env-changed=ZCODE_GUEST_BIN");
    println!("cargo:rerun-if-env-changed=ZCODE_SKIP_GUEST");
    println!("cargo:rerun-if-changed=crates/zcode-guest");
    println!("cargo:rerun-if-changed=crates/zcode-tools/src");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let dest = out.join("zcode-guest.bin");
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    let bytes: Vec<u8> = if let Ok(p) = std::env::var("ZCODE_GUEST_BIN") {
        std::fs::read(&p).unwrap_or_else(|e| panic!("ZCODE_GUEST_BIN={p}: {e}"))
    } else if os != "linux" || std::env::var("ZCODE_SKIP_GUEST").is_ok() {
        vec![]
    } else {
        let target = format!("{arch}-unknown-linux-musl");
        let tdir = out.join("guest-target");
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let st = std::process::Command::new(cargo)
            .args(["build", "--release", "-p", "zcode-guest", "--target", &target, "--manifest-path", "Cargo.toml"])
            .env("CARGO_TARGET_DIR", &tdir).env("CARGO_PROFILE_RELEASE_OPT_LEVEL", "s").env("CARGO_PROFILE_RELEASE_PANIC", "abort").env_remove("RUSTFLAGS").env_remove("CARGO_ENCODED_RUSTFLAGS").status();
        match st {
            Ok(s) if s.success() => std::fs::read(tdir.join(&target).join("release/zcode-guest")).unwrap_or_default(),
            _ => {
                println!("cargo:warning=tier 2 guest not embedded: `cargo build -p zcode-guest --target {target}` failed (rustup target add {target}); `--sandbox container` will be unavailable");
                vec![]
            }
        }
    };
    std::fs::write(dest, bytes).unwrap();
}
