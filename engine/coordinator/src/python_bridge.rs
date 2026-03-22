/// PyO3 bridge — calls Python scenario callbacks from the Rust coordinator.
///
/// Lifecycle:
///   1. `PythonBridge::new` loads the scenario file, instantiates N VUsers.
///   2. `call_on_start(i)` runs `vuser.on_start(LoadClient)` for VUser i.
///   3. `run_task(i, "task_name")` runs the task with a `RustClient`.
///      Inside `RustClient.get/post/...`, the GIL is released via
///      `Python::detach` while reqwest executes the HTTP request.
///      Each call is recorded; results are returned as Vec<CallResult>.
///   4. `call_on_stop(i)` runs `vuser.on_stop(LoadClient)` for VUser i.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{anyhow, Result};
use pyo3::prelude::*;
use pyo3::types::PyDict;

// ── RustResponse ──────────────────────────────────────────────────────────────

/// HTTP response returned to Python task methods by RustClient.
/// Mirrors the LoadClient / CheckResponse interface so existing task code
/// and check_* methods work without modification.
#[pyclass]
pub struct RustResponse {
    #[pyo3(get)]
    pub status_code: u16,
    #[pyo3(get)]
    pub ok: bool,
    #[pyo3(get)]
    pub text: String,
    headers_map: HashMap<String, String>,
}

#[pymethods]
impl RustResponse {
    fn json(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let value: serde_json::Value = serde_json::from_str(&self.text).map_err(|e| {
            pyo3::exceptions::PyValueError::new_err(format!("JSON decode error: {}", e))
        })?;
        json_to_py(py, &value)
    }

    fn raise_for_status(&self) -> PyResult<()> {
        if self.status_code >= 400 {
            Err(pyo3::exceptions::PyValueError::new_err(format!(
                "HTTP error {}",
                self.status_code
            )))
        } else {
            Ok(())
        }
    }

    #[getter]
    fn headers(&self, py: Python<'_>) -> Py<PyAny> {
        let dict = PyDict::new(py);
        for (k, v) in &self.headers_map {
            let _ = dict.set_item(k, v);
        }
        dict.into_any().unbind()
    }
}

// ── RustClient ────────────────────────────────────────────────────────────────

/// Python-callable HTTP client backed by reqwest.
///
/// Passed to task methods instead of httpx LoadClient. Each HTTP method
/// releases the GIL via `Python::detach` while the request is in flight,
/// so the Python scheduler can run other work and the tokio runtime can
/// drive other async tasks concurrently.
///
/// After a task finishes, `PythonBridge::run_task` reads `calls` and
/// `last_response` directly from this struct to build metrics.
#[pyclass]
pub struct RustClient {
    base_url: String,
    http_client: reqwest::Client,
    rt_handle: tokio::runtime::Handle,
    // Populated as HTTP calls are made — read by run_task after the task returns.
    pub calls: Vec<(u64, u16)>, // (elapsed_ms, status_code)
    last_response: Option<Py<RustResponse>>,
}

impl RustClient {
    fn new(
        base_url: String,
        http_client: reqwest::Client,
        rt_handle: tokio::runtime::Handle,
    ) -> Self {
        RustClient {
            base_url,
            http_client,
            rt_handle,
            calls: Vec::new(),
            last_response: None,
        }
    }

    fn do_request(
        &mut self,
        py: Python<'_>,
        method: &str,
        path: &str,
        req_headers: HashMap<String, String>,
        body: Option<String>,
        is_form: bool,
    ) -> PyResult<Py<RustResponse>> {
        let url = {
            let base = self.base_url.trim_end_matches('/');
            let p = if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{}", path)
            };
            format!("{}{}", base, p)
        };

        let client = self.http_client.clone();
        let handle = self.rt_handle.clone();
        let method_upper = method.to_uppercase();

        let t0 = Instant::now();

