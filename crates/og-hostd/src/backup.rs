//! Backups of containers and stacks into a datastore, restores, and scheduled backup jobs.
//!
//! A container backup holds its named volumes (and, if asked, its bind mounts) plus its
//! settings; a stack backup also holds the compose project directory (the compose file and
//! the data under it, i.e. the usual `./data` binds). Modes: `pause` freezes the containers
//! while their data is read (consistent, no restart; the default), `stop` stops them and
//! starts them again, `live` reads while they run. Images aren't backed up: they come back
//! from the registry (or are rebuilt).
//!
//! A restore puts the data back in place (the containers are stopped, then started again),
//! or unpacks the archives into a directory to pick files from.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::containers::Meta;
use crate::serve::Daemon;
use crate::tasks::Task;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "name", rename_all = "lowercase")]
pub enum Target {
    Container(String),
    Stack(String),
    Vm(String),
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::Container(n) => format!("ct/{n}"),
            Target::Stack(n) => format!("stack/{n}"),
            Target::Vm(n) => format!("vm/{n}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Opts {
    /// pause | stop | live
    pub mode: String,
    pub include_binds: bool,
    pub notes: String,
}

impl Default for Opts {
    fn default() -> Self {
        Opts { mode: "pause".into(), include_binds: false, notes: String::new() }
    }
}

fn host_name() -> String {
    std::fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "og-host".into())
}

/// Keeps containers quiet while their data is read; undoes it when dropped.
struct Quiet<'a> {
    d: &'a Daemon,
    task: &'a Task,
    frozen: Vec<PathBuf>,
    stopped: Vec<String>,
}

impl<'a> Quiet<'a> {
    fn new(d: &'a Daemon, task: &'a Task, ids: &[String], mode: &str) -> Result<Quiet<'a>> {
        let mut q = Quiet { d, task, frozen: vec![], stopped: vec![] };
        let running: Vec<&String> = ids.iter().filter(|id| d.node.status(id).state == "running").collect();
        match mode {
            "live" => task.log("live mode: reading while the containers run"),
            "stop" => {
                for id in running {
                    task.log(format!("stopping {}", short(d, id)));
                    d.node.stop(id, 30)?;
                    q.stopped.push(id.clone());
                }
            }
            "pause" => {
                for id in running {
                    let Some(cg) = crate::resources::cgroup_of(&d.node, id) else {
                        task.warn(format!("{}: cgroup not found, backing it up live", short(d, id)));
                        continue;
                    };
                    std::fs::write(cg.join("cgroup.freeze"), "1").with_context(|| format!("freezing {}", short(d, id)))?;
                    q.frozen.push(cg.clone());
                    let ok = (0..100).any(|_| {
                        let f = std::fs::read_to_string(cg.join("cgroup.events")).unwrap_or_default().lines().any(|l| l == "frozen 1");
                        if !f {
                            std::thread::sleep(std::time::Duration::from_millis(100));
                        }
                        f
                    });
                    if !ok {
                        task.warn(format!("{} didn't freeze within 10 s; reading anyway", short(d, id)));
                    } else {
                        task.log(format!("paused {}", short(d, id)));
                    }
                }
            }
            m => bail!("unknown mode {m:?} (pause, stop or live)"),
        }
        Ok(q)
    }
}

impl Drop for Quiet<'_> {
    fn drop(&mut self) {
        let any = !self.frozen.is_empty() || !self.stopped.is_empty();
        for cg in self.frozen.drain(..) {
            if let Err(e) = std::fs::write(cg.join("cgroup.freeze"), "0") {
                self.task.warn(format!("thawing {}: {e}", cg.display()));
            }
        }
        if any {
            self.task.log("resumed");
        }
        // start again in creation order
        for id in self.stopped.drain(..).rev() {
            let _ = self.d.node.set_desired(&id, "running");
            match self.d.node.start(&id) {
                Ok(()) => self.task.log(format!("started {}", short(self.d, &id))),
                Err(e) => self.task.warn(format!("starting {} again: {e:#}", short(self.d, &id))),
            }
        }
    }
}

