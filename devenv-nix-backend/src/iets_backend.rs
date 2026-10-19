//! `backend: iets` without libnix: evaluation, locking, builds, GC roots and
//! collection go through iets, linked as a library, and the Nix daemon it
//! speaks to. Nothing here calls into the C++ Nix libraries, so a run under
//! this backend never initialises them.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use devenv_activity::{Activity, ActivityInstrument, activity};
use devenv_core::bootstrap_args::BootstrapArgs;
use devenv_core::evaluator::{BuildOptions, DevEnvOutput, Evaluator};
use devenv_core::store::{
    GcOptions, GcStats, PathInfo as CorePathInfo, Store as StoreTrait, StorePath as CoreStorePath,
};
use devenv_core::realized::RealizedPathsObserver;
use devenv_core::{DevenvPaths, PortAllocator, StoreSettings};
use iets::api::store::{GcAction, OnEvent, Store};
use miette::{IntoDiagnostic, Result, WrapErr, miette};

use crate::build_environment::BuildEnvironment as RustBuildEnvironment;
use crate::cachix_host::CachixHost;
use crate::iets_shell::{IetsEvaluator, LockUpdate, lock_inputs};

/// The Nix daemon, as iets reaches it.
pub struct IetsStore {
    store: Arc<Store>,
}

impl IetsStore {
    pub fn daemon() -> Result<IetsStore> {
        let quiet: OnEvent = Arc::new(|_| {});
        let store = Store::open("daemon", Default::default(), quiet).map_err(|e| miette!("{e}"))?;
        Ok(IetsStore {
            store: Arc::new(store),
        })
    }

    /// Runs `f` on the blocking pool: every daemon call waits on a socket.
    async fn call<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Store) -> std::result::Result<T, String> + Send + 'static,
    ) -> Result<T> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(&store))
            .await
            .into_diagnostic()?
            .map_err(|e| miette!("{e}"))
    }

    fn closure(store: &Store, roots: &[String]) -> std::result::Result<Vec<String>, String> {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut todo: VecDeque<String> = roots.iter().cloned().collect();
        while let Some(p) = todo.pop_front() {
            if !seen.insert(p.clone()) {
                continue;
            }
            if let Some(info) = store.path_info(&p)? {
                todo.extend(info.references.into_iter().filter(|r| !seen.contains(r)));
            }
        }
        Ok(seen.into_iter().collect())
    }
}

#[async_trait(?Send)]
impl StoreTrait for IetsStore {
    fn uri(&self) -> Result<String> {
        Ok(self.store.uri())
    }

    async fn add_gc_root(&self, gc_root: &Path, store_path: &CoreStorePath) -> Result<()> {
        let root = gc_root.to_path_buf();
        let path = store_path.as_str().to_string();
        self.call(move |s| s.add_perm_root(&path, &root))
            .await
            .wrap_err_with(|| format!("Failed to add GC root {}", gc_root.display()))
    }

    async fn realise(&self, drv: &CoreStorePath) -> Result<Vec<CoreStorePath>> {
        let drv = drv.as_str().to_string();
        let outs = self.call(move |s| s.realise(&drv)).await?;
        Ok(outs.into_values().map(|p| CoreStorePath::from(PathBuf::from(p))).collect())
    }

    async fn is_trusted_user(&self) -> Result<bool> {
        Ok(self.call(|s| s.trusted()).await?.unwrap_or(false))
    }

    async fn query_path_info(&self, p: &CoreStorePath) -> Result<Option<CorePathInfo>> {
        let path = p.as_str().to_string();
        let info = self.call(move |s| s.path_info(&path)).await?;
        Ok(info.map(|i| CorePathInfo {
            path: p.clone(),
            nar_size: i.nar_size,
            references: i
                .references
                .into_iter()
                .map(|r| CoreStorePath::from(PathBuf::from(r)))
                .collect(),
        }))
    }

