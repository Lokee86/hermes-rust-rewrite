//! hermes-python-host: runs one Python Hermes plugin (`plugin.yaml` + `__init__.py` with
//! `register(ctx)`) in CPython embedded in this process, and serves the tools and hooks it
//! registered to the desktop engine over MCP stdio (newline-delimited JSON-RPC).
//!
//!   hermes-python-host serve <plugin dir> [<packages dir>]
//!   hermes-python-host install <packages dir> <requirement>...
//!   hermes-python-host --version
//!
//! The interpreter is the CPython runtime packaged with the app (Resources/python on macOS,
//! lib/hermes/python on Linux, python\ next to the executable on Windows), so plugins need no
//! separate Python install; an unpackaged build uses the interpreter it was linked against.
//! The process owns the Python registrations and calls the gateway for profile services.
//! Unsupported registrations are reported with the plugin metadata.
use pyo3::exceptions::{PyOSError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use serde_json::Value;
use std::ffi::CString;
use std::sync::Arc;
use std::path::{Path, PathBuf};

mod wire;

const HOST: &str = include_str!("host.py");
const COMPAT: &str = include_str!("compat.py");
const VERSION: &str = env!("HERMES_PYTHON_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--version") => version(),
        Some("serve") if args.len() >= 2 => serve(Path::new(&args[1]), args.get(2).map(PathBuf::from)),
        Some("run") if args.len() == 2 => run(Path::new(&args[1])),
        Some("install") if args.len() >= 3 => install(Path::new(&args[1]), &args[2..]),
        _ => { eprintln!("usage: hermes-python-host serve <plugin dir> [<packages dir>] | install <packages dir> <requirement>... | run <script>"); 2 }
    };
    std::process::exit(code);
}

/// `<version> <sys.prefix>`: which interpreter and runtime this host runs (packaging checks it).
fn version() -> i32 {
    start();
    Python::attach(|py| -> PyResult<()> {
        let sys = py.import("sys")?;
        let version = sys.getattr("version_info")?;
        println!("{}.{}.{} {}", version.get_item(0)?, version.get_item(1)?, version.get_item(2)?, sys.getattr("prefix")?);
        Ok(())
    }).map_or_else(|e| { eprintln!("{e}"); 1 }, |()| 0)
}

/// The packaged runtime next to this executable, if there is one.
fn bundled_home() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // Resolve symlinks (a Linux bin/ link) on Unix; Windows canonical paths are \\?\ paths,
    // which PYTHONHOME should not carry.
    let exe = if cfg!(windows) { exe } else { exe.canonicalize().ok()? };
    let (dir, up) = (exe.parent()?, exe.parent()?.parent()?);
    let stdlib = if cfg!(windows) { PathBuf::from("Lib/os.py") } else { PathBuf::from(format!("lib/python{VERSION}/os.py")) };
    [up.join("Resources/python"), up.join("lib/hermes/python"), dir.join("python")].into_iter().find(|home| home.join(&stdlib).is_file())
}

fn start() {
    // SAFETY: single-threaded; nothing else has started yet.
    unsafe {
        if let Some(home) = bundled_home().or_else(|| option_env!("HERMES_PYTHON_BUILD_HOME").map(PathBuf::from)) { std::env::set_var("PYTHONHOME", home); }
        // Isolated like `python -I`: no user site-packages, no PYTHONPATH from the caller.
        std::env::set_var("PYTHONNOUSERSITE", "1");
        // The runtime may sit in a signed app bundle; never write .pyc files into it.
        std::env::set_var("PYTHONDONTWRITEBYTECODE", "1");
        std::env::remove_var("PYTHONPATH");
    }
    Python::initialize();
}

