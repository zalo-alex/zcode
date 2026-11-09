//! Per-agent root filesystem: a writable layer over a shared read-only base image.
//! The mechanism sits behind `RootfsProvider` so it can be swapped; `pick` chooses the cheapest available one.
use super::probe::Probe;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// What the namespace supervisor (or the host, for `Copy`) must do before the OCI runtime starts.
pub enum Plan {
    /// kernel overlayfs, mounted by the supervisor inside its user+mount namespace
    Overlay { lower: PathBuf, upper: PathBuf, work: PathBuf, merged: PathBuf },
    /// `fuse-overlayfs`, started by the supervisor inside its namespace
    Fuse { lower: PathBuf, upper: PathBuf, work: PathBuf, merged: PathBuf },
    /// private copy of the base (reflink when the filesystem supports it); slow start
    Copy { lower: PathBuf, rootfs: PathBuf },
}

impl Plan {
    /// path the OCI `root.path` must point at
    pub fn rootfs(&self) -> &Path {
        match self { Plan::Overlay { merged, .. } | Plan::Fuse { merged, .. } => merged, Plan::Copy { rootfs, .. } => rootfs }
    }
    pub fn to_json(&self) -> Value {
        match self {
            Plan::Overlay { lower, upper, work, merged } => json!({"kind":"overlay","lower":lower,"upper":upper,"work":work,"merged":merged}),
            Plan::Fuse { lower, upper, work, merged } => json!({"kind":"fuse","lower":lower,"upper":upper,"work":work,"merged":merged}),
            Plan::Copy { .. } => json!({"kind":"copy"}),
        }
    }
    /// work the host does itself (only `Copy`)
    pub fn prepare_host(&self) -> Result<(), String> {
        if let Plan::Copy { lower, rootfs } = self {
            std::fs::create_dir_all(rootfs).map_err(|e| e.to_string())?;
            let ok = std::process::Command::new("cp").args(["-a", "--reflink=auto"]).arg(format!("{}/.", lower.display())).arg(rootfs).status().map(|s| s.success()).unwrap_or(false);
            if !ok { return Err("copying the image failed".into()); }
        }
        Ok(())
    }
}

pub trait RootfsProvider: Send + Sync {
    fn name(&self) -> &'static str;
    fn available(&self, p: &Probe) -> bool;
    fn plan(&self, lower: &Path, state: &Path) -> Plan;
}

fn dirs(state: &Path) -> (PathBuf, PathBuf, PathBuf) { (state.join("upper"), state.join("work"), state.join("merged")) }

pub struct KernelOverlay;
impl RootfsProvider for KernelOverlay {
    fn name(&self) -> &'static str { "overlay" }
    fn available(&self, p: &Probe) -> bool { p.overlay }
    fn plan(&self, lower: &Path, state: &Path) -> Plan { let (upper, work, merged) = dirs(state); Plan::Overlay { lower: lower.into(), upper, work, merged } }
}

pub struct FuseOverlay;
impl RootfsProvider for FuseOverlay {
    fn name(&self) -> &'static str { "fuse" }
    fn available(&self, p: &Probe) -> bool { p.fuse_overlayfs.is_some() }
    fn plan(&self, lower: &Path, state: &Path) -> Plan { let (upper, work, merged) = dirs(state); Plan::Fuse { lower: lower.into(), upper, work, merged } }
}

pub struct CopyBase;
impl RootfsProvider for CopyBase {
    fn name(&self) -> &'static str { "copy" }
    fn available(&self, _: &Probe) -> bool { true }
    fn plan(&self, lower: &Path, state: &Path) -> Plan { Plan::Copy { lower: lower.into(), rootfs: state.join("rootfs") } }
}

/// `preferred` is config `container.rootfs`: auto | overlay | fuse | copy. Auto order: kernel overlayfs, fuse-overlayfs, copy.
pub fn pick(preferred: &str, p: &Probe) -> Box<dyn RootfsProvider> {
    let all: [Box<dyn RootfsProvider>; 3] = [Box::new(KernelOverlay), Box::new(FuseOverlay), Box::new(CopyBase)];
    for prov in all {
        if (preferred == "auto" || preferred == prov.name()) && prov.available(p) { return prov; }
    }
    Box::new(CopyBase)
}