    async fn collect_garbage(&self, opts: GcOptions) -> Result<GcStats> {
        let max_freed = opts.max_freed;
        let Some(paths) = opts.paths else {
            let activity = activity!(INFO, operation, "Deleting unused store paths");
            let (deleted, bytes) = self
                .call(move |s| s.collect_garbage(GcAction::DeleteDead, &[], false, max_freed))
                .in_activity(&activity)
                .await?;
            return Ok(GcStats {
                paths_deleted: deleted.len() as u64,
                bytes_freed: bytes,
            });
        };
        let store_dir = std::env::var("NIX_STORE_DIR").unwrap_or_else(|_| "/nix/store".into());
        let (store_paths, plain): (Vec<_>, Vec<_>) = paths
            .into_iter()
            .map(|p| p.as_path().to_path_buf())
            .partition(|p| p.starts_with(&store_dir) && p.components().count() == 4);
        let mut stats = GcStats::default();
        if !store_paths.is_empty() {
            let roots: Vec<String> = store_paths.iter().map(|p| p.display().to_string()).collect();
            let activity = activity!(INFO, operation, "Deleting unused store paths");
            let (deleted, bytes) = self
                .call(move |s| {
                    let closure = Self::closure(s, &roots)?;
                    // a 2.35 daemon restricts DeleteDead to the paths given;
                    // an older one refuses, and each path goes on its own
                    match s.collect_garbage(GcAction::DeleteDead, &closure, false, max_freed) {
                        Ok(r) => Ok(r),
                        Err(_) => {
                            let mut all = (Vec::new(), 0);
                            for p in closure {
                                if let Ok((d, b)) =
                                    s.collect_garbage(GcAction::DeleteSpecific, &[p], false, 0)
                                {
                                    all.0.extend(d);
                                    all.1 += b;
                                }
                            }
                            Ok(all)
                        }
                    }
                })
                .in_activity(&activity)
                .await?;
            stats.paths_deleted = deleted.len() as u64;
            stats.bytes_freed = bytes;
        }
        for p in plain {
            let _ = if p.is_dir() {
                tokio::fs::remove_dir_all(&p).await
            } else {
                tokio::fs::remove_file(&p).await
            };
        }
        Ok(stats)
    }

    async fn copy_paths(&self, _dest: &dyn StoreTrait, _paths: &[CoreStorePath]) -> Result<()> {
        Err(miette!("copying store paths is not supported with backend: iets"))
    }
}

pub struct IetsBackend {
    paths: DevenvPaths,
    bootstrap: PathBuf,
    nixpkgs_config: PathBuf,
    bootstrap_args: Arc<BootstrapArgs>,
    ports: Option<Arc<PortAllocator>>,
    /// Dropped to evaluate afresh: hot reload, an enter-shell refresh.
    evaluator: Mutex<Option<Arc<IetsEvaluator>>>,
    store: IetsStore,
    /// What every daemon connection is opened with: cachix adds substituters.
    options: Mutex<iets::api::store::Options>,
    observers: Mutex<Vec<Arc<dyn RealizedPathsObserver>>>,
}

impl IetsBackend {
    pub fn new(
        paths: DevenvPaths,
        bootstrap_args: Arc<BootstrapArgs>,
        ports: Option<Arc<PortAllocator>>,
        nixpkgs_config: &devenv_core::config::NixpkgsConfig,
    ) -> Result<IetsBackend> {
        let bootstrap = crate::backend::extract_bootstrap_files(&paths.dotfile)?;
        let nixpkgs_config = crate::backend::write_nixpkgs_config(nixpkgs_config, &paths.dotfile)?;
        Ok(IetsBackend {
            paths,
            bootstrap,
            nixpkgs_config,
            bootstrap_args,
            ports,
            evaluator: Mutex::new(None),
            store: IetsStore::daemon()?,
            options: Mutex::new(Default::default()),
            observers: Mutex::new(Vec::new()),
        })
    }

    fn evaluator(&self) -> Result<Arc<IetsEvaluator>> {
        let mut slot = self.evaluator.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(e) = &*slot {
            return Ok(e.clone());
        }
        let e = Arc::new(IetsEvaluator::start(
            &self.bootstrap.join("default.nix"),
            self.bootstrap_args.as_str(),
            &self.paths.root,
            self.ports.clone(),
            &self.nixpkgs_config,
        )?);
        *slot = Some(e.clone());
        Ok(e)
    }