fn helper(py: Python<'_>) -> PyResult<Bound<'_, PyModule>> {
    let code = CString::new(HOST).expect("host.py has no NUL");
    PyModule::from_code(py, &code, c"hermes_python_host.py", c"hermes_python_host")
}

fn install(target: &Path, requirements: &[String]) -> i32 {
    start();
    Python::attach(|py| -> PyResult<i32> {
        std::fs::create_dir_all(target).map_err(|e| PyOSError::new_err(e.to_string()))?;
        helper(py)?.getattr("install")?.call1((target.to_string_lossy().into_owned(), requirements.to_vec()))?.extract()
    }).unwrap_or_else(|e| { eprintln!("{e}"); 1 })
}

#[pyclass]
struct Services {
    wire: Arc<wire::Wire>,
}

#[pymethods]
impl Services {
    fn request(&self, py: Python<'_>, operation: &str, encoded: &str) -> PyResult<String> {
        let params = serde_json::from_str(encoded)
            .map_err(|error| PyValueError::new_err(error.to_string()))?;
        let result = py
            .detach(|| self.wire.service(operation, params))
            .map_err(PyRuntimeError::new_err)?;
        Ok(result.to_string())
    }
}

/// Run one script as `python <script>` would.
fn run(script: &Path) -> i32 {
    start();
    let code = c"
import os, runpy, sys, traceback
def run(path):
    sys.argv = [path]
    sys.path.insert(0, os.path.dirname(os.path.abspath(path)))
    try:
        runpy.run_path(path, run_name='__main__')
        return 0
    except SystemExit as stop:
        if stop.code is None or isinstance(stop.code, int):
            return stop.code or 0
        print(stop.code, file=sys.stderr)
        return 1
    except BaseException:
        traceback.print_exc()
        return 1
    finally:
        sys.stdout.flush()
";
    Python::attach(|py| -> PyResult<i32> {
        let module = PyModule::from_code(py, code, c"hermes_run.py", c"hermes_run")?;
        module.getattr("run")?.call1((script.to_string_lossy(),))?.extract()
    }).unwrap_or_else(|error| { eprintln!("{error}"); 1 })
}

fn dispatch(host: &Py<PyAny>, method: &str, request: &Value) -> Result<Value, String> {
    let params = request.get("params").map(Value::to_string).unwrap_or_else(|| "{}".into());
    Python::attach(|py| {
        let encoded: String = host.call_method1(py, "dispatch_json", (method, params))?.extract(py)?;
        serde_json::from_str::<Value>(&encoded).map_err(|error| PyValueError::new_err(error.to_string()))
    }).map_err(|error: PyErr| error.to_string())
}

fn serve(plugin_dir: &Path, packages: Option<PathBuf>) -> i32 {
    start();
    let Ok(plugin_dir) = plugin_dir.canonicalize() else {
        eprintln!("No plugin at {}", plugin_dir.display());
        return 2;
    };
    let name = plugin_dir.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
    let home = std::env::var("HERMES_HOME").ok().filter(|value| !value.is_empty());
    let wire = Arc::new(wire::Wire::default());
    let incoming = wire.read_requests();
    let host = Python::attach(|py| -> PyResult<Py<PyAny>> {
        let helper = helper(py)?;
        let compat = if home.is_some() {
            let code = CString::new(COMPAT).expect("compat.py has no NUL");
            Some(PyModule::from_code(py, &code, c"hermes_python_compat.py", c"hermes_python_compat")?)
        } else {
            None
        };
        let service = (std::env::var("HERMES_PLUGIN_SERVICES").as_deref() == Ok("1"))
            .then(|| Py::new(py, Services { wire: wire.clone() }))
            .transpose()?;
        let packages = packages.map(|path| path.to_string_lossy().into_owned());
        Ok(helper.getattr("start")?.call1((
            plugin_dir.to_string_lossy(), name, packages, home, compat, service, env!("CARGO_PKG_VERSION"),
        ))?.unbind())
    });
    let host = match host {
        Ok(host) => host,
        Err(error) => {
            Python::attach(|py| {
                eprintln!("The plugin did not load:");
                error.print(py);
            });
            return 3;
        }
    };
    let host = Arc::new(host);
    for request in incoming {
        let Some(id) = request.get("id") else { continue };
        let method = request["method"].as_str().unwrap_or_default();
        // Tools run beside each other, as in the Python agent, so a slow one in one chat does not
        // hold up the rest. Hooks, commands and memory calls keep their order.
        if method == "tools/call" {
            let (host, wire, request) = (host.clone(), wire.clone(), request.clone());
            std::thread::spawn(move || {
                let result = dispatch(&host, "tools/call", &request);
                let _ = wire.reply(&request["id"], result);
            });
            continue;
        }
        if wire.reply(id, dispatch(&host, method, &request)).is_err() || method == "hermes/unload" {
            break;
        }
    }
    Python::attach(|py| {
        if let Err(error) = host.call_method0(py, "close") {
            eprintln!("Plugin cleanup: {error}");
        }
    });
    0
}