fn short(d: &Daemon, id: &str) -> String {
    d.node.find(id).map(|m| m.name).unwrap_or_else(|_| id.chars().take(12).collect())
}

fn vol_name(dir: &Path) -> String {
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn safe_archive(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || "_.-".contains(c) { c } else { '_' }).take(90).collect()
}

fn human(b: u64) -> String {
    let u = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < u.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    format!("{v:.1} {}", u[i])
}

/// Back up one container or stack. Blocking; run it in a task.
/// None: there was nothing to back up (a container without volumes).
pub fn run(d: &Daemon, task: &Task, store: &str, target: &Target, opts: &Opts) -> Result<Option<og_backup::Manifest>> {
    if let Target::Vm(name) = target {
        return run_vm(d, task, store, name, opts).map(Some);
    }
    let t0 = std::time::Instant::now();
    let (_s, repo) = d.storage.repo(store)?;
    let all = d.node.list()?;
    let (group, metas, project): (String, Vec<Meta>, Option<(PathBuf, PathBuf)>) = match target {
        Target::Container(name) => {
            let m = d.node.find(name)?;
            (format!("ct/{}", m.name), vec![m], None)
        }
        Target::Vm(_) => unreachable!(),
        Target::Stack(name) => {
            let file = crate::stacks::file_of(&d.node, &d.root, name).with_context(|| format!("no compose file known for stack {name}"))?;
            let dir = file.parent().context("compose file has no directory")?.to_path_buf();
            if dir == Path::new("/") || dir == Path::new("/home") || dir == Path::new("/root") {
                bail!("the stack's directory {} is too broad to back up", dir.display());
            }
            let mut ms: Vec<Meta> = all.iter().filter(|(m, _)| m.project.as_deref() == Some(name.as_str())).map(|(m, _)| m.clone()).collect();
            ms.reverse(); // creation order
            (format!("stack/{name}"), ms, Some((file, dir)))
        }
    };
    let now = crate::containers::unix_now();
    let mut snap = repo.begin(&group, now)?;
    task.log(format!("backing up {group} to datastore {store}{}", repo.key_fingerprint().map(|f| format!(" (encrypted, key {f})")).unwrap_or_default()));
    let ids: Vec<String> = metas.iter().map(|m| m.id.clone()).collect();
    let mut vols: Vec<Value> = vec![];
    let mut binds: Vec<Value> = vec![];
    let mut seen = std::collections::HashSet::new();
    {
        let _quiet = Quiet::new(d, task, &ids, &opts.mode)?;
        let mut add = |name: &str, src: &Path, prefix: &str, exclude: &[String]| -> Result<og_backup::ArchiveInfo> {
            task.check()?;
            let w = snap.archive(name)?;
            let w = og_backup::tar_dir(w, src, prefix, exclude).with_context(|| format!("reading {}", src.display()))?;
            let a = w.finish()?;
            task.log(format!("  {name}: {} ({} new in {} chunks)", human(a.size), human(a.new_bytes), a.new_chunks));
            Ok(a)
        };
        if let Some((_, dir)) = &project {
            // the project directory, minus volume dirs that happen to live under it
            add("project", dir, "project", &[])?;
        }
        for m in &metas {
            for (dir, target, _) in &m.volumes {
                if !seen.insert(dir.clone()) || !dir.exists() {
                    continue;
                }
                let v = vol_name(dir);
                let arch = format!("vol-{}", safe_archive(&v));
                add(&arch, dir, &v, &[])?;
                vols.push(json!({"archive": arch, "dir": dir, "target": target, "container": m.name}));
            }
            for (i, (src, target, _)) in m.binds.iter().enumerate() {
                let under_project = project.as_ref().is_some_and(|(_, p)| src.starts_with(p));
                if under_project || !seen.insert(src.clone()) {
                    continue;
                }
                if !opts.include_binds {
                    binds.push(json!({"source": src, "target": target, "container": m.name, "skipped": true}));
                    continue;
                }
                if !src.exists() {
                    continue;
                }
                let arch = format!("bind-{}-{i}", safe_archive(&m.name));
                add(&arch, src, "bind", &[])?;
                binds.push(json!({"archive": arch, "source": src, "target": target, "container": m.name}));
            }
        }
    }
    if snap.is_empty() {
        task.log(format!("{group}: no volumes or data to back up (settings and image come back on their own); skipped"));
        return Ok(None);
    }
    for b in binds.iter().filter(|b| b["skipped"] == true) {
        task.log(format!("  not included (bind mount outside the stack): {}", b["source"].as_str().unwrap_or("")));
    }
    let meta = json!({
        "kind": match target { Target::Container(_) => "container", Target::Stack(_) => "stack", Target::Vm(_) => "vm" },
        "name": match target { Target::Container(n) | Target::Stack(n) | Target::Vm(n) => n },
        "mode": opts.mode,
        "containers": metas.iter().map(|m| json!({"name": m.name, "id": m.id, "image": m.image, "service": m.service, "meta": m})).collect::<Vec<_>>(),
        "volumes": vols,
        "binds": binds,
        "compose_file": project.as_ref().map(|p| p.0.clone()),
        "project_dir": project.as_ref().map(|p| p.1.clone()),
    });
    let m = snap.commit(&host_name(), meta, &opts.notes, t0.elapsed().as_millis() as u64)?;
    let new: u64 = m.archives.iter().map(|a| a.new_bytes).sum();
    task.log(format!("done: {} read, {} stored, {:.1} s", human(m.size()), human(new), t0.elapsed().as_secs_f64()));
    Ok(Some(m))
}

