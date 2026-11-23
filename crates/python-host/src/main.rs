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
//! `ctx` is implemented here: register_tool, register_hook and get_config behave as in the
//! Python agent; other register_* calls are accepted and reported as unsupported on stderr.
use pyo3::exceptions::{PyAttributeError, PyOSError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyCFunction, PyDict, PyTuple};
use serde_json::{Value, json};
use std::ffi::CString;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const HOST: &str = include_str!("host.py");
const VERSION: &str = env!("HERMES_PYTHON_VERSION");

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.first().map(String::as_str) {
        Some("--version") => version(),
        Some("serve") if args.len() >= 2 => serve(Path::new(&args[1]), args.get(2).map(PathBuf::from)),
        Some("install") if args.len() >= 3 => install(Path::new(&args[1]), &args[2..]),
        _ => { eprintln!("usage: hermes-python-host serve <plugin dir> [<packages dir>] | install <packages dir> <requirement>..."); 2 }
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

struct Tool { name: String, schema: Py<PyAny>, handler: Py<PyAny>, check: Option<Py<PyAny>>, requires_env: Vec<String>, description: String }

/// The `ctx` passed to a plugin's `register(ctx)`.
#[pyclass(unsendable)]
struct Context { plugin: String, tools: Vec<Tool>, hooks: Vec<(String, Py<PyAny>)> }

#[pymethods]
impl Context {
    #[getter]
    fn plugin_id(&self) -> String { self.plugin.clone() }

    #[pyo3(signature = (name, toolset, schema, handler, check_fn=None, requires_env=None, is_async=false, description=String::new(), emoji=String::new(), r#override=false))]
    #[allow(clippy::too_many_arguments, unused_variables)]
    fn register_tool(&mut self, py: Python<'_>, name: String, toolset: String, schema: Py<PyAny>, handler: Py<PyAny>, check_fn: Option<Py<PyAny>>,
                     requires_env: Option<Vec<Py<PyAny>>>, is_async: bool, description: String, emoji: String, r#override: bool) -> PyResult<()> {
        if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) {
            return Err(PyValueError::new_err(format!("Tool name {name:?} must be letters, digits, _ or -")));
        }
        let requires_env = requires_env.unwrap_or_default().iter().filter_map(|v| v.extract::<String>(py).ok()).collect();
        self.tools.retain(|t| t.name != name);
        self.tools.push(Tool { name, schema, handler, check: check_fn, requires_env, description });
        Ok(())
    }

    #[pyo3(signature = (hook_name, callback, hold_stream=false))]
    #[allow(unused_variables)]
    fn register_hook(&mut self, hook_name: String, callback: Py<PyAny>, hold_stream: bool) {
        self.hooks.push((hook_name, callback));
    }

    /// The plugin's config value, which the engine passes as PLUGIN_CONFIG_<KEY>.
    #[pyo3(signature = (key, default=None))]
    fn get_config(&self, py: Python<'_>, key: &str, default: Option<Py<PyAny>>) -> PyResult<Py<PyAny>> {
        let name = format!("PLUGIN_CONFIG_{}", key.to_uppercase().replace(|c: char| !c.is_ascii_alphanumeric(), "_"));
        Ok(match std::env::var(name) {
            Ok(value) => value.into_pyobject(py)?.into_any().unbind(),
            Err(_) => default.unwrap_or_else(|| py.None()),
        })
    }

    fn __getattr__(&self, py: Python<'_>, name: &str) -> PyResult<Py<PyAny>> {
        if !name.starts_with("register_") { return Err(PyAttributeError::new_err(format!("ctx.{name} is not available in the Hermes desktop app"))); }
        let message = format!("{}: ctx.{name} is not supported in the Hermes desktop app; skipped", self.plugin);
        let skip = PyCFunction::new_closure(py, None, None, move |_args: &Bound<'_, PyTuple>, _kwargs: Option<&Bound<'_, PyDict>>| { eprintln!("{message}"); })?;
        Ok(skip.into_any().unbind())
    }
}

