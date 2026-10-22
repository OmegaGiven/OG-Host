//! Compose stacks for the UI: managed ones live in `<root>/stacks/<name>/compose.yaml`;
//! stacks started elsewhere (`og-hostd up` in some directory) are found through their
//! containers' compose labels. Edits keep a backup and are rolled back if the file no longer
//! parses.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Serialize;

use crate::containers::Node;

#[derive(Debug, Serialize)]
pub struct Service {
    pub name: String,
    pub container: Option<String>,
    pub container_name: Option<String>,
    pub state: String,
    pub image: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Stack {
    pub name: String,
    pub managed: bool,
    pub file: Option<PathBuf>,
    pub services: Vec<Service>,
    pub running: usize,
}

pub fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 63 && n.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_') && !n.starts_with('-')
}

fn managed_dir(root: &Path) -> PathBuf {
    root.join("stacks")
}

fn managed_file(root: &Path, name: &str) -> PathBuf {
    managed_dir(root).join(name).join("compose.yaml")
}

/// Where a stack's compose file is: managed, or what its containers say.
pub fn file_of(node: &Node, root: &Path, name: &str) -> Option<PathBuf> {
    let m = managed_file(root, name);
    if m.exists() {
        return Some(m);
    }
    node.list().ok()?.into_iter().find_map(|(m, _)| {
        (m.project.as_deref() == Some(name)).then(|| m.labels.get("com.docker.compose.project.config_files").map(PathBuf::from)).flatten()
    }).filter(|p| is_compose_file(p))
}

/// Labels can come from any Docker API client, so a discovered path is only trusted when it
/// is an absolute path to a regular file with a compose file name.
fn is_compose_file(p: &Path) -> bool {
    const NAMES: &[&str] = &["compose.yaml", "compose.yml", "docker-compose.yaml", "docker-compose.yml"];
    p.is_absolute()
        && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| NAMES.contains(&n))
        && std::fs::symlink_metadata(p).is_ok_and(|m| m.is_file())
}

pub fn list(node: &Node, root: &Path) -> Vec<Stack> {
    let mut stacks: BTreeMap<String, Stack> = BTreeMap::new();
    if let Ok(rd) = std::fs::read_dir(managed_dir(root)) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            let file = managed_file(root, &name);
            if !valid_name(&name) || !file.exists() {
                continue;
            }
            // services as declared (shown even before the first `up`)
            let services = og_compose::load(&file, Some(&name), &Default::default())
                .map(|p| p.services.iter().map(|s| Service { name: s.name.clone(), container: None, container_name: None, state: "not created".into(), image: s.image.clone() }).collect())
                .unwrap_or_default();
            stacks.insert(name.clone(), Stack { name, managed: true, file: Some(file), services, running: 0 });
        }
    }
    for (m, st) in node.list().unwrap_or_default() {
        let Some(project) = m.project.clone() else { continue };
        let stack = stacks.entry(project.clone()).or_insert_with(|| Stack {
            name: project.clone(),
            managed: false,
            file: m.labels.get("com.docker.compose.project.config_files").map(PathBuf::from).filter(|p| is_compose_file(p)),
            services: vec![],
            running: 0,
        });
        let svc_name = m.service.clone().unwrap_or_else(|| m.name.clone());
        let svc = match stack.services.iter_mut().find(|s| s.name == svc_name && s.container.is_none()) {
            Some(s) => s,
            None => {
                stack.services.push(Service { name: svc_name, container: None, container_name: None, state: String::new(), image: None });
                stack.services.last_mut().unwrap()
            }
        };
        svc.container = Some(m.id.clone());
        svc.container_name = Some(m.name.clone());
        svc.state = st.state.clone();
        svc.image = Some(m.image.clone());
    }
    for s in stacks.values_mut() {
        s.running = s.services.iter().filter(|x| x.state == "running").count();
    }
    stacks.into_values().collect()
}

pub fn read_file(node: &Node, root: &Path, name: &str) -> Result<(PathBuf, String)> {
    let f = file_of(node, root, name).context("no compose file known for this stack")?;
    Ok((f.clone(), std::fs::read_to_string(&f)?))
}

/// Write a stack's compose file; a new managed stack when it doesn't exist yet.
pub fn write_file(node: &Node, root: &Path, name: &str, content: &str) -> Result<PathBuf> {
    if !valid_name(name) {
        bail!("stack names are lowercase letters, digits, - and _");
    }
    if content.len() > 1 << 20 {
        bail!("compose file too large");
    }
    let f = file_of(node, root, name).unwrap_or_else(|| managed_file(root, name));
    std::fs::create_dir_all(f.parent().unwrap())?;
    let backup = f.with_extension("yaml.bak");
    let had = f.exists();
    if had {
        std::fs::copy(&f, &backup)?;
    }
    std::fs::write(&f, content)?;
    if let Err(e) = og_compose::load(&f, Some(name), &Default::default()) {
        if had {
            std::fs::copy(&backup, &f)?;
        } else {
            let _ = std::fs::remove_file(&f);
            let _ = std::fs::remove_dir(f.parent().unwrap());
        }
        bail!("not saved, the compose file doesn't parse: {e}");
    }
    Ok(f)
}

pub fn load(node: &Node, root: &Path, name: &str) -> Result<og_compose::Project> {
    let f = file_of(node, root, name).context("no compose file known for this stack")?;
    let env: og_compose::interp::Vars = std::env::vars().collect();
    Ok(og_compose::load(&f, Some(name), &env)?)
}

/// Remove a managed stack's directory (after `down`).
pub fn delete_managed(root: &Path, name: &str) -> Result<()> {
    if !valid_name(name) {
        bail!("bad stack name");
    }
    let d = managed_dir(root).join(name);
    if !d.join("compose.yaml").exists() {
        bail!("only stacks created here can be deleted here");
    }
    std::fs::remove_dir_all(d)?;
    Ok(())
}