/// A VM: its whole directory (settings, UEFI variables, TPM state, disks) as one archive.
/// `pause` stops its CPUs while the disks are read; `stop` shuts it down and starts it again.
fn run_vm(d: &Daemon, task: &Task, store: &str, name: &str, opts: &Opts) -> Result<og_backup::Manifest> {
    let t0 = std::time::Instant::now();
    let vm = d.vms.get(name)?;
    let (_s, repo) = d.storage.repo(store)?;
    let group = format!("vm/{name}");
    let outside: Vec<String> = vm.disks.iter().filter(|x| x.file.starts_with('/')).map(|x| x.file.clone()).collect();
    if !outside.is_empty() {
        task.warn(format!("disks outside the VM's folder aren't included: {}", outside.join(", ")));
    }
    let mut snap = repo.begin(&group, crate::containers::unix_now())?;
    task.log(format!("backing up {group} to datastore {store}{}", repo.key_fingerprint().map(|f| format!(" (encrypted, key {f})")).unwrap_or_default()));
    let state = d.vms.status(name).state;
    let mut resume = false;
    let mut restart = false;
    if state == "running" {
        match opts.mode.as_str() {
            "pause" => {
                d.vms.qmp(name, "stop", json!({}))?;
                resume = true;
                task.log("paused the VM");
            }
            "stop" => {
                task.log("shutting the VM down");
                crate::vmrun::stop(d, name, false, 180)?;
                restart = true;
            }
            _ => task.log("live: reading the disks while the VM runs (it may not boot cleanly from this backup)"),
        }
    }
    let r = (|| -> Result<og_backup::ArchiveInfo> {
        let w = snap.archive("vm")?;
        let w = og_backup::tar_dir(w, &d.vms.dir(name), "vm", &["console.log".into(), "swtpm.log".into()])?;
        Ok(w.finish()?)
    })();
    if resume {
        match d.vms.qmp(name, "cont", json!({})) {
            Ok(_) => task.log("resumed"),
            Err(e) => task.warn(format!("resuming the VM: {e:#}")),
        }
    }
    if restart {
        match crate::vmrun::start(d, name) {
            Ok(()) => task.log("started the VM again"),
            Err(e) => task.warn(format!("starting the VM again: {e:#}")),
        }
    }
    let a = r?;
    task.log(format!("  vm: {} ({} new in {} chunks)", human(a.size), human(a.new_bytes), a.new_chunks));
    let meta = json!({"kind": "vm", "name": name, "mode": opts.mode, "vm": vm, "state": state});
    let m = snap.commit(&host_name(), meta, &opts.notes, t0.elapsed().as_millis() as u64)?;
    task.log(format!("done: {} read, {} stored, {:.1} s", human(m.size()), human(a.new_bytes), t0.elapsed().as_secs_f64()));
    Ok(m)
}

