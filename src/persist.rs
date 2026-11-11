//! Restart manifests of the daemon: one file per instance holding its `create` request, its workers and its event
//! counter, so a restarted `zcode serve` can recreate instances, containers and agents (conversations come from the
//! session files). Files are 0600: the request carries the provider config including the API key.
use serde_json::Value;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;

pub fn dir() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME").map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"));
    base.join("zcode/daemon")
}

fn file(app: &str, name: &str) -> PathBuf { dir().join(format!("{app}__{name}.json")) }

pub fn save(app: &str, name: &str, m: &Value) {
    use std::io::Write;
    let d = dir();
    if std::fs::create_dir_all(&d).is_err() { return; }
    let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
    let path = file(app, name);
    let tmp = path.with_extension("tmp");
    let ok = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)
        .and_then(|mut f| f.write_all(m.to_string().as_bytes()));
    if ok.is_ok() { let _ = std::fs::rename(&tmp, &path); }
}

pub fn remove(app: &str, name: &str) { let _ = std::fs::remove_file(file(app, name)); }

/// Every readable manifest, oldest first (so `sandbox_of` sources come back before their users).
pub fn load_all() -> Vec<Value> {
    let mut v: Vec<(std::time::SystemTime, Value)> = std::fs::read_dir(dir()).into_iter().flatten().flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            let m = serde_json::from_str::<Value>(&std::fs::read_to_string(e.path()).ok()?).ok()?;
            (m["app"].is_string() && m["name"].is_string() && m["req"].is_object()).then(|| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), m))
        }).collect();
    v.sort_by_key(|(t, _)| *t);
    v.into_iter().map(|(_, m)| m).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip_and_remove() {
        let d = std::env::temp_dir().join(format!("zc-persist-{}", std::process::id()));
        // SAFETY: only this test touches XDG_STATE_HOME in this binary
        unsafe { std::env::set_var("XDG_STATE_HOME", &d); }
        save("a", "x", &json!({"app": "a", "name": "x", "req": {"cwd": "/"}, "seq": 7}));
        let all = load_all();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["seq"], 7);
        assert_eq!(std::fs::metadata(file("a", "x")).unwrap().permissions().mode() & 0o777, 0o600);
        remove("a", "x");
        assert!(load_all().is_empty());
        let _ = std::fs::remove_dir_all(d);
    }
}
