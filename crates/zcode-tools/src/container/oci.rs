//! OCI runtime `config.json`, built with serde_json from a small template plus the agent's Policy.
use serde_json::{Value, json};
use super::custom::{Custom, merge_patch};
use std::path::{Path, PathBuf};

pub struct Spec<'a> {
    pub rootfs: &'a Path,
    pub project: &'a Path,
    /// the whole project is writable (the lead); otherwise only `write`
    pub project_rw: bool,
    pub write: &'a [PathBuf],
    pub guest: &'a Path,
    pub sockdir: &'a Path,
    pub agent: &'a str,
    pub net: bool,
    /// resolv.conf to mount instead of the host's (slirp4netns: its own resolver)
    pub resolv: Option<&'a Path>,
    pub mem: Option<u64>,
    pub cpus: Option<f64>,
    pub pids: Option<u64>,
    pub cgroup_path: Option<String>,
    pub seccomp: bool,
    pub readonly_rootfs: bool,
    pub custom: &'a Custom,
}

const DENY_SYSCALLS: &[&str] = &[
    "keyctl", "add_key", "request_key", "kexec_load", "kexec_file_load", "bpf", "perf_event_open", "userfaultfd",
    "mount", "umount", "umount2", "pivot_root", "setns", "unshare", "io_uring_setup", "io_uring_enter", "io_uring_register",
    "open_by_handle_at", "init_module", "finit_module", "delete_module", "reboot", "swapon", "swapoff", "acct",
    "settimeofday", "clock_settime", "clock_adjtime", "adjtimex", "ptrace", "process_vm_readv", "process_vm_writev",
];

fn bind(src: &Path, dst: &Path, opts: &[&str]) -> Value {
    json!({"destination": dst, "type": "bind", "source": src, "options": opts})
}