/// Put a directory archive back at `dest` (replacing what's there), via a temporary
/// directory next to it so a failed restore leaves the current data alone.
fn restore_dir(repo: &og_backup::Repo, m: &og_backup::Manifest, archive: &str, prefix: &str, dest: &Path, task: &Task) -> Result<()> {
    task.check()?;
    let parent = dest.parent().context("bad destination")?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".og-restore-{}-{}", vol_name(dest), std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    let n = og_backup::untar(repo.reader(m, archive)?, &tmp).with_context(|| format!("unpacking {archive}"))?;
    let unpacked = tmp.join(prefix);
    if !unpacked.exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        bail!("{archive} doesn't hold {prefix}");
    }
    let old = parent.join(format!(".og-pre-restore-{}-{}", vol_name(dest), crate::containers::unix_now()));
    if std::fs::symlink_metadata(dest).is_ok() {
        std::fs::rename(dest, &old).with_context(|| format!("moving {} aside", dest.display()))?;
    }
    if let Err(e) = std::fs::rename(&unpacked, dest) {
        let _ = std::fs::rename(&old, dest);
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e).with_context(|| format!("putting {} in place", dest.display()));
    }
    let _ = std::fs::remove_dir_all(&tmp);
    if old.exists() {
        if old.is_dir() { std::fs::remove_dir_all(&old) } else { std::fs::remove_file(&old) }.with_context(|| format!("removing the replaced {}", old.display()))?;
    }
    task.log(format!("  {archive} -> {} ({n} entries)", dest.display()));
    Ok(())
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RestoreOpts {
    /// unpack the archives into this directory instead of putting them back
    pub to: Option<PathBuf>,
    /// only these archives (default: all)
    pub archives: Vec<String>,
    /// start the containers afterwards even if they weren't running
    pub start: bool,
}