fn to_py<'py>(py: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> { py.import("json")?.getattr("loads")?.call1((value.to_string(),)) }
fn dumps(helper: &Bound<'_, PyModule>, value: &Bound<'_, PyAny>) -> PyResult<String> { helper.getattr("dumps")?.call1((value,))?.extract() }
fn from_py(helper: &Bound<'_, PyModule>, value: &Bound<'_, PyAny>) -> PyResult<Value> {
    let text = dumps(helper, value)?;
    Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

fn serve(plugin_dir: &Path, packages: Option<PathBuf>) -> i32 {
    start();
    let Ok(plugin_dir) = plugin_dir.canonicalize() else { eprintln!("No plugin at {}", plugin_dir.display()); return 2 };
    let name = plugin_dir.file_name().map(|n| n.to_string_lossy().replace(|c: char| !c.is_ascii_alphanumeric(), "_")).unwrap_or_default();
    let loaded = Python::attach(|py| -> PyResult<(Py<PyModule>, Py<Context>)> {
        let helper = helper(py)?;
        if let Some(packages) = &packages { py.import("sys")?.getattr("path")?.call_method1("insert", (0, packages.to_string_lossy().into_owned()))?; }
        let module = helper.getattr("load")?.call1((plugin_dir.to_string_lossy().into_owned(), &name))?;
        let ctx = Py::new(py, Context { plugin: name.clone(), tools: vec![], hooks: vec![] })?;
        module.getattr("register")?.call1((ctx.clone_ref(py),))?;
        Ok((helper.unbind(), ctx))
    });
    let (helper, ctx) = match loaded {
        Ok(loaded) => loaded,
        Err(e) => { Python::attach(|py| { eprintln!("The plugin did not load:"); e.print(py) }); return 3 }
    };
    let stdout = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = request.get("id").cloned() else { continue };
        let params = request.get("params").cloned().unwrap_or(json!({}));
        let method = request["method"].as_str().unwrap_or_default();
        let reply = Python::attach(|py| handle(py, helper.bind(py), &ctx.bind(py).borrow(), method, &params));
        let message = match reply {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": error}}),
        };
        let mut out = stdout.lock();
        if writeln!(out, "{message}").and_then(|_| out.flush()).is_err() { break }
    }
    0
}

/// Call a plugin callable the way Hermes does (unaccepted keyword arguments dropped, coroutines run).
fn call<'py>(helper: &Bound<'py, PyModule>, f: &Bound<'py, PyAny>, args: Bound<'py, PyTuple>, kwargs: &Bound<'py, PyAny>) -> PyResult<Bound<'py, PyAny>> {
    helper.getattr("call")?.call1((f, args, kwargs))
}

fn available(py: Python<'_>, tool: &Tool) -> bool {
    tool.requires_env.iter().all(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
        && tool.check.as_ref().is_none_or(|check| check.bind(py).call0().and_then(|r| r.is_truthy()).unwrap_or(false))
}

fn handle(py: Python<'_>, helper: &Bound<'_, PyModule>, ctx: &Context, method: &str, params: &Value) -> Result<Value, String> {
    let failed = |e: PyErr| e.to_string();
    match method {
        "initialize" => {
            let mut hooks: Vec<&str> = ctx.hooks.iter().map(|(h, _)| h.as_str()).collect();
            hooks.sort_unstable();
            hooks.dedup();
            Ok(json!({"protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}, "experimental": {"hermes": {"hooks": hooks}}},
                "serverInfo": {"name": ctx.plugin, "version": env!("CARGO_PKG_VERSION")}}))
        }
        "tools/list" => {
            let mut tools = vec![];
            for tool in ctx.tools.iter().filter(|t| available(py, t)) {
                let schema = from_py(helper, tool.schema.bind(py)).map_err(failed)?;
                let description = schema["description"].as_str().filter(|d| !d.is_empty()).unwrap_or(&tool.description).to_owned();
                let parameters = schema.get("parameters").filter(|p| p.is_object()).cloned().unwrap_or(json!({"type": "object", "properties": {}}));
                tools.push(json!({"name": tool.name, "description": description, "inputSchema": parameters}));
            }
            Ok(json!({"tools": tools}))
        }
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let tool = ctx.tools.iter().find(|t| t.name == name && available(py, t)).ok_or_else(|| format!("Unknown tool: {name}"))?;
            let args = to_py(py, params.get("arguments").unwrap_or(&json!({}))).map_err(failed)?;
            let args = PyTuple::new(py, [args]).map_err(failed)?;
            let (text, error) = match call(helper, tool.handler.bind(py), args, PyDict::new(py).as_any()).and_then(|r| dumps(helper, &r)) {
                Ok(text) => { let error = serde_json::from_str::<Value>(&text).is_ok_and(|v| v.get("error").is_some()); (text, error) }
                Err(e) => (json!({"error": e.to_string()}).to_string(), true),
            };
            Ok(json!({"content": [{"type": "text", "text": text}], "isError": error}))
        }
        // A Hermes lifecycle hook: run every callback for `name`, return the non-None results.
        // A callback that raises is skipped, as in the Python agent.
        "hermes/hook" => {
            let hook = params["name"].as_str().unwrap_or_default();
            let kwargs = to_py(py, params.get("kwargs").unwrap_or(&json!({}))).map_err(failed)?;
            let mut results = vec![];
            for (_, callback) in ctx.hooks.iter().filter(|(h, _)| h == hook) {
                match call(helper, callback.bind(py), PyTuple::empty(py), &kwargs) {
                    Ok(result) if !result.is_none() => results.push(from_py(helper, &result).map_err(failed)?),
                    Ok(_) => {}
                    Err(e) => eprintln!("{}: {hook} hook failed: {e}", ctx.plugin),
                }
            }
            Ok(json!({"results": results}))
        }
        "ping" => Ok(json!({})),
        other => Err(format!("Unsupported method: {other}")),
    }
}