pub fn config(s: &Spec) -> Value {
    let mut mounts = vec![
        json!({"destination":"/proc","type":"proc","source":"proc"}),
        json!({"destination":"/dev","type":"tmpfs","source":"tmpfs","options":["nosuid","strictatime","mode=755","size=65536k"]}),
        json!({"destination":"/dev/pts","type":"devpts","source":"devpts","options":["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620"]}),
        json!({"destination":"/dev/shm","type":"tmpfs","source":"shm","options":["nosuid","noexec","nodev","mode=1777","size=65536k"]}),
        json!({"destination":"/tmp","type":"tmpfs","source":"tmpfs","options":["nosuid","nodev","mode=1777"]}),
    ];
    for d in ["null", "zero", "full", "random", "urandom"] {
        let p = PathBuf::from(format!("/dev/{d}"));
        mounts.push(bind(&p, &p, &["rbind", "nosuid", "noexec"]));
    }
    mounts.push(bind(s.guest, Path::new("/zcode-guest"), &["bind", "ro", "nosuid"]));
    mounts.push(bind(s.sockdir, Path::new("/run/zcode"), &["bind", "nosuid", "nodev"]));
    let resolv = s.resolv.unwrap_or(Path::new("/etc/resolv.conf"));
    if s.net && resolv.exists() {
        mounts.push(bind(resolv, Path::new("/etc/resolv.conf"), &["bind", "ro", "nosuid", "nodev", "noexec"]));
    }
    // project at the SAME absolute path: read-only first, then read-write binds (OCI mounts apply in order)
    if s.project_rw {
        mounts.push(bind(s.project, s.project, &["rbind", "nosuid", "nodev"]));
    } else {
        mounts.push(bind(s.project, s.project, &["rbind", "ro", "rro", "nosuid", "nodev"]));
        for w in s.write { mounts.push(bind(w, w, &["rbind", "nosuid", "nodev"])); }
    }
    mounts.extend(s.custom.oci_mounts());
    let mut caps: Vec<String> = ["CAP_CHOWN", "CAP_DAC_OVERRIDE", "CAP_FOWNER", "CAP_FSETID", "CAP_KILL", "CAP_SETGID", "CAP_SETUID", "CAP_SETPCAP",
        "CAP_NET_BIND_SERVICE", "CAP_NET_ADMIN", "CAP_SYS_CHROOT", "CAP_SETFCAP"].iter().map(|x| x.to_string()).collect();
    for c in &s.custom.cap_add { if !caps.contains(c) { caps.push(c.clone()); } }
    caps.retain(|c| !s.custom.cap_drop.contains(c));
    let mut linux = json!({
        "namespaces": [{"type":"pid"},{"type":"network"},{"type":"ipc"},{"type":"uts"},{"type":"mount"}],
        "maskedPaths": ["/proc/acpi","/proc/kcore","/proc/keys","/proc/latency_stats","/proc/timer_list","/proc/timer_stats","/proc/sched_debug","/proc/scsi","/sys/firmware"],
        "readonlyPaths": ["/proc/asound","/proc/bus","/proc/fs","/proc/irq","/proc/sys","/proc/sysrq-trigger"],
        "rootfsPropagation": "private",
    });
    if s.seccomp {
        let mut deny: Vec<&str> = DENY_SYSCALLS.iter().copied().filter(|n| !s.custom.seccomp_allow.iter().any(|a| a == n)).collect();
        for d in &s.custom.seccomp_deny { if !deny.contains(&d.as_str()) { deny.push(d); } }
        linux["seccomp"] = json!({"defaultAction":"SCMP_ACT_ALLOW","syscalls":[{"names":deny,"action":"SCMP_ACT_ERRNO","errnoRet":1}]});
    }
    let mut res = json!({});
    if let Some(m) = s.mem { res["memory"] = json!({"limit": m, "swap": m}); }
    if let Some(c) = s.cpus { res["cpu"] = json!({"quota": (c * 100_000.0) as i64, "period": 100_000}); }
    if let Some(p) = s.pids { res["pids"] = json!({"limit": p}); }
    if res.as_object().is_some_and(|o| !o.is_empty()) {
        linux["resources"] = res;
        if let Some(p) = &s.cgroup_path { linux["cgroupsPath"] = json!(p); }
    }
    if !s.custom.sysctls.is_empty() { linux["sysctl"] = json!(s.custom.sysctls); }
    let mut env: Vec<String> = ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "HOME=/root", "TERM=xterm", "LANG=C.UTF-8", "TMPDIR=/tmp"].iter().map(|x| x.to_string()).collect();
    for (k, v) in &s.custom.env {
        env.retain(|e| !e.starts_with(&format!("{k}=")));
        env.push(format!("{k}={v}"));
    }
    let mut rlimits = vec![("nofile".to_string(), (4096, 4096)), ("core".to_string(), (0, 0))];
    for (k, v) in &s.custom.ulimits { rlimits.retain(|(n, _)| n != k); rlimits.push((k.clone(), *v)); }
    let rlimits: Vec<Value> = rlimits.into_iter().map(|(n, (soft, hard))| json!({"type": format!("RLIMIT_{}", n.to_uppercase()), "hard": hard, "soft": soft})).collect();
    let (uid, gid) = s.custom.user.unwrap_or((0, 0));
    let mut cfg = json!({
        "ociVersion": "1.0.2",
        "process": {
            "terminal": false,
            "user": {"uid": uid, "gid": gid},
            "args": ["/zcode-guest", "--connect", "/run/zcode/g.sock", "--cwd", s.project],
            "env": env,
            "cwd": s.project,
            "noNewPrivileges": true,
            "capabilities": {"bounding": caps, "effective": caps, "permitted": caps, "inheritable": [], "ambient": []},
            "rlimits": rlimits,
        },
        "root": {"path": s.rootfs, "readonly": s.readonly_rootfs},
        "hostname": s.custom.hostname.clone().unwrap_or_else(|| format!("zc-{}", s.agent)),
        "mounts": mounts,
        "linux": linux,
    });
    if let Some(p) = &s.custom.oci_patch { merge_patch(&mut cfg, p); }
    cfg
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn project_ro_then_rw_in_order() {
        let w = [PathBuf::from("/p/out")];
        let s = Spec { rootfs: Path::new("/r"), project: Path::new("/p"), project_rw: false, write: &w, guest: Path::new("/g"), sockdir: Path::new("/s"), resolv: None,
            agent: "a", net: false, mem: Some(1 << 30), cpus: Some(2.0), pids: None, cgroup_path: Some("x/y".into()), seccomp: true, readonly_rootfs: false, custom: &Custom::default() };
        let c = config(&s);
        let m = c["mounts"].as_array().unwrap();
        let ro = m.iter().position(|x| x["destination"] == "/p").unwrap();
        let rw = m.iter().position(|x| x["destination"] == "/p/out").unwrap();
        assert!(ro < rw && m[ro]["options"].to_string().contains("rro") && !m[rw]["options"].to_string().contains("\"ro\""));
        assert_eq!(c["linux"]["resources"]["memory"]["limit"], 1u64 << 30);
        assert!(c["linux"]["seccomp"].is_object() && c["process"]["noNewPrivileges"] == true);
        // no host env or home leaks into the container config
        assert!(!c.to_string().contains("API_KEY"));
    }
}
