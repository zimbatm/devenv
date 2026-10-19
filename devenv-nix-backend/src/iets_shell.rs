//! `backend: iets`: the shell, its `-env` build and attribute evaluation run
//! in iets, linked as a library. Locking, the store, cachix and the REPL stay
//! on libnix.
//!
//! One resident iets evaluator serves the whole devenv run, so the module
//! tree is evaluated at most once. `loadDotenv` and `allocatePort` are iets
//! host builtins, handed to the bootstrap as its `primops`.

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use devenv_core::PortAllocator;
use devenv_core::eval_op::{EvalInputState, EvalOp, OpObserver};
use iets::api::shell_env::{GcRoot, Options, Transport};
use iets::api::host::HostBuiltin;
use iets::api::resident::Resident;
use iets::api::value::{Eval, Value};
use iets::api::{EvalArgs, Input};
use iets::eval::EvalConfig;
use miette::{IntoDiagnostic, Result, WrapErr, miette};

pub struct IetsShell {
    /// The `-env` output: the JSON `get-env.sh` wrote, and a GC root for the
    /// whole shell.
    pub out_path: String,
    /// The files the evaluation read, from iets's tracing cache.
    pub inputs: Vec<PathBuf>,
}

/// iets reports to its session log; this collects it for the next error.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn take(&self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut *self.0.lock().unwrap())).into_owned()
    }
}

pub struct IetsEvaluator {
    resident: Resident,
    log: Captured,
}

impl IetsEvaluator {
    /// The evaluator for `(import bootstrap (args // { primops = …; }))`.
    pub fn start(
        bootstrap: &Path,
        args_nix: &str,
        root: &Path,
        ports: Option<Arc<PortAllocator>>,
        nixpkgs_config: &Path,
    ) -> Result<IetsEvaluator> {
        let bootstrap = ser_nix::to_string(&ser_nix::NixPathBuf::from(bootstrap.to_path_buf()))
            .into_diagnostic()
            .wrap_err("Failed to serialize the bootstrap path")?;
        let mut primops = vec!["loadDotenv = builtins.devenvLoadDotenv;"];
        let mut cfg = EvalConfig::new().pure(false).host_builtin(load_dotenv(root));
        if let Some(ports) = ports {
            primops.push("allocatePort = builtins.devenvAllocatePort;");
            cfg = cfg.host_builtin(allocate_port(ports));
        }
        let expr = format!(
            "import ({bootstrap}) (({args_nix}) // {{ primops = {{ {} }}; }})",
            primops.join(" ")
        );
        let log = Captured::default();
        // as libnix's EvalState gets it: an override of the process environment
        let env = iets::env::Env::from_pairs(std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v))))
            .with([("NIXPKGS_CONFIG", nixpkgs_config.as_os_str().to_owned())]);
        let session = iets::session::Session::with_env(env);
        session.log.write_to(Box::new(log.clone()));
        let _ = session.load_settings(&[]);
        let root = EvalArgs {
            input: Some(Input::Expr(expr)),
            ..EvalArgs::new()
        };
        let cfg = root
            .config(cfg.session(session.clone()))
            .map_err(|e| miette!("{}", e.msg))?;
        Ok(IetsEvaluator {
            resident: Resident::start(session, cfg, root),
            log,
        })
    }

    fn report(&self, why: String) -> miette::Report {
        let log = self.log.take();
        miette!("{}", if log.trim().is_empty() { why } else { log })
    }

    /// `.shell`, with its `-env` derivation built through the local daemon.
    /// Blocks; call it off the runtime.
    pub fn shell(&self, options: Options) -> Result<IetsShell> {
        let transport = Transport::parse("daemon").ok_or_else(|| miette!("no local Nix daemon"))?;
        let env = self
            .resident
            .shell_env("shell", transport, options, GcRoot::None)
            .map_err(|why| self.report(why))?;
        let inputs = env
            .entry
            .as_deref()
            .and_then(|entry| iets::api::watch::watch_paths(entry).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|w| PathBuf::from(w.path))
            .filter(|p| !p.starts_with("/nix/store"))
            .collect();
        Ok(IetsShell {
            out_path: env.out_path,
            inputs,
        })
    }

    /// `.<attr>` as strict JSON, as `nix eval --json` prints it. Blocks.
    pub fn eval_json(&self, attr: &str) -> Result<String> {
        let bytes = self
            .resident
            .eval_json(attr)
            .map_err(|why| self.report(why))?;
        String::from_utf8(bytes).into_diagnostic()
    }
}