pub fn restore(d: &Daemon, task: &Task, store: &str, snapshot: &str, o: &RestoreOpts) -> Result<()> {
    let (_s, repo) = d.storage.repo(store)?;
    let m = repo.snapshot(snapshot)?;
    let pick = |a: &str| o.archives.is_empty() || o.archives.iter().any(|x| x == a);
    if let Some(to) = &o.to {
        if !to.is_absolute() {
            bail!("give an absolute directory to unpack into");
        }
        for a in m.archives.iter().filter(|a| pick(&a.name)) {
            task.check()?;
            let n = og_backup::untar(repo.reader(&m, &a.name)?, &to.join(&a.name))?;
            task.log(format!("  {} -> {} ({n} entries)", a.name, to.join(&a.name).display()));
        }
        return Ok(());
    }
    let kind = m.meta["kind"].as_str().unwrap_or("");
    let name = m.meta["name"].as_str().unwrap_or("").to_string();
    task.log(format!("restoring {} from {}", m.group, og_backup::time_name(m.time)));
    if kind == "vm" {
        if !crate::vms::valid_name(&name) {
            bail!("bad VM name in the backup");
        }
        let was = d.vms.get(&name).is_ok() && d.vms.status(&name).state != "stopped";
        if was {
            task.log("shutting the VM down");
            crate::vmrun::stop(d, &name, false, 180)?;
        }
        std::fs::create_dir_all(&d.vms.root)?;
        restore_dir(&repo, &m, "vm", "vm", &d.vms.dir(&name), task)?;
        if was || o.start {
            crate::vmrun::start(d, &name)?;
            task.log("started the VM");
        }
        return Ok(());
    }
    // the containers that exist now, and whether they run
    let now: Vec<(Meta, bool)> = d
        .node
        .list()?
        .into_iter()
        .filter(|(c, _)| if kind == "stack" { c.project.as_deref() == Some(name.as_str()) } else { c.name == name })
        .map(|(c, st)| (c, st.state == "running"))
        .collect();
    if kind == "container" && now.is_empty() {
        bail!("container {name} doesn't exist any more: restore its stack, or unpack the backup into a directory");
    }
    for (c, running) in &now {
        if *running {
            task.log(format!("stopping {}", c.name));
            d.node.stop(&c.id, 30)?;
        }
    }
    let r = (|| -> Result<()> {
        if kind == "stack" && pick("project") {
            if let Some(dir) = m.meta["project_dir"].as_str() {
                restore_dir(&repo, &m, "project", "project", Path::new(dir), task)?;
            }
        }
        for v in m.meta["volumes"].as_array().into_iter().flatten() {
            let (Some(a), Some(dir)) = (v["archive"].as_str(), v["dir"].as_str()) else { continue };
            if pick(a) {
                let dir = Path::new(dir);
                restore_dir(&repo, &m, a, &vol_name(dir), dir, task)?;
            }
        }
        for b in m.meta["binds"].as_array().into_iter().flatten() {
            let (Some(a), Some(src)) = (b["archive"].as_str(), b["source"].as_str()) else { continue };
            if pick(a) {
                restore_dir(&repo, &m, a, "bind", Path::new(src), task)?;
            }
        }
        Ok(())
    })();
    // start again what ran before (also after a failed restore: the old data is still there)
    let started_any = now.iter().any(|(_, r)| *r) || o.start;
    if kind == "stack" && (started_any || now.is_empty()) && r.is_ok() {
        let file = m.meta["compose_file"].as_str().map(PathBuf::from).context("backup has no compose file path")?;
        task.log(format!("bringing up the stack from {}", file.display()));
        let env: og_compose::interp::Vars = std::env::vars().collect();
        let p = og_compose::load(&file, Some(&name), &env)?;
        tokio::runtime::Handle::current().block_on(crate::compose::up(&d.node, &d.root, &d.node.store, &p, false))?;
    } else {
        for (c, running) in now.iter().rev() {
            if *running || o.start {
                let _ = d.node.set_desired(&c.id, "running");
                match d.node.start(&c.id) {
                    Ok(()) => task.log(format!("started {}", c.name)),
                    Err(e) => task.warn(format!("starting {}: {e:#}", c.name)),
                }
            }
        }
    }
    r
}

// ---- jobs ----

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Job {
    pub id: String,
    pub enabled: bool,
    pub schedule: String,
    pub store: String,
    /// everything: all stacks and the containers not in one
    pub all: bool,
    pub stacks: Vec<String>,
    pub containers: Vec<String>,
    pub vms: Vec<String>,
    /// names (stack, container or VM) left out of `all`
    pub exclude: Vec<String>,
    pub opts: Opts,
    pub keep: og_backup::prune::Keep,
    /// failure | always | never (Signal, through the lab)
    pub notify: String,
    pub comment: String,
}