        // Python 3.13t has no GIL — call block_on directly from this
        // spawn_blocking thread. On GIL-enabled Python builds, wrap this
        // with py.allow_threads(|| ...) to release the GIL during I/O.
        let http_result = handle.block_on(async move {
                let mut req = match method_upper.as_str() {
                    "GET" => client.get(&url),
                    "POST" => client.post(&url),
                    "PUT" => client.put(&url),
                    "PATCH" => client.patch(&url),
                    "DELETE" => client.delete(&url),
                    other => {
                        return Err(format!("Unknown HTTP method: {}", other));
                    }
                };

                for (k, v) in &req_headers {
                    req = req.header(k.as_str(), v.as_str());
                }

                if let Some(b) = body {
                    if is_form {
                        req = req
                            .header("Content-Type", "application/x-www-form-urlencoded")
                            .body(b);
                    } else {
                        req = req
                            .header("Content-Type", "application/json")
                            .body(b);
                    }
                }

                match req.send().await {
                    Ok(resp) => {
                        let status = resp.status().as_u16();
                        let resp_headers: HashMap<String, String> = resp
                            .headers()
                            .iter()
                            .filter_map(|(k, v)| {
                                Some((k.as_str().to_string(), v.to_str().ok()?.to_string()))
                            })
                            .collect();
                        let body_text = resp.text().await.unwrap_or_default();
                        Ok((status, resp_headers, body_text))
                    }
                    Err(e) => Err(e.to_string()),
                }
        });

        let elapsed_ms = t0.elapsed().as_millis() as u64;

        match http_result {
            Ok((status, resp_headers, body_text)) => {
                self.calls.push((elapsed_ms, status));
                let response = Py::new(
                    py,
                    RustResponse {
                        status_code: status,
                        ok: status < 400,
                        text: body_text,
                        headers_map: resp_headers,
                    },
                )?;
                self.last_response = Some(response.clone_ref(py));
                Ok(response)
            }
            Err(e) => {
                self.calls.push((elapsed_ms, 0));
                Err(pyo3::exceptions::PyRuntimeError::new_err(e))
            }
        }
    }
}