    fn options(&self) -> iets::api::store::Options {
        self.options.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    fn notify_realized(&self, paths: &[PathBuf]) {
        let observers = self.observers.lock().unwrap_or_else(|p| p.into_inner()).clone();
        for o in &observers {
            o.on_realized(paths);
        }
    }

    /// The next evaluation starts from scratch; iets's cache still answers
    /// for whatever did not change.
    pub fn invalidate(&self) {
        self.evaluator
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
    }

    /// `.<attr>` as JSON.
    pub async fn eval_attr(&self, attr_path: &str, activity: &Activity) -> Result<String> {
        let iets = self.evaluator()?;
        let attr = attr_path.trim_start_matches(".#").to_owned();
        tokio::task::spawn_blocking(move || iets.eval_json(&attr))
            .in_activity(activity)
            .await
            .into_diagnostic()?
    }

    /// The shell's environment, its `-env` output rooted at `gc_root`.
    pub async fn dev_env(&self, json: bool, gc_root: &Path) -> Result<DevEnvOutput> {
        let activity = activity!(INFO, evaluate, "Evaluating shell");
        let iets = self.evaluator()?;
        let options = self.options();
        let shell = tokio::task::spawn_blocking(move || iets.shell(options))
            .in_activity(&activity)
            .await
            .into_diagnostic()??;
        self.store
            .add_gc_root(gc_root, &CoreStorePath::from(PathBuf::from(&shell.out_path)))
            .await?;
        self.notify_realized(&[PathBuf::from(&shell.out_path)]);
        let env_json = std::fs::read_to_string(&shell.out_path)
            .into_diagnostic()
            .wrap_err("Failed to read the shell environment")?;
        let bash_env = if json {
            env_json
        } else {
            RustBuildEnvironment::from_json(&env_json)
                .into_diagnostic()
                .wrap_err("Failed to parse the shell environment")?
                .to_activation_script()
        };
        Ok(DevEnvOutput {
            bash_env: bash_env.into_bytes(),
            inputs: shell.inputs,
        })
    }

    /// Settings the daemon takes from the client for the builds that follow.
    fn set_overrides(&self, pairs: &[(&str, String)]) {
        let mut options = self.options();
        for (name, value) in pairs {
            options.overrides.retain(|(k, _)| k != name);
            options.overrides.push((name.to_string(), value.clone()));
        }
        self.store.store.set_options(options.clone());
        *self.options.lock().unwrap_or_else(|p| p.into_inner()) = options;
    }

    /// `--use-machines-as-builders`: the daemon builds on `builders`.
    pub fn apply_remote_builders(&self, builders: &str) {
        self.set_overrides(&[
            ("builders", builders.to_string()),
            ("builders-use-substitutes", "true".to_string()),
        ]);
    }

    pub async fn search(
        &self,
        name: &str,
        max_results: Option<usize>,
    ) -> Result<devenv_core::evaluator::SearchResults> {
        let activity = activity!(INFO, evaluate, "Searching packages");
        let iets = self.evaluator()?;
        let name = name.to_owned();
        tokio::task::spawn_blocking(move || iets.search(&name, max_results))
            .in_activity(&activity)
            .await
            .into_diagnostic()?
    }

    /// `devenv repl`: lines from the terminal, evaluated with `devenv`
    /// and `pkgs` in scope. `:p EXPR` prints all of a value, `:q` quits.
    pub async fn repl(&self) -> Result<()> {
        use std::io::{BufRead, Write};
        let iets = self.evaluator()?;
        tokio::task::spawn_blocking(move || {
            eprintln!("devenv repl (iets): devenv and pkgs are in scope; :p EXPR prints deeply, :q quits");
            let stdin = std::io::stdin();
            let mut line = String::new();
            loop {
                eprint!("devenv-repl> ");
                let _ = std::io::stderr().flush();
                line.clear();
                if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
                    return;
                }
                let input = line.trim();
                let (expr, strict) = match input {
                    "" => continue,
                    ":q" | ":quit" => return,
                    _ => match input.strip_prefix(":p ") {
                        Some(e) => (e, true),
                        None => (input, false),
                    },
                };
                match iets.repl(expr, strict) {
                    Ok(out) => println!("{out}"),
                    Err(e) => eprintln!("{e:?}"),
                }
            }
        })
        .await
        .into_diagnostic()
    }