impl Default for Job {
    fn default() -> Self {
        Job { id: String::new(), enabled: true, schedule: "daily 02:00".into(), store: "local".into(), all: true, stacks: vec![], containers: vec![], vms: vec![], exclude: vec![], opts: Opts::default(), keep: og_backup::prune::Keep { last: 3, daily: 7, weekly: 4, monthly: 6, ..Default::default() }, notify: "failure".into(), comment: String::new() }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobState {
    pub last_run: u64,
    pub last_status: String,
    pub last_task: String,
}

fn jobs_file(root: &Path) -> PathBuf {
    root.join("backup-jobs.json")
}

pub fn jobs(root: &Path) -> Vec<Job> {
    std::fs::read(jobs_file(root)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_jobs(root: &Path, jobs: &[Job]) -> Result<()> {
    let tmp = jobs_file(root).with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(jobs)?)?;
    std::fs::rename(tmp, jobs_file(root))?;
    Ok(())
}

pub fn job_states(root: &Path) -> std::collections::BTreeMap<String, JobState> {
    std::fs::read(root.join("backup-jobs-state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn set_job_state(root: &Path, id: &str, st: JobState) {
    let mut all = job_states(root);
    all.insert(id.into(), st);
    let _ = std::fs::write(root.join("backup-jobs-state.json"), serde_json::to_vec_pretty(&all).unwrap_or_default());
}

pub fn validate_job(d: &Daemon, j: &mut Job) -> Result<()> {
    if !crate::storage::valid_id(&j.id) {
        bail!("job names are letters, digits, - and _");
    }
    crate::schedule::Schedule::parse(&j.schedule)?;
    d.storage.get(&j.store)?;
    if !["pause", "stop", "live"].contains(&j.opts.mode.as_str()) {
        bail!("mode is pause, stop or live");
    }
    if !["failure", "always", "never"].contains(&j.notify.as_str()) {
        bail!("notify is failure, always or never");
    }
    if !j.all && j.stacks.is_empty() && j.containers.is_empty() && j.vms.is_empty() {
        bail!("pick what to back up");
    }
    j.comment = j.comment.chars().take(200).collect();
    Ok(())
}

/// What a job backs up now.
pub fn job_targets(d: &Daemon, j: &Job) -> Vec<Target> {
    let mut out = vec![];
    let all = d.node.list().unwrap_or_default();
    let skip = |name: &str| j.exclude.iter().any(|e| e == name);
    if j.all {
        for s in crate::stacks::list(&d.node, &d.root) {
            if !skip(&s.name) && s.file.is_some() {
                out.push(Target::Stack(s.name));
            }
        }
        for (m, _) in &all {
            if m.project.is_none() && !skip(&m.name) && m.labels.get("og.backup").map(String::as_str) != Some("false") {
                out.push(Target::Container(m.name.clone()));
            }
        }
        for vm in d.vms.list() {
            if !skip(&vm.name) && vm.labels.get("og.backup").map(String::as_str) != Some("false") {
                out.push(Target::Vm(vm.name));
            }
        }
    } else {
        out.extend(j.stacks.iter().cloned().map(Target::Stack));
        out.extend(j.containers.iter().cloned().map(Target::Container));
        out.extend(j.vms.iter().cloned().map(Target::Vm));
    }
    out
}

/// Prune one group by a retention policy; returns the snapshots removed.
pub fn prune(repo: &og_backup::Repo, group: &str, keep: &og_backup::prune::Keep, dry_run: bool) -> Result<Vec<String>> {
    let snaps: Vec<og_backup::Manifest> = repo.snapshots().into_iter().filter(|m| m.group == group).collect();
    let plan = og_backup::prune::plan(&snaps.iter().map(|m| (m.time, m.protected)).collect::<Vec<_>>(), keep);
    let mut gone = vec![];
    for (m, k) in snaps.iter().zip(plan) {
        if !k {
            if !dry_run {
                repo.delete(&m.id(), false)?;
            }
            gone.push(m.id());
        }
    }
    Ok(gone)
}

pub fn run_job(d: &Arc<Daemon>, task: &Task, j: &Job) -> Result<()> {
    let targets = job_targets(d, j);
    task.log(format!("job {}: {} to back up", j.id, targets.len()));
    let mut failed = vec![];
    for t in &targets {
        task.check()?;
        match run(d, task, &j.store, t, &j.opts) {
            Ok(None) => {}
            Ok(Some(m)) => {
                if !j.keep.is_empty() {
                    if let Ok((_, repo)) = d.storage.repo(&j.store) {
                        match prune(&repo, &m.group, &j.keep, false) {
                            Ok(g) if !g.is_empty() => task.log(format!("  pruned {} old backup(s) of {}", g.len(), m.group)),
                            Err(e) => task.warn(format!("pruning {}: {e:#}", m.group)),
                            _ => {}
                        }
                    }
                }
            }
            Err(e) => {
                task.warn(format!("{}: {e:#}", t.label()));
                failed.push(t.label());
            }
        }
    }
    let summary = if failed.is_empty() { format!("{} backed up", targets.len()) } else { format!("{} of {} failed: {}", failed.len(), targets.len(), failed.join(", ")) };
    task.log(format!("job {}: {summary}", j.id));
    if j.notify == "always" || (j.notify == "failure" && !failed.is_empty()) {
        if let Some(lab) = d.lab.clone() {
            let text = format!("OG-Host {}: backup job {} — {summary}", host_name(), j.id);
            tokio::runtime::Handle::current().spawn(async move {
                let _ = og_lab::notify::send_signal(&lab, &text).await;
            });
        }
    }
    if !failed.is_empty() && failed.len() == targets.len() {
        bail!("{summary}");
    }
    Ok(())
}

pub fn start_job(d: &Arc<Daemon>, j: Job, user: &str) -> Result<String> {
    if d.tasks.busy("backup-job", &j.id) {
        bail!("job {} is running already", j.id);
    }
    let d2 = d.clone();
    let root = d.root.clone();
    let id = j.id.clone();
    let jid = j.id.clone();
    let tid = d.tasks.spawn("backup-job", &jid, user, move |task| {
        let r = run_job(&d2, task, &j);
        let tid = task.info.lock().unwrap().id.clone();
        set_job_state(&root, &id, JobState { last_run: crate::containers::unix_now(), last_status: match &r { Ok(()) => "ok".into(), Err(e) => format!("error: {e:#}") }, last_task: tid });
        r
    })?;
    Ok(tid)
}

/// Runs jobs and datastore garbage collection on their schedules (checked every 20 s).
pub async fn scheduler(d: Arc<Daemon>) {
    let mut last = crate::containers::unix_now();
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(20)).await;
        let now = crate::containers::unix_now();
        let due = |sched: &str| crate::schedule::Schedule::parse(sched).ok().and_then(|s| s.next_after(last)).is_some_and(|n| n <= now);
        for j in jobs(&d.root).into_iter().filter(|j| j.enabled) {
            if due(&j.schedule) {
                if let Err(e) = start_job(&d, j.clone(), "system") {
                    eprintln!("og-hostd: backup job {}: {e:#}", j.id);
                }
            }
        }
        if due(&crate::update::config(&d.root).schedule) {
            tokio::spawn(crate::update::auto(d.clone()));
        }
        for j in crate::cluster::sync_jobs_list(&d.root).into_iter().filter(|j| j.enabled) {
            if due(&j.schedule) {
                if let Err(e) = crate::cluster::start_sync(&d, j.clone(), "system") {
                    eprintln!("og-hostd: sync job {}: {e:#}", j.id);
                }
            }
        }
        for s in d.storage.list().into_iter().filter(|s| s.enabled && !s.gc_schedule.is_empty()) {
            if due(&s.gc_schedule) && !d.tasks.busy("gc", &s.id) {
                let d2 = d.clone();
                let id = s.id.clone();
                let _ = d.tasks.spawn("gc", &s.id, "system", move |task| gc(&d2, task, &id));
            }
        }
        last = now;
    }
}

pub fn gc(d: &Daemon, task: &Task, store: &str) -> Result<()> {
    let (_, repo) = d.storage.repo(store)?;
    task.log(format!("garbage collection on {store}"));
    let r = repo.gc()?;
    task.log(format!("{} backups, {} chunks in use ({}), removed {} chunks ({}), {} too new to remove", r.snapshots, r.chunks, human(r.bytes), r.removed_chunks, human(r.removed_bytes), r.pending));
    if r.bytes > 0 {
        task.log(format!("deduplication factor {:.2} ({} backed up in {})", r.logical as f64 / r.bytes as f64, human(r.logical), human(r.bytes)));
    }
    Ok(())
}