#[pymethods]
impl RustClient {
    #[pyo3(signature = (path, **kwargs))]
    fn get(
        &mut self,
        py: Python<'_>,
        path: String,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<RustResponse>> {
        let (headers, body, is_form) = extract_kwargs(py, kwargs)?;
        self.do_request(py, "GET", &path, headers, body, is_form)
    }

    #[pyo3(signature = (path, **kwargs))]
    fn post(
        &mut self,
        py: Python<'_>,
        path: String,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<RustResponse>> {
        let (headers, body, is_form) = extract_kwargs(py, kwargs)?;
        self.do_request(py, "POST", &path, headers, body, is_form)
    }

    #[pyo3(signature = (path, **kwargs))]
    fn put(
        &mut self,
        py: Python<'_>,
        path: String,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<RustResponse>> {
        let (headers, body, is_form) = extract_kwargs(py, kwargs)?;
        self.do_request(py, "PUT", &path, headers, body, is_form)
    }

    #[pyo3(signature = (path, **kwargs))]
    fn patch(
        &mut self,
        py: Python<'_>,
        path: String,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<RustResponse>> {
        let (headers, body, is_form) = extract_kwargs(py, kwargs)?;
        self.do_request(py, "PATCH", &path, headers, body, is_form)
    }

    #[pyo3(signature = (path, **kwargs))]
    fn delete(
        &mut self,
        py: Python<'_>,
        path: String,
        kwargs: Option<&Bound<'_, PyDict>>,
    ) -> PyResult<Py<RustResponse>> {
        let (headers, body, is_form) = extract_kwargs(py, kwargs)?;
        self.do_request(py, "DELETE", &path, headers, body, is_form)
    }
}

// ── Metrics for one HTTP call ─────────────────────────────────────────────────

pub struct CallResult {
    pub elapsed_ms: u64,
    pub status_code: u16,
    pub success: bool,
    pub error: Option<String>,
}

// ── PythonBridge ──────────────────────────────────────────────────────────────

struct VUser {
    instance: Py<PyAny>,
}

pub struct PythonBridge {
    /// One mutex per VUser — tasks for different VUsers run in parallel;
    /// tasks for the same VUser are serialised (correct per-VUser semantics).
    ///
    /// With free-threaded Python (3.13t) there is no GIL, so multiple VUsers
    /// can execute Python callbacks truly in parallel. The per-VUser mutex
    /// remains necessary to protect per-VUser Python state (e.g. self.token)
    /// when the same VUser receives concurrent task dispatches.
    vusers: Vec<Mutex<VUser>>,
    base_url: String,
    has_on_start: bool,
    has_on_stop: bool,
    http_client: reqwest::Client,
    rt_handle: tokio::runtime::Handle,
}

impl PythonBridge {
    pub fn new(
        scenario_file: &str,
        scenario_class: &str,
        n_vusers: usize,
        base_url: &str,
        http_client: reqwest::Client,
        rt_handle: tokio::runtime::Handle,
    ) -> Result<Self> {
        Python::attach(|py| {
            // Pre-import the loadpilot package before adding the scenario directory
            // to sys.path. This prevents a circular import when the scenario file
            // itself is named loadpilot.py (which would shadow the package if the
            // directory were prepended first).
            let _ = py.import("loadpilot");

            let sys = py.import("sys")?;
            let path = sys.getattr("path")?;
            let dir = std::path::Path::new(scenario_file)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or(".");
            path.call_method1("insert", (0, dir))?;

            let util = py.import("importlib.util")?;
            let spec = util.call_method1(
                "spec_from_file_location",
                ("_loadpilot_scenario", scenario_file),
            )?;
            let module = util.call_method1("module_from_spec", (&spec,))?;
            spec.getattr("loader")?
                .call_method1("exec_module", (&module,))?;

            let cls = module.getattr(scenario_class)?;
            let cls_dict = cls.getattr("__dict__")?;
            let has_on_start = cls_dict.contains("on_start")?;
            let has_on_stop = cls_dict.contains("on_stop")?;

            let mut vusers = Vec::with_capacity(n_vusers);
            for _ in 0..n_vusers {
                let instance: Py<PyAny> = cls.call0()?.unbind();
                vusers.push(Mutex::new(VUser { instance }));
            }

            Ok(PythonBridge {
                vusers,
                base_url: base_url.to_string(),
                has_on_start,
                has_on_stop,
                http_client,
                rt_handle,
            })
        })
        .map_err(|e: PyErr| anyhow!("Python bridge init error: {}", e))
    }

    pub fn n_vusers(&self) -> usize {
        self.vusers.len()
    }

    pub fn call_on_start(&self, idx: usize) -> Result<()> {
        if !self.has_on_start {
            return Ok(());
        }
        // Acquire VUser lock before attaching to Python.
        // Recover from a poisoned mutex — a previous panic in a task should not
        // permanently block this VUser.
        let vuser = self.vusers[idx].lock().unwrap_or_else(|e| e.into_inner());
        Python::attach(|py| {
            let client = self.make_real_client(py)?;
            vuser.instance.call_method1(py, "on_start", (client,))?;
            Ok(())
        })
        .map_err(|e: PyErr| anyhow!("on_start error (VUser {}): {}", idx, e))
    }

    pub fn call_on_stop(&self, idx: usize) -> Result<()> {
        if !self.has_on_stop {
            return Ok(());
        }
        let vuser = self.vusers[idx].lock().unwrap_or_else(|e| e.into_inner());
        Python::attach(|py| {
            let client = self.make_real_client(py)?;
            vuser.instance.call_method1(py, "on_stop", (client,))?;
            Ok(())
        })
        .map_err(|e: PyErr| anyhow!("on_stop error (VUser {}): {}", idx, e))
    }

    /// Run the task with a `RustClient`. The client releases the GIL for each
    /// HTTP request so other Python callbacks and tokio tasks proceed in parallel.
    /// Returns one `CallResult` per HTTP call made by the task.
    pub fn run_task(&self, vuser_idx: usize, task_name: &str) -> Result<Vec<CallResult>> {
        // Acquire VUser lock before attaching to Python.
        let vuser = self.vusers[vuser_idx].lock().unwrap_or_else(|e| e.into_inner());

        Python::attach(|py| {
            let rust_client = Py::new(
                py,
                RustClient::new(
                    self.base_url.clone(),
                    self.http_client.clone(),
                    self.rt_handle.clone(),
                ),
            )?;

            let vuser = vuser.instance.bind(py);

            // Run the task — GIL is released inside each client.get/post/...
            if let Err(e) = vuser.call_method1(task_name, (rust_client.bind(py),)) {
                return Ok(vec![CallResult {
                    elapsed_ms: 0,
                    status_code: 500,
                    success: false,
                    error: Some(e.to_string()),
                }]);
            }

            // Read recorded calls before running the check (which may borrow vuser).
            let rc = rust_client.borrow(py);
            let calls_data = rc.calls.clone();
            let last_resp = rc.last_response.as_ref().map(|r| r.clone_ref(py));
            drop(rc);

            // Run optional check_{task}(self, response) with the last response.
            let check_name = format!("check_{}", task_name);
            let check_failed = if vuser.hasattr(check_name.as_str())? {
                if let Some(resp) = last_resp {
                    match vuser.call_method1(check_name.as_str(), (resp,)) {
                        Ok(_) => false,
                        Err(e) => {
                            eprintln!(
                                "[loadpilot] check_{} failed: {}",
                                task_name, e
                            );
                            true
                        }
                    }
                } else {
                    false
                }
            } else {
                false
            };

            let n = calls_data.len();
            if n == 0 {
                return Ok(vec![]);
            }

            let results = calls_data
                .into_iter()
                .enumerate()
                .map(|(i, (elapsed_ms, status))| {
                    let success =
                        if i == n - 1 && check_failed { false } else { status < 400 };
                    CallResult { elapsed_ms, status_code: status, success, error: None }
                })
                .collect();

            Ok(results)
        })
        .map_err(|e: PyErr| {
            anyhow!(
                "run_task error (VUser {}, task '{}'): {}",
                vuser_idx,
                task_name,
                e
            )
        })
    }

    // ── private helpers ──────────────────────────────────────────────────────

    /// httpx-backed LoadClient used for on_start / on_stop.
    /// These lifecycle methods are called once per VUser and need full kwargs
    /// support (data=, files=, etc.) that RustClient doesn't expose.
    fn make_real_client<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let loadpilot = py.import("loadpilot")?;
        let client_cls = loadpilot.getattr("LoadClient")?;
        client_cls.call1((&self.base_url,))
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Extract `headers=`, `json=`, and `data=` from Python **kwargs.
/// Returns (headers, body_string, is_form_encoded).
fn extract_kwargs(
    py: Python<'_>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<(HashMap<String, String>, Option<String>, bool)> {
    let mut headers = HashMap::new();
    let mut body: Option<String> = None;
    let mut is_form = false;

    let Some(kw) = kwargs else {
        return Ok((headers, body, is_form));
    };

    if let Some(h) = kw.get_item("headers")? {
        let h_dict = h.cast::<PyDict>()?;
        for (k, v) in h_dict {
            headers.insert(k.extract::<String>()?, v.extract::<String>()?);
        }
    }

    if let Some(json_val) = kw.get_item("json")? {
        let json_mod = py.import("json")?;
        let s: String = json_mod.call_method1("dumps", (json_val,))?.extract()?;
        body = Some(s);
    } else if let Some(data_val) = kw.get_item("data")? {
        // form-encoded: serialize dict as key=value&key=value
        if let Ok(d) = data_val.cast::<PyDict>() {
            let mut parts = Vec::new();
            for (k, v) in d {
                parts.push(format!(
                    "{}={}",
                    urlencoded_str(&k.extract::<String>()?),
                    urlencoded_str(&v.extract::<String>()?)
                ));
            }
            body = Some(parts.join("&"));
        } else {
            body = Some(data_val.extract::<String>()?);
        }
        is_form = true;
    }

    Ok((headers, body, is_form))
}

fn urlencoded_str(s: &str) -> String {
    s.chars()
        .flat_map(|c| match c {
            c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') => {
                vec![c]
            }
            c => format!("%{:02X}", c as u32).chars().collect::<Vec<_>>(),
        })
        .collect()
}

/// Recursively convert a serde_json::Value into a Python object.
fn json_to_py(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    match value {
        serde_json::Value::Null => Ok(py.None()),
        serde_json::Value::Bool(b) => {
            // bool::into_pyobject returns Borrowed (Python True/False singletons).
            // Deref to Bound explicitly so that .clone() calls Bound::clone (not Borrowed::clone),
            // giving an owned Bound that can be moved into into_any().
            let b_py = (*b).into_pyobject(py)?;
            Ok((*b_py).clone().into_any().unbind())
        }
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(i.into_pyobject(py)?.into_any().unbind())
            } else {
                Ok(n.as_f64().unwrap_or(0.0).into_pyobject(py)?.into_any().unbind())
            }
        }
        serde_json::Value::String(s) => Ok(s.clone().into_pyobject(py)?.into_any().unbind()),
        serde_json::Value::Array(arr) => {
            let list = pyo3::types::PyList::empty(py);
            for item in arr {
                list.append(json_to_py(py, item)?)?;
            }
            Ok(list.into_any().unbind())
        }
        serde_json::Value::Object(map) => {
            let dict = PyDict::new(py);
            for (k, v) in map {
                dict.set_item(k, json_to_py(py, v)?)?;
            }
            Ok(dict.into_any().unbind())
        }
    }
}