    /// `devenv update [name]`.
    pub async fn update(
        &self,
        input_name: &Option<String>,
        inputs: &BTreeMap<String, devenv_core::config::Input>,
        override_inputs: &[String],
    ) -> Result<()> {
        let update = match input_name {
            Some(name) => LockUpdate::Only(name),
            None => LockUpdate::All,
        };
        lock_inputs(
            &self.paths.root,
            &self.paths.lock_file,
            inputs,
            update,
            override_inputs,
        )?;
        self.invalidate();
        Ok(())
    }

    async fn build_attr(&self, attr: &str, opts: &BuildOptions) -> Result<CoreStorePath> {
        let iets = self.evaluator()?;
        let out_attr = format!("{attr}.outPath");
        let it = iets.clone();
        let json = tokio::task::spawn_blocking(move || it.eval_json(&out_attr))
            .await
            .into_diagnostic()??;
        let mut path: String = serde_json::from_str(&json).into_diagnostic()?;
        if !Path::new(&path).exists() {
            let a = attr.to_owned();
            let options = self.options();
            let built = tokio::task::spawn_blocking(move || iets.build(&a, options))
                .await
                .into_diagnostic()??;
            path = built
                .into_iter()
                .next()
                .ok_or_else(|| miette!("{attr} built nothing"))?;
            self.notify_realized(&[PathBuf::from(&path)]);
        }
        let out = CoreStorePath::from(PathBuf::from(&path));
        if let Some(base) = &opts.gc_root {
            let root = base.with_file_name(format!(
                "{}-{}",
                base.file_name().unwrap_or_default().to_string_lossy(),
                attr.replace('.', "-")
            ));
            self.store.add_gc_root(&root, &out).await?;
        }
        Ok(out)
    }
}

#[async_trait(?Send)]
impl Evaluator for IetsBackend {
    fn name(&self) -> &str {
        "iets"
    }

    fn store(&self) -> &dyn StoreTrait {
        &self.store
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn eval(&self, attrs: &[&str]) -> Result<String> {
        let mut results = Vec::new();
        for attr in attrs {
            let activity = activity!(INFO, evaluate, "Evaluating Nix");
            results.push(self.eval_attr(attr, &activity).await?);
        }
        Ok(if results.len() == 1 {
            results.remove(0)
        } else {
            format!("[{}]", results.join(","))
        })
    }

    async fn build(&self, attrs: &[&str], opts: BuildOptions) -> Result<Vec<CoreStorePath>> {
        let mut out = Vec::new();
        for attr in attrs {
            let activity = activity!(INFO, build, format!("Building {attr}"));
            out.push(self.build_attr(attr, &opts).in_activity(&activity).await?);
        }
        Ok(out)
    }
}

#[async_trait(?Send)]
impl CachixHost for IetsBackend {
    async fn eval_attr(&self, attr_path: &str, activity: &Activity) -> Result<String> {
        IetsBackend::eval_attr(self, attr_path, activity).await
    }

    /// The daemon takes substituters, keys and the netrc from the client
    /// as setting overrides, as libnix's client sends them.
    fn apply_store_settings(&self, settings: &StoreSettings) {
        let mut pairs = Vec::new();
        if !settings.extra_substituters.is_empty() {
            pairs.push(("extra-substituters", settings.extra_substituters.join(" ")));
        }
        if !settings.extra_trusted_public_keys.is_empty() {
            pairs.push((
                "extra-trusted-public-keys",
                settings.extra_trusted_public_keys.join(" "),
            ));
        }
        if let Some(netrc) = &settings.netrc_path {
            pairs.push(("netrc-file", netrc.display().to_string()));
        }
        self.set_overrides(&pairs);
    }

    fn add_realized_observer(&self, observer: Arc<dyn RealizedPathsObserver>) {
        self.observers
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(observer);
    }
}
