use super::*;

#[test]
fn references_parse_like_docker() {
    let r = |s| parse_ref(s).unwrap();
    assert_eq!(r("alpine"), Ref { endpoint: "registry-1.docker.io".into(), repo: "library/alpine".into(), reference: "latest".into() });
    assert_eq!(r("node:20-slim").reference, "20-slim");
    assert_eq!(r("docker.io/foo/bar:1").repo, "foo/bar");
    assert_eq!(r("ghcr.io/org/app:v2"), Ref { endpoint: "ghcr.io".into(), repo: "org/app".into(), reference: "v2".into() });
    assert_eq!(r("localhost:5000/x/y").endpoint, "localhost:5000");
    let d = format!("sha256:{}", "a".repeat(64));
    assert_eq!(r(&format!("alpine@{d}")).reference, d);
    for bad in ["", "a/../b", "UPPER", "alpine@md5:abc", "alpine:", "a//b", "alpine:-x"] { assert!(parse_ref(bad).is_err(), "{bad}"); }
    assert!(Platform::parse("linux/arm64/v8").is_ok() && Platform::parse("a/b/c/d").is_err());
}

#[test]
fn challenges_parse_with_commas_inside_quotes() {
    let p = parse_challenge(r#"Bearer realm="https://auth.example/token",service="registry.example",scope="repository:a/b:pull,push""#).unwrap();
    assert_eq!(p[0], ("realm".into(), "https://auth.example/token".into()));
    assert_eq!(p[2], ("scope".into(), "repository:a/b:pull,push".into()));
    assert!(parse_challenge("Basic realm=\"x\"").is_none());
}

