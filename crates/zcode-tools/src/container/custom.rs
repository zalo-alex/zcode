//! User customization of the container: extra mounts, env, caps, ulimits, ... plus a raw OCI merge-patch.
//! Parsed from the `container` config object (global) and from a profile's `container` object (override).
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq)]
pub struct Mount {
    pub src: PathBuf,
    pub dst: PathBuf,
    /// extra mount options on top of `rbind,nosuid,nodev`; `ro` implies `rro`
    pub opts: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Custom {
    pub mounts: Vec<Mount>,
    /// (destination, options)
    pub tmpfs: Vec<(PathBuf, Vec<String>)>,
    pub env: BTreeMap<String, String>,
    pub hostname: Option<String>,
    pub user: Option<(u32, u32)>,
    pub cap_add: Vec<String>,
    pub cap_drop: Vec<String>,
    /// lowercase name (`nofile`) -> (soft, hard)
    pub ulimits: BTreeMap<String, (u64, u64)>,
    pub sysctls: BTreeMap<String, String>,
    /// host device nodes bound at the same path
    pub devices: Vec<PathBuf>,
    pub seccomp_deny: Vec<String>,
    pub seccomp_allow: Vec<String>,
    /// RFC 7386 JSON merge-patch applied last to the generated OCI config
    pub oci_patch: Option<Value>,
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") { Some(r) => crate::sandbox::home().join(r), None if p == "~" => crate::sandbox::home(), None => PathBuf::from(p) }
}

fn strs(v: Option<&Value>, key: &str) -> Result<Vec<String>, String> {
    match v {
        None | Some(Value::Null) => Ok(vec![]),
        Some(Value::Array(a)) => a.iter().map(|x| x.as_str().map(String::from).ok_or_else(|| format!("container.{key} must be a list of strings"))).collect(),
        Some(_) => Err(format!("container.{key} must be a list of strings")),
    }
}

fn opts(s: &str) -> Vec<String> { s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect() }

fn abs(p: PathBuf, what: &str) -> Result<PathBuf, String> {
    if p.is_absolute() { Ok(p) } else { Err(format!("container.{what}: `{}` must be an absolute path", p.display())) }
}

fn parse_mount(v: &Value) -> Result<Mount, String> {
    match v {
        Value::String(s) => {
            let mut it = s.splitn(3, ':');
            let (src, dst) = (it.next().unwrap_or(""), it.next().ok_or_else(|| format!("container.mounts: `{s}` is not `src:dst[:opts]`"))?);
            Ok(Mount { src: abs(expand(src), "mounts")?, dst: abs(PathBuf::from(dst), "mounts")?, opts: it.next().map(opts).unwrap_or_default() })
        }
        Value::Object(o) => {
            let g = |k: &str| o.get(k).and_then(|x| x.as_str());
            if let Some(t) = g("type") && t != "bind" { return Err(format!("container.mounts: type `{t}` is not supported (use `tmpfs` for tmpfs)")); }
            let (src, dst) = (g("src").ok_or("container.mounts: object needs `src`")?, g("dst").ok_or("container.mounts: object needs `dst`")?);
            let mut ops = o.get("options").and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>()).unwrap_or_default();
            if o.get("ro").and_then(|x| x.as_bool()) == Some(true) { ops.push("ro".into()); }
            Ok(Mount { src: abs(expand(src), "mounts")?, dst: abs(PathBuf::from(dst), "mounts")?, opts: ops })
        }
        _ => Err("container.mounts: entries are `src:dst[:opts]` strings or objects".into()),
    }
}