fn force<'e>(e: &mut Eval<'e>, v: Value<'e>) -> std::result::Result<Value<'e>, String> {
    e.force(v).map_err(|err| err.msg.clone())
}

fn string<'e>(e: &mut Eval<'e>, v: Value<'e>, what: &str) -> std::result::Result<String, String> {
    let v = force(e, v)?;
    v.as_bytes()
        .map(|b| String::from_utf8_lossy(b).into_owned())
        .ok_or_else(|| format!("{what} must be a string, got {}", v.kind().type_of()))
}

/// The dotenv loader keeps its own record of what it read; iets needs the
/// same reads in its trace, so they are replayed through `Eval`.
#[derive(Default)]
struct Reads(Mutex<Vec<EvalInputState>>);

impl OpObserver for Reads {
    fn record(&self, _op: EvalOp) {}

    fn record_input_state(&self, input: EvalInputState) {
        self.0.lock().unwrap().push(input);
    }
}

fn load_dotenv(root: &Path) -> HostBuiltin {
    let root = root.to_path_buf();
    HostBuiltin::new("devenvLoadDotenv", 2, move |e, args| {
        let list = force(e, args[0])?;
        let n = list.list_len().ok_or("filenames must be a list")?;
        let mut paths = Vec::with_capacity(n);
        for i in 0..n {
            let item = list.list_get(i).ok_or("filenames is too short")?;
            let name = string(e, item, "a filename")?;
            let p = Path::new(&name);
            paths.push(if p.is_absolute() { p.to_path_buf() } else { root.join(p) });
        }
        let substitution = force(e, args[1])?
            .as_bool()
            .ok_or("substitution must be a Boolean")?;
        let reads = Arc::new(Reads::default());
        let variables: BTreeMap<String, String> =
            devenv_core::dotenv::load_dotenv_tracked(&paths, substitution, reads.as_ref())
                .map_err(|err| format!("{err:?}"))?;
        for input in reads.0.lock().unwrap().drain(..) {
            match input {
                EvalInputState::File { path, .. } => {
                    let _ = e.read_file(path.as_os_str().as_encoded_bytes());
                }
                EvalInputState::Env { name, .. } => {
                    e.getenv(&name);
                }
            }
        }
        let values: Vec<(String, Value<'_>)> = variables
            .into_iter()
            .map(|(k, v)| {
                let s = e.string(v.as_bytes());
                (k, s)
            })
            .collect();
        let entries: Vec<(&[u8], Value<'_>)> =
            values.iter().map(|(k, v)| (k.as_bytes(), *v)).collect();
        e.attrs(&entries).map_err(|err| err.msg.clone())
    })
}

fn allocate_port(ports: Arc<PortAllocator>) -> HostBuiltin {
    let key = format!(
        "enabled={}:strict={}:seeds={}",
        ports.is_enabled(),
        ports.is_strict(),
        ports.seeded_ports_cache_key()
    );
    let enabled = ports.is_enabled();
    let b = HostBuiltin::new("devenvAllocatePort", 3, move |e, args| {
        let process = string(e, args[0], "processName")?;
        let port_name = string(e, args[1], "portName")?;
        let base = force(e, args[2])?
            .as_int()
            .ok_or("basePort must be an integer")?;
        let base = u16::try_from(base)
            .map_err(|_| format!("basePort must be between 0 and 65535, got {base}"))?;
        let port = ports
            .allocate(&process, &port_name, base)
            .map_err(|err| err.to_string())?;
        Ok(e.int(i64::from(port)))
    })
    .key(key.as_bytes());
    // a free port is a fact about the machine now, not about the inputs
    if enabled { b.impure() } else { b }
}

/// Which inputs `lock_inputs` locks again although the lock has them.
pub enum LockUpdate<'a> {
    Nothing,
    All,
    Only(&'a str),
}

fn lock_input(i: &devenv_core::config::Input) -> iets::api::lock::Input {
    iets::api::lock::Input {
        url: i.url.clone(),
        follows: i.follows.clone(),
        flake: i.flake,
        inputs: i.inputs.iter().map(|(k, v)| (k.clone(), lock_input(v))).collect(),
    }
}

/// `devenv.lock` for `inputs`, locked by iets: a lock that still matches is
/// only read, and anything else is fetched and written. `overrides` are
/// `[name, url, …]` pairs that replace a declared input. Gives the lock's
/// fingerprint, which goes into the bootstrap arguments.
pub fn lock_inputs(
    root: &Path,
    lock_file: &Path,
    inputs: &BTreeMap<String, devenv_core::config::Input>,
    update: LockUpdate<'_>,
    overrides: &[String],
) -> Result<String> {
    use iets::api::lock::{Options as LockOptions, Update, lock};
    let mut inputs: BTreeMap<String, iets::api::lock::Input> =
        inputs.iter().map(|(k, v)| (k.clone(), lock_input(v))).collect();
    for pair in overrides.chunks_exact(2) {
        let i = inputs
            .entry(pair[0].clone())
            .or_insert_with(|| iets::api::lock::Input {
                url: None,
                follows: None,
                flake: true,
                inputs: BTreeMap::new(),
            });
        i.url = Some(pair[1].clone());
        i.follows = None;
    }
    let log = Captured::default();
    let session = iets::session::Session::new();
    session.log.write_to(Box::new(log.clone()));
    let _ = session.load_settings(&[]);
    let _entered = session.enter();
    let old = std::fs::read(lock_file).ok();
    let opts = LockOptions {
        update: match update {
            LockUpdate::Nothing => Update::Nothing,
            LockUpdate::All => Update::All,
            LockUpdate::Only(name) => Update::Only(vec![name.to_string()]),
        },
        // devenv reads a local path input where it is
        unlocked_paths: true,
        base: root.to_path_buf(),
    };
    let cfg = EvalConfig::new().pure(false).session(session.clone());
    let locked = lock(cfg, &inputs, old.as_deref(), &opts).map_err(|why| {
        let said = log.take();
        miette!("Failed to lock inputs: {why}{}", if said.trim().is_empty() { String::new() } else { format!("\n{said}") })
    })?;
    if locked.changed {
        let _guard = crate::umask_guard::UmaskGuard::restrictive();
        let tmp = lock_file.with_extension("lock.tmp");
        std::fs::write(&tmp, &locked.bytes)
            .and_then(|()| std::fs::rename(&tmp, lock_file))
            .into_diagnostic()
            .wrap_err_with(|| format!("Failed to write {}", lock_file.display()))?;
    }
    Ok(devenv_cache_core::compute_string_hash(&String::from_utf8_lossy(&locked.bytes)))
}

impl IetsEvaluator {
    /// Builds `.<attr>` through the local daemon; its output paths. Blocks.
    pub fn build(&self, attr: &str, options: Options) -> Result<Vec<String>> {
        let transport = Transport::parse("daemon").ok_or_else(|| miette!("no local Nix daemon"))?;
        self.resident
            .build(attr, transport, options)
            .map_err(|why| self.report(why))
    }
}

impl IetsEvaluator {
    /// The packages of `.pkgs` that `regex` matches. Blocks.
    pub fn search(
        &self,
        regex: &str,
        max: Option<usize>,
    ) -> Result<devenv_core::evaluator::SearchResults> {
        let found = self
            .resident
            .search("pkgs", vec![regex.to_string()], max)
            .map_err(|why| self.report(why))?;
        Ok(found
            .into_iter()
            .map(|p| {
                (
                    p.attr,
                    devenv_core::evaluator::PackageSearchResult {
                        pname: p.pname,
                        version: p.version,
                        description: p.description,
                    },
                )
            })
            .collect())
    }
}

impl IetsEvaluator {
    /// One REPL line, with `devenv` and `pkgs` in scope as libnix's REPL
    /// has them. Blocks.
    pub fn repl(&self, line: &str, strict: bool) -> Result<String> {
        let scope = vec![
            ("devenv".to_string(), String::new()),
            ("pkgs".to_string(), "pkgs".to_string()),
        ];
        self.resident
            .repl(line, scope, strict)
            .map_err(|why| self.report(why))
    }
}
