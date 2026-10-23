//! Container Device Interface (CDI): the vendor-neutral way GPUs (and other devices) are given
//! to containers. Vendors ship specs (`nvidia-ctk cdi generate` -> /etc/cdi/nvidia.yaml) that
//! list the device nodes, driver library mounts, environment and hooks a container needs.
//! We resolve fully-qualified names like `nvidia.com/gpu=0` or `nvidia.com/gpu=all` and turn
//! the edits into run options.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Error, Result};

pub const SPEC_DIRS: &[&str] = &["/etc/cdi", "/var/run/cdi"];

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerEdits {
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default)]
    pub device_nodes: Vec<DeviceNode>,
    #[serde(default)]
    pub mounts: Vec<CdiMount>,
    #[serde(default)]
    pub hooks: Vec<Hook>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceNode {
    pub path: String,
    #[serde(default)]
    pub host_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CdiMount {
    pub host_path: String,
    pub container_path: String,
    #[serde(default)]
    pub options: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Hook {
    pub hook_name: String,
    pub path: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Device {
    name: String,
    #[serde(default)]
    container_edits: ContainerEdits,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Spec {
    kind: String,
    #[serde(default)]
    devices: Vec<Device>,
    #[serde(default)]
    container_edits: ContainerEdits,
}

/// All CDI specs found on this host, by kind (e.g. "nvidia.com/gpu").
#[derive(Debug, Default)]
pub struct Registry {
    specs: BTreeMap<String, Spec>,
}

impl Registry {
    pub fn load() -> Self {
        Self::load_from(SPEC_DIRS.iter().map(PathBuf::from).collect())
    }

    pub fn load_from(dirs: Vec<PathBuf>) -> Self {
        let mut specs = BTreeMap::new();
        for d in dirs {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
            files.sort();
            for f in files {
                let text = match std::fs::read_to_string(&f) {
                    Ok(t) => t,
                    Err(_) => continue,
                };
                let parsed: Option<Spec> = match f.extension().and_then(|e| e.to_str()) {
                    Some("json") => serde_json::from_str(&text).map_err(|e| e.to_string()),
                    Some("yaml") | Some("yml") => serde_yaml::from_str(&text).map_err(|e| e.to_string()),
                    _ => continue,
                }
                .map_err(|e| eprintln!("og-runtime: ignoring CDI spec {}: {e}", f.display()))
                .ok();
                if let Some(s) = parsed {
                    specs.insert(s.kind.clone(), s); // later dirs (/var/run/cdi) override
                }
            }
        }
        Registry { specs }
    }

    pub fn kinds(&self) -> Vec<String> {
        self.specs.keys().cloned().collect()
    }

    pub fn device_names(&self, kind: &str) -> Vec<String> {
        self.specs.get(kind).map(|s| s.devices.iter().map(|d| d.name.clone()).collect()).unwrap_or_default()
    }

    /// Resolve `vendor.com/class=name` into the combined edits (spec-wide edits included once).
    pub fn resolve(&self, qualified: &[String]) -> Result<ContainerEdits> {
        let mut out = ContainerEdits::default();
        let mut kinds_used = Vec::new();
        for q in qualified {
            let (kind, name) = q.split_once('=').ok_or_else(|| Error::Invalid(format!("CDI device {q:?} must look like vendor.com/class=name")))?;
            let spec = self.specs.get(kind).ok_or_else(|| Error::Invalid(format!("no CDI spec for {kind} (for NVIDIA: nvidia-ctk cdi generate --output=/etc/cdi/nvidia.yaml)")))?;
            let dev = spec.devices.iter().find(|d| d.name == name).ok_or_else(|| {
                Error::Invalid(format!("CDI device {q} not found; {kind} has: {}", spec.devices.iter().map(|d| d.name.as_str()).collect::<Vec<_>>().join(", ")))
            })?;
            merge(&mut out, &dev.container_edits);
            if !kinds_used.contains(&kind.to_string()) {
                kinds_used.push(kind.to_string());
                merge(&mut out, &spec.container_edits);
            }
        }
        Ok(out)
    }
}

fn merge(into: &mut ContainerEdits, from: &ContainerEdits) {
    for e in &from.env {
        if !into.env.contains(e) {
            into.env.push(e.clone());
        }
    }
    for d in &from.device_nodes {
        if !into.device_nodes.iter().any(|x| x.path == d.path) {
            into.device_nodes.push(d.clone());
        }
    }
    for m in &from.mounts {
        if !into.mounts.iter().any(|x| x.container_path == m.container_path) {
            into.mounts.push(m.clone());
        }
    }
    for h in &from.hooks {
        into.hooks.push(h.clone());
    }
}

impl ContainerEdits {
    /// Host device paths (CDI's hostPath, else the container path).
    pub fn device_paths(&self) -> Vec<PathBuf> {
        self.device_nodes.iter().map(|d| PathBuf::from(d.host_path.as_deref().unwrap_or(&d.path))).collect()
    }

    pub fn bind_mounts(&self) -> Vec<crate::BindMount> {
        self.mounts
            .iter()
            .filter(|m| Path::new(&m.host_path).exists())
            .map(|m| crate::BindMount { source: PathBuf::from(&m.host_path), target: PathBuf::from(&m.container_path), read_only: m.options.iter().any(|o| o == "ro") })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NVIDIA: &str = r#"
cdiVersion: 0.5.0
kind: nvidia.com/gpu
devices:
- name: "0"
  containerEdits:
    deviceNodes:
    - path: /dev/nvidia0
- name: all
  containerEdits:
    deviceNodes:
    - path: /dev/nvidia0
containerEdits:
  env:
  - NVIDIA_VISIBLE_DEVICES=void
  deviceNodes:
  - path: /dev/nvidiactl
  - path: /dev/nvidia-uvm
  mounts:
  - hostPath: /usr/lib/x86_64-linux-gnu/libcuda.so.580.95
    containerPath: /usr/lib/x86_64-linux-gnu/libcuda.so.580.95
    options: [ro, nosuid, nodev, bind]
  hooks:
  - hookName: createContainer
    path: /usr/bin/nvidia-cdi-hook
    args: [nvidia-cdi-hook, update-ldcache, --folder, /usr/lib/x86_64-linux-gnu]
"#;

    #[test]
    fn resolves_nvidia_spec() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("nvidia.yaml"), NVIDIA).unwrap();
        let r = Registry::load_from(vec![d.path().to_path_buf()]);
        assert_eq!(r.kinds(), vec!["nvidia.com/gpu"]);
        assert_eq!(r.device_names("nvidia.com/gpu"), vec!["0", "all"]);
        let e = r.resolve(&["nvidia.com/gpu=all".into()]).unwrap();
        let paths: Vec<_> = e.device_nodes.iter().map(|n| n.path.as_str()).collect();
        assert_eq!(paths, ["/dev/nvidia0", "/dev/nvidiactl", "/dev/nvidia-uvm"]);
        assert_eq!(e.mounts.len(), 1);
        assert_eq!(e.hooks[0].hook_name, "createContainer");
        assert!(r.resolve(&["nvidia.com/gpu=7".into()]).unwrap_err().to_string().contains("has: 0, all"));
        assert!(r.resolve(&["amd.com/gpu=0".into()]).is_err());
        assert!(r.resolve(&["bogus".into()]).is_err());
    }
}