impl Custom {
    /// Parse a `container` object. Only syntax is checked; host paths are checked by [`Custom::validate`].
    pub fn from_value(c: &Value) -> Result<Custom, String> {
        let mut out = Custom::default();
        if let Some(a) = c.get("mounts") {
            for m in a.as_array().ok_or("container.mounts must be a list")? { out.mounts.push(parse_mount(m)?); }
        }
        for t in strs(c.get("tmpfs"), "tmpfs")? {
            let (d, o) = t.split_once(':').unwrap_or((&t, ""));
            out.tmpfs.push((abs(PathBuf::from(d), "tmpfs")?, opts(o)));
        }
        match c.get("env") {
            None | Some(Value::Null) => {}
            Some(Value::Object(m)) => for (k, v) in m { out.env.insert(k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string())); },
            Some(Value::Array(a)) => for x in a {
                let s = x.as_str().ok_or("container.env list entries must be `KEY=value` strings")?;
                let (k, v) = s.split_once('=').ok_or_else(|| format!("container.env: `{s}` is not KEY=value"))?;
                out.env.insert(k.into(), v.into());
            },
            Some(_) => return Err("container.env must be an object or a list of KEY=value".into()),
        }
        for k in out.env.keys() { if k.is_empty() || k.contains('=') { return Err(format!("container.env: bad name `{k}`")); } }
        out.hostname = c.get("hostname").and_then(|v| v.as_str()).map(String::from);
        if let Some(u) = c.get("user") {
            let s = u.as_str().ok_or("container.user must be `uid[:gid]`")?;
            let (a, b) = s.split_once(':').unwrap_or((s, s));
            out.user = Some((a.parse().map_err(|_| format!("container.user: bad uid `{a}`"))?, b.parse().map_err(|_| format!("container.user: bad gid `{b}`"))?));
        }
        let cap = |v: Vec<String>| v.into_iter().map(|x| { let x = x.to_uppercase(); if x.starts_with("CAP_") { x } else { format!("CAP_{x}") } }).collect::<Vec<_>>();
        out.cap_add = cap(strs(c.get("cap_add"), "cap_add")?);
        out.cap_drop = cap(strs(c.get("cap_drop"), "cap_drop")?);
        if let Some(m) = c.get("ulimits") {
            for (k, v) in m.as_object().ok_or("container.ulimits must be an object")? {
                let lim = match v {
                    Value::Number(n) => n.as_u64().map(|n| (n, n)),
                    Value::Array(a) if a.len() == 2 => a[0].as_u64().zip(a[1].as_u64()),
                    _ => None,
                }.ok_or_else(|| format!("container.ulimits.{k} must be a number or [soft, hard]"))?;
                out.ulimits.insert(k.to_lowercase(), lim);
            }
        }
        if let Some(m) = c.get("sysctls") {
            for (k, v) in m.as_object().ok_or("container.sysctls must be an object")? {
                out.sysctls.insert(k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
            }
        }
        for d in strs(c.get("devices"), "devices")? { out.devices.push(abs(expand(&d), "devices")?); }
        out.seccomp_deny = strs(c.get("seccomp_deny"), "seccomp_deny")?;
        out.seccomp_allow = strs(c.get("seccomp_allow"), "seccomp_allow")?;
        match c.get("oci_patch") {
            None | Some(Value::Null) => {}
            Some(p @ Value::Object(_)) => out.oci_patch = Some(p.clone()),
            Some(_) => return Err("container.oci_patch must be an object".into()),
        }
        Ok(out)
    }

    /// Host-side checks: every mount source and device must exist.
    pub fn validate(&self) -> Result<(), String> {
        for m in &self.mounts { if !m.src.exists() { return Err(format!("container.mounts: source `{}` does not exist", m.src.display())); } }
        for d in &self.devices { if !d.exists() { return Err(format!("container.devices: `{}` does not exist", d.display())); } }
        Ok(())
    }

    /// `over` (a profile) on top of `self` (global): lists append, maps merge by key, scalars replace.
    pub fn merged(&self, over: &Custom) -> Custom {
        let mut o = self.clone();
        o.mounts.extend(over.mounts.iter().cloned());
        o.tmpfs.extend(over.tmpfs.iter().cloned());
        o.env.extend(over.env.clone());
        o.hostname = over.hostname.clone().or(o.hostname);
        o.user = over.user.or(o.user);
        o.cap_add.extend(over.cap_add.iter().cloned());
        o.cap_drop.extend(over.cap_drop.iter().cloned());
        o.ulimits.extend(over.ulimits.clone());
        o.sysctls.extend(over.sysctls.clone());
        o.devices.extend(over.devices.iter().cloned());
        o.seccomp_deny.extend(over.seccomp_deny.iter().cloned());
        o.seccomp_allow.extend(over.seccomp_allow.iter().cloned());
        o.oci_patch = match (o.oci_patch.take(), &over.oci_patch) {
            (Some(mut a), Some(b)) => { merge_patch(&mut a, b); Some(a) }
            (a, b) => b.clone().or(a),
        };
        o
    }

    /// OCI mount entries for the custom bind mounts, tmpfs and devices.
    pub fn oci_mounts(&self) -> Vec<Value> {
        let mut v = vec![];
        for m in &self.mounts {
            let mut o: Vec<String> = vec!["rbind".into(), "nosuid".into(), "nodev".into()];
            for x in &m.opts {
                match x.as_str() {
                    "rw" | "rbind" | "bind" => {}
                    "ro" => { for t in ["ro", "rro"] { if !o.iter().any(|y| y == t) { o.push(t.into()); } } }
                    "suid" => o.retain(|y| y != "nosuid"),
                    "dev" => o.retain(|y| y != "nodev"),
                    other => o.push(other.into()),
                }
            }
            v.push(json!({"destination": m.dst, "type": "bind", "source": m.src, "options": o}));
        }
        for (d, o) in &self.tmpfs {
            let mut o = o.clone();
            for b in ["nosuid", "nodev"] { if !o.iter().any(|y| y == b) { o.push(b.into()); } }
            v.push(json!({"destination": d, "type": "tmpfs", "source": "tmpfs", "options": o}));
        }
        for d in &self.devices { v.push(json!({"destination": d, "type": "bind", "source": d, "options": ["rbind", "nosuid"]})); }
        v
    }
}

/// RFC 7386: objects merge recursively, `null` deletes a key, anything else (arrays too) replaces.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    if let Value::Object(p) = patch {
        if !target.is_object() { *target = json!({}); }
        let t = target.as_object_mut().unwrap();
        for (k, v) in p {
            if v.is_null() { t.remove(k); } else { merge_patch(t.entry(k.clone()).or_insert(Value::Null), v); }
        }
    } else {
        *target = patch.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container::oci::{Spec, config};

    fn cfg_with(c: &Custom) -> Value {
        let s = Spec { rootfs: std::path::Path::new("/r"), project: std::path::Path::new("/p"), project_rw: true, write: &[], guest: std::path::Path::new("/g"),
            sockdir: std::path::Path::new("/s"), resolv: None, agent: "a", net: false, mem: None, cpus: None, pids: None, cgroup_path: None, seccomp: true,
            readonly_rootfs: false, custom: c };
        config(&s)
    }

    #[test]
    fn parses_mount_strings_objects_and_home() {
        let v = json!({"mounts": ["~/x:/y:ro", "/a:/b", {"src": "/c", "dst": "/d", "ro": true}]});
        let c = Custom::from_value(&v).unwrap();
        assert_eq!(c.mounts[0].src, crate::sandbox::home().join("x"));
        assert_eq!(c.mounts[0].opts, ["ro"]);
        assert_eq!(c.mounts[2].opts, ["ro"]);
        assert!(Custom::from_value(&json!({"mounts": ["nodst"]})).is_err());
        assert!(Custom::from_value(&json!({"mounts": ["rel:/b"]})).is_err());
        assert!(Custom::from_value(&json!({"env": ["NOEQ"]})).is_err());
        assert!(Custom::from_value(&json!({"user": "x"})).is_err());
    }

    #[test]
    fn validate_rejects_missing_source() {
        let c = Custom::from_value(&json!({"mounts": ["/definitely/not/here:/z"]})).unwrap();
        assert!(c.validate().unwrap_err().contains("does not exist"));
    }

    #[test]
    fn applies_to_oci_config() {
        let c = Custom::from_value(&json!({
            "mounts": ["/tmp:/data:ro"], "tmpfs": ["/scratch:size=1m"], "env": {"FOO": "bar", "HOME": "/h"}, "hostname": "box",
            "cap_add": ["net_raw"], "cap_drop": ["NET_ADMIN"], "ulimits": {"nofile": [1, 2]}, "sysctls": {"a.b": "1"},
            "seccomp_deny": ["chroot"], "seccomp_allow": ["ptrace"], "user": "1000:100",
        })).unwrap();
        let j = cfg_with(&c);
        let m = j["mounts"].as_array().unwrap();
        let d = m.iter().find(|x| x["destination"] == "/data").unwrap();
        assert!(d["options"].to_string().contains("rro"));
        assert!(m.iter().any(|x| x["destination"] == "/scratch" && x["type"] == "tmpfs"));
        let env = j["process"]["env"].to_string();
        assert!(env.contains("FOO=bar") && env.contains("HOME=/h") && !env.contains("HOME=/root"));
        assert_eq!(j["hostname"], "box");
        let caps = j["process"]["capabilities"]["effective"].to_string();
        assert!(caps.contains("CAP_NET_RAW") && !caps.contains("CAP_NET_ADMIN"));
        assert!(j["process"]["rlimits"].to_string().contains("\"hard\":2"));
        assert_eq!(j["linux"]["sysctl"]["a.b"], "1");
        let deny = j["linux"]["seccomp"]["syscalls"][0]["names"].to_string();
        assert!(deny.contains("chroot") && !deny.contains("ptrace"));
        assert_eq!(j["process"]["user"]["uid"], 1000);
        assert_eq!(j["process"]["noNewPrivileges"], true);
    }

    #[test]
    fn oci_patch_merges_and_replaces_arrays() {
        let c = Custom::from_value(&json!({"oci_patch": {"process": {"noNewPrivileges": false, "cwd": "/w"}, "linux": {"maskedPaths": ["/x"], "rootfsPropagation": null}}})).unwrap();
        let j = cfg_with(&c);
        assert_eq!(j["process"]["noNewPrivileges"], false);
        assert_eq!(j["process"]["cwd"], "/w");
        assert_eq!(j["linux"]["maskedPaths"], json!(["/x"]));
        assert!(j["linux"].get("rootfsPropagation").is_none());
        assert!(j["process"]["args"].is_array());
    }

    #[test]
    fn profile_layers_over_global() {
        let g = Custom::from_value(&json!({"mounts": ["/a:/a"], "env": {"X": "1", "Y": "1"}, "hostname": "g"})).unwrap();
        let p = Custom::from_value(&json!({"mounts": ["/b:/b"], "env": {"Y": "2"}})).unwrap();
        let m = g.merged(&p);
        assert_eq!(m.mounts.len(), 2);
        assert_eq!((m.env["X"].as_str(), m.env["Y"].as_str()), ("1", "2"));
        assert_eq!(m.hostname.as_deref(), Some("g"));
    }
}
