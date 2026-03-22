/// PyO3 bridge — calls Python scenario callbacks from the Rust coordinator.
///
/// Architecture: one dedicated OS thread per VUser.
///   • `Python::attach` is called ONCE per VUser thread and kept active for
///     the entire test duration — no per-task Python thread-state overhead.
///   • The coordinator sends task requests through an `mpsc` channel and
///     awaits a `oneshot` reply — the tokio executor is never blocked.
///   • HTTP I/O inside RustClient calls `handle.block_on(...)` directly on
///     the VUser OS thread (valid; the thread is not a tokio worker thread).
use std::collections::HashMap;
use std::time::Instant;

use anyhow::{anyhow, Result};
use pyo3::prelude::*;
use pyo3::types::PyDict;
use tokio::sync::{mpsc, oneshot};

// ── RustResponse ──────────────────────────────────────────────────────────────

/// HTTP response returned to Python task methods by RustClient.
#[pyclass]
pub struct RustResponse {
    #[pyo3(get)]
    pub status_code: u16,
    #[pyo3(get)]
    pub ok: bool,
    #[pyo3(get)]
    pub text: String,
    headers_map: HashMap<String, String>,
    /// Pre-built Python object from `json_to_py`, populated eagerly in
    /// `do_request`/`do_batch` right after `py.detach()` exits (GIL held, but
    /// as part of the existing post-HTTP window — no extra serialisation).
    /// Calling `response.json()` from `check_*` then becomes a `clone_ref`
    /// (~1 ns) instead of `serde_json::from_str + json_to_py` (hundreds of µs
    /// per call × N VUser threads competing for the GIL).
    json_cache: Option<Py<PyAny>>,
}

#[pymethods]
impl RustResponse {
    fn json(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        // Fast path: return the pre-built Python object (eager-cached in do_request).
        if let Some(ref cached) = self.json_cache {
            return Ok(cached.clone_ref(py));
        }
        // Fallback: parse on demand (e.g. RustResponse constructed in tests).
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

#[pyclass]
pub struct RustClient {
    base_url: String,
    http_client: reqwest::Client,
    rt_handle: tokio::runtime::Handle,
    pub calls: Vec<(u64, u16)>,
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

    fn build_url(&self, path: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let p = if path.starts_with('/') { path.to_string() } else { format!("/{}", path) };
        format!("{}{}", base, p)
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
        let url = self.build_url(path);
        let client = self.http_client.clone();
        let handle = self.rt_handle.clone();
        let method_upper = method.to_uppercase();

        let t0 = Instant::now();
        // Release the GIL while waiting for the HTTP response so other VUser
        // threads can run their Python code concurrently. All captured values
        // are Send Rust types — no Python objects cross the allow_threads boundary.
        let http_result = py.detach(|| handle.block_on(async move {
            let mut req = match method_upper.as_str() {
                "GET" => client.get(&url),
                "POST" => client.post(&url),
                "PUT" => client.put(&url),
                "PATCH" => client.patch(&url),
                "DELETE" => client.delete(&url),
                other => return Err(format!("Unknown HTTP method: {}", other)),
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
                    req = req.header("Content-Type", "application/json").body(b);
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
                    // Parse JSON while the GIL is still released — serde_json is
                    // pure Rust and needs no Python runtime. The resulting Value
                    // is converted to a Py<PyAny> after py.detach() exits below.
                    let json_parsed = parse_json_body(&body_text);
                    Ok((status, resp_headers, body_text, json_parsed))
                }
                Err(e) => Err(e.to_string()),
            }
        }));

        let elapsed_ms = t0.elapsed().as_millis() as u64;

        match http_result {
            Ok((status, resp_headers, body_text, json_parsed)) => {
                self.calls.push((elapsed_ms, status));
                // GIL is re-acquired here. Convert the pre-parsed JSON Value to a
                // Python object now, in the same post-HTTP GIL window used to create
                // RustResponse. This amortises json_to_py so that check_* calling
                // response.json() needs only a clone_ref (~1 ns) instead of
                // parsing + json_to_py under GIL contention.
                let json_cache = json_parsed.as_ref().and_then(|v| json_to_py(py, v).ok());
                let response = Py::new(
                    py,
                    RustResponse {
                        status_code: status,
                        ok: status < 400,
                        text: body_text,
                        headers_map: resp_headers,
                        json_cache,
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

    /// Execute N HTTP requests concurrently inside Rust, paying the PyO3 boundary
    /// cost once for the whole batch.
    ///
    /// Each element of `requests` is a dict with keys:
    ///   - `"method"` (str, default `"GET"`)
    ///   - `"path"`   (str, required)
    ///   - `"headers"` (dict[str,str], optional)
    ///   - `"json"`    (any, optional) — serialised as JSON body
    ///   - `"data"`    (dict|str, optional) — form-encoded body
    ///
    /// ```python
    /// @task()
    /// def fetch_batch(self, client):
    ///     auth = {"Authorization": f"Bearer {self.token}"}
    ///     client.batch([
    ///         {"method": "GET", "path": "/api/user",    "headers": auth},
    ///         {"method": "GET", "path": "/api/product", "headers": auth},
    ///         {"method": "GET", "path": "/api/cart",    "headers": auth},
    ///     ])
    /// ```
    ///
    /// All requests are dispatched to the tokio executor as independent tasks and
    /// run concurrently while the GIL is released. Results are collected in
    /// completion order, metrics recorded in dispatch order.
    fn do_batch(
        &mut self,
        py: Python<'_>,
        requests: &Bound<'_, pyo3::types::PyList>,
    ) -> PyResult<Vec<Py<RustResponse>>> {
        // Parse all request specs while holding the GIL — nothing async yet.
        struct ReqSpec {
            method: String,
            url: String,
            headers: HashMap<String, String>,
            body: Option<String>,
            is_form: bool,
        }

        let specs: Vec<ReqSpec> = requests
            .iter()
            .map(|item| {
                let d = item.cast::<PyDict>()?;
                let method = d
                    .get_item("method")?
                    .map(|v| v.extract::<String>())
                    .transpose()?
                    .unwrap_or_else(|| "GET".to_string())
                    .to_uppercase();
                let path: String = d
                    .get_item("path")?
                    .ok_or_else(|| {
                        pyo3::exceptions::PyValueError::new_err("batch item missing 'path'")
                    })?
                    .extract()?;
                let url = self.build_url(&path);

                // Reuse extract_kwargs logic inline for headers / json / data.
                let mut headers = HashMap::new();
                let mut body: Option<String> = None;
                let mut is_form = false;

                if let Some(h) = d.get_item("headers")? {
                    let h_dict = h.cast::<PyDict>()?;
                    for (k, v) in h_dict {
                        headers.insert(k.extract::<String>()?, v.extract::<String>()?);
                    }
                }
                if let Some(json_val) = d.get_item("json")? {
                    let json_mod = py.import("json")?;
                    let s: String = json_mod.call_method1("dumps", (json_val,))?.extract()?;
                    body = Some(s);
                } else if let Some(data_val) = d.get_item("data")? {
                    if let Ok(dd) = data_val.cast::<PyDict>() {
                        let mut parts = Vec::new();
                        for (k, v) in dd {
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

                Ok(ReqSpec { method, url, headers, body, is_form })
            })
            .collect::<PyResult<_>>()?;

        let client = self.http_client.clone();
        let handle = self.rt_handle.clone();

        type HttpOutcome = Result<(u64, u16, HashMap<String, String>, String, Option<serde_json::Value>), (u64, String)>;

        // Release the GIL for all concurrent HTTP. All captured values are Send
        // Rust types — no Python objects cross the py.detach boundary.
        let outcomes: Vec<HttpOutcome> = py.detach(|| {
            handle.block_on(async move {
                let mut set = tokio::task::JoinSet::new();
                for spec in specs {
                    let c = client.clone();
                    set.spawn(async move {
                        let t0 = Instant::now();
                        let mut req = match spec.method.as_str() {
                            "GET"    => c.get(&spec.url),
                            "POST"   => c.post(&spec.url),
                            "PUT"    => c.put(&spec.url),
                            "PATCH"  => c.patch(&spec.url),
                            "DELETE" => c.delete(&spec.url),
                            other    => {
                                let ms = t0.elapsed().as_millis() as u64;
                                return Err((ms, format!("Unknown method: {}", other)));
                            }
                        };
                        for (k, v) in &spec.headers {
                            req = req.header(k.as_str(), v.as_str());
                        }
                        if let Some(ref b) = spec.body {
                            req = if spec.is_form {
                                req.header("Content-Type", "application/x-www-form-urlencoded")
                                   .body(b.clone())
                            } else {
                                req.header("Content-Type", "application/json").body(b.clone())
                            };
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
                                let json_parsed = parse_json_body(&body_text);
                                let ms = t0.elapsed().as_millis() as u64;
                                Ok((ms, status, resp_headers, body_text, json_parsed))
                            }
                            Err(e) => {
                                let ms = t0.elapsed().as_millis() as u64;
                                Err((ms, e.to_string()))
                            }
                        }
                    });
                }
                let mut results = Vec::new();
                while let Some(r) = set.join_next().await {
                    results.push(r.unwrap()); // JoinError only on panic
                }
                results
            })
        });

        // GIL re-acquired: build Python response objects and record metrics.
        let mut responses = Vec::new();
        for outcome in outcomes {
            match outcome {
                Ok((elapsed_ms, status, resp_headers, body_text, json_parsed)) => {
                    self.calls.push((elapsed_ms, status));
                    let json_cache = json_parsed.as_ref().and_then(|v| json_to_py(py, v).ok());
                    let resp = Py::new(
                        py,
                        RustResponse {
                            status_code: status,
                            ok: status < 400,
                            text: body_text,
                            headers_map: resp_headers,
                            json_cache,
                        },
                    )?;
                    self.last_response = Some(resp.clone_ref(py));
                    responses.push(resp);
                }
                Err((elapsed_ms, e)) => {
                    self.calls.push((elapsed_ms, 0));
                    return Err(pyo3::exceptions::PyRuntimeError::new_err(e));
                }
            }
        }
        Ok(responses)
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

    // ── Batch: N concurrent requests, one PyO3 call ───────────────────────────

    #[pyo3(signature = (requests))]
    fn batch(
        &mut self,
        py: Python<'_>,
        requests: &Bound<'_, pyo3::types::PyList>,
    ) -> PyResult<Vec<Py<RustResponse>>> {
        self.do_batch(py, requests)
    }
}

// ── Metrics for one HTTP call ─────────────────────────────────────────────────

pub struct CallResult {
    pub elapsed_ms: u64,
    #[allow(dead_code)]
    pub status_code: u16,
    pub success: bool,
    pub error: Option<String>,
}

// ── VUser worker message ──────────────────────────────────────────────────────

pub enum VUserMsg {
    OnStart(oneshot::Sender<Result<()>>),
    Task {
        name: String,
        reply: oneshot::Sender<Result<Vec<CallResult>>>,
    },
    OnStop(oneshot::Sender<Result<()>>),
    Shutdown,
}

// ── PythonBridge ──────────────────────────────────────────────────────────────

pub struct PythonBridge {
    /// One channel sender per VUser. Tasks are sent here and results come
    /// back via the per-message oneshot. The sending side lives on tokio
    /// async tasks; the receiving side lives on dedicated OS threads.
    senders: Vec<mpsc::UnboundedSender<VUserMsg>>,
    pub has_on_start: bool,
    pub has_on_stop: bool,
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
        // Phase 1: Python setup — import scenario, detect callbacks, create instances.
        let (instances, has_on_start, has_on_stop) = Python::attach(|py| {
            // Pre-import the loadpilot package before adding the scenario directory
            // to sys.path to prevent a circular import when the scenario file is
            // itself named loadpilot.py.
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

            let instances: Vec<Py<PyAny>> = (0..n_vusers)
                .map(|_| cls.call0().map(|b| b.unbind()))
                .collect::<PyResult<_>>()?;

            Ok::<_, PyErr>((instances, has_on_start, has_on_stop))
        })
        .map_err(|e: PyErr| anyhow!("Python bridge init error: {}", e))?;

        // Phase 2: Spawn one OS thread per VUser.
        let mut senders = Vec::with_capacity(n_vusers);
        for (idx, instance) in instances.into_iter().enumerate() {
            let (tx, rx) = mpsc::unbounded_channel::<VUserMsg>();
            let base_url = base_url.to_string();
            let http_client = http_client.clone();
            let rt_handle = rt_handle.clone();

            std::thread::Builder::new()
                .name(format!("vuser-{}", idx))
                .spawn(move || {
                    vuser_thread_main(instance, rx, base_url, http_client, rt_handle)
                })
                .map_err(|e| anyhow!("failed to spawn VUser thread {}: {}", idx, e))?;

            senders.push(tx);
        }

        Ok(PythonBridge { senders, has_on_start, has_on_stop })
    }

    pub fn n_vusers(&self) -> usize {
        self.senders.len()
    }

    pub async fn call_on_start(&self, idx: usize) -> Result<()> {
        if !self.has_on_start {
            return Ok(());
        }
        let (tx, rx) = oneshot::channel();
        self.senders[idx]
            .send(VUserMsg::OnStart(tx))
            .map_err(|_| anyhow!("VUser {} thread exited before on_start", idx))?;
        rx.await
            .map_err(|_| anyhow!("VUser {} on_start reply dropped", idx))?
    }

    pub async fn run_task(&self, idx: usize, name: String) -> Result<Vec<CallResult>> {
        let (tx, rx) = oneshot::channel();
        self.senders[idx]
            .send(VUserMsg::Task { name, reply: tx })
            .map_err(|_| anyhow!("VUser {} thread exited", idx))?;
        rx.await
            .map_err(|_| anyhow!("VUser {} task reply dropped", idx))?
    }

    pub async fn call_on_stop(&self, idx: usize) -> Result<()> {
        if !self.has_on_stop {
            let _ = self.senders[idx].send(VUserMsg::Shutdown);
            return Ok(());
        }
        let (tx, rx) = oneshot::channel();
        self.senders[idx]
            .send(VUserMsg::OnStop(tx))
            .map_err(|_| anyhow!("VUser {} thread exited before on_stop", idx))?;
        rx.await
            .map_err(|_| anyhow!("VUser {} on_stop reply dropped", idx))?
    }

    /// Signal any VUser threads still alive to exit.
    /// Call this after all `call_on_stop` calls have been awaited.
    pub fn shutdown(&self) {
        for tx in &self.senders {
            let _ = tx.send(VUserMsg::Shutdown);
        }
    }
}

// ── VUser worker thread ───────────────────────────────────────────────────────

/// Entry point for each VUser's dedicated OS thread.
///
/// `Python::attach` is called per-message. A single `asyncio` event loop is
/// created once at thread start and reused across all tasks, so coroutine
/// startup overhead is paid once per VUser rather than per task.
fn vuser_thread_main(
    instance: Py<PyAny>,
    mut rx: mpsc::UnboundedReceiver<VUserMsg>,
    base_url: String,
    http_client: reqwest::Client,
    rt_handle: tokio::runtime::Handle,
) {
    let event_loop: Option<Py<PyAny>> = Python::attach(|py| {
        py.import("asyncio")
            .and_then(|m| m.call_method0("new_event_loop"))
            .map(|b| b.unbind())
            .ok()
    });

    loop {
        match rx.blocking_recv() {
            None | Some(VUserMsg::Shutdown) => break,

            Some(VUserMsg::OnStart(reply)) => {
                let result = Python::attach(|py| {
                    do_on_start(py, &instance, &base_url, event_loop.as_ref())
                });
                let _ = reply.send(result);
            }

            Some(VUserMsg::Task { name, reply }) => {
                let result = Python::attach(|py| {
                    do_run_task(
                        py,
                        &instance,
                        &name,
                        &base_url,
                        &http_client,
                        &rt_handle,
                        event_loop.as_ref(),
                    )
                });
                let _ = reply.send(result);
            }

            Some(VUserMsg::OnStop(reply)) => {
                let result = Python::attach(|py| {
                    do_on_stop(py, &instance, &base_url, event_loop.as_ref())
                });
                let _ = reply.send(result);
                break;
            }
        }
    }

    if let Some(ref loop_) = event_loop {
        Python::attach(|py| {
            let _ = loop_.call_method0(py, "close");
        });
    }
}

// ── Per-VUser lifecycle helpers ───────────────────────────────────────────────

fn do_on_start(
    py: Python<'_>,
    instance: &Py<PyAny>,
    base_url: &str,
    event_loop: Option<&Py<PyAny>>,
) -> Result<()> {
    let client = make_real_client(py, base_url)?;
    let call_result = instance.call_method1(py, "on_start", (client,))
        .map(|p| p.into_bound(py));
    run_maybe_coro(py, call_result, event_loop)
        .map_err(|e| anyhow!("on_start error: {}", e))
}

fn do_on_stop(
    py: Python<'_>,
    instance: &Py<PyAny>,
    base_url: &str,
    event_loop: Option<&Py<PyAny>>,
) -> Result<()> {
    let client = make_real_client(py, base_url)?;
    let call_result = instance.call_method1(py, "on_stop", (client,))
        .map(|p| p.into_bound(py));
    run_maybe_coro(py, call_result, event_loop)
        .map_err(|e| anyhow!("on_stop error: {}", e))
}

fn do_run_task(
    py: Python<'_>,
    instance: &Py<PyAny>,
    task_name: &str,
    base_url: &str,
    http_client: &reqwest::Client,
    rt_handle: &tokio::runtime::Handle,
    event_loop: Option<&Py<PyAny>>,
) -> Result<Vec<CallResult>> {
    let rust_client = Py::new(
        py,
        RustClient::new(base_url.to_string(), http_client.clone(), rt_handle.clone()),
    )
    .map_err(|e| anyhow!("failed to create RustClient: {}", e))?;

    let vuser = instance.bind(py);

    let call_result = vuser.call_method1(task_name, (rust_client.bind(py),));

    if let Err(e) = run_maybe_coro(py, call_result, event_loop) {
        return Ok(vec![CallResult {
            elapsed_ms: 0,
            status_code: 500,
            success: false,
            error: Some(e.to_string()),
        }]);
    }

    let rc = rust_client.borrow(py);
    let calls_data = rc.calls.clone();
    // Extract the data check_* needs as plain Python primitives — no PyO3 wrapper
    // objects, no descriptor-protocol overhead. check_*(self, status_code, body)
    // receives a Python int and a pre-built dict (or None), so the only Python
    // work left inside check_* is native comparisons and dict lookups.
    let last_status: Option<u16> = rc.last_response.as_ref()
        .map(|r| r.borrow(py).status_code);
    let last_json: Option<Py<PyAny>> = rc.last_response.as_ref()
        .and_then(|r| r.borrow(py).json_cache.as_ref().map(|j| j.clone_ref(py)));
    drop(rc);

    let check_name = format!("check_{}", task_name);
    let check_failed = if vuser.hasattr(check_name.as_str()).unwrap_or(false) {
        if let Some(status) = last_status {
            let status_py = status
                .into_pyobject(py)
                .map_err(|e| anyhow!("status into_pyobject: {}", e))?;
            let body_py: Py<PyAny> = last_json.unwrap_or_else(|| py.None());
            match vuser.call_method1(check_name.as_str(), (status_py, body_py)) {
                Ok(_) => false,
                Err(e) => {
                    eprintln!("[loadpilot] check_{} failed: {}", task_name, e);
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

    Ok(calls_data
        .into_iter()
        .enumerate()
        .map(|(i, (elapsed_ms, status))| {
            let success = if i == n - 1 && check_failed { false } else { status < 400 };
            CallResult { elapsed_ms, status_code: status, success, error: None }
        })
        .collect())
}

fn make_real_client<'py>(py: Python<'py>, base_url: &str) -> PyResult<Bound<'py, PyAny>> {
    let loadpilot = py.import("loadpilot")?;
    let client_cls = loadpilot.getattr("LoadClient")?;
    client_cls.call1((base_url,))
}

// ── Async helpers ─────────────────────────────────────────────────────────────

/// Drive `call_result` to completion: if the return value is a coroutine
/// (i.e. the method was `async def`), run it via the per-VUser event loop.
/// For sync methods the method has already executed and this is a no-op.
fn run_maybe_coro<'py>(
    py: Python<'py>,
    call_result: PyResult<Bound<'py, PyAny>>,
    event_loop: Option<&Py<PyAny>>,
) -> PyResult<()> {
    let ret = call_result?;
    let asyncio = py.import("asyncio")?;
    let is_coro: bool = asyncio
        .call_method1("iscoroutine", (&ret,))?
        .extract()?;
    if is_coro {
        // Fast path: drive the coroutine with a single send(None).
        // For async def bodies that contain no real `await` expressions
        // (only synchronous calls like RustClient.get / RustClient.batch),
        // the coroutine completes immediately and raises StopIteration —
        // no asyncio scheduling overhead at all (~10µs vs ~200µs).
        match ret.call_method1("send", (py.None(),)) {
            Err(e) if e.is_instance_of::<pyo3::exceptions::PyStopIteration>(py) => {
                // Coroutine completed in one step — the common case.
            }
            Ok(_yielded) => {
                // Coroutine has real awaits — fall back to the event loop.
                if let Some(loop_) = event_loop {
                    loop_.call_method1(py, "run_until_complete", (&ret,))?;
                } else {
                    asyncio.call_method1("run", (&ret,))?;
                }
            }
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

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
            c if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~') => vec![c],
            c => format!("%{:02X}", c as u32).chars().collect::<Vec<_>>(),
        })
        .collect()
}

/// Parse a response body as JSON if it looks like an object or array.
/// Called inside `py.detach()` — pure Rust, no GIL needed.
fn parse_json_body(body: &str) -> Option<serde_json::Value> {
    let trimmed = body.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str(body).ok()
    } else {
        None
    }
}

fn json_to_py(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
    match value {
        serde_json::Value::Null => Ok(py.None()),
        serde_json::Value::Bool(b) => {
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── run_maybe_coro ────────────────────────────────────────────────────────

    #[test]
    fn sync_function_returns_none_is_noop() {
        Python::attach(|py| {
            // A regular (non-async) call returns None — run_maybe_coro is a no-op.
            let none_val = py.None().into_bound(py);
            let result = run_maybe_coro(py, Ok(none_val), None);
            assert!(result.is_ok());
        });
    }

    #[test]
    fn sync_body_async_fn_completes_without_event_loop() {
        Python::attach(|py| {
            // async def f(): return 42  — no real awaits; runs via asyncio.run
            let locals = PyDict::new(py);
            py.run(
                pyo3::ffi::c_str!("async def f(): return 42\ncoro = f()"),
                None,
                Some(&locals),
            )
            .unwrap();
            let coro = locals.get_item("coro").unwrap().unwrap();
            // No event_loop → asyncio.run() is used
            let result = run_maybe_coro(py, Ok(coro), None);
            assert!(result.is_ok());
        });
    }

    #[test]
    fn async_fn_with_await_uses_event_loop() {
        Python::attach(|py| {
            // async def f(): await asyncio.sleep(0) — has a real await.
            // Use a shared globals dict so that f.__globals__ contains asyncio.
            let globals = PyDict::new(py);
            py.run(
                pyo3::ffi::c_str!(
                    "import asyncio\nasync def f(): await asyncio.sleep(0)\ncoro = f()"
                ),
                Some(&globals),
                None,
            )
            .unwrap();
            let coro = globals.get_item("coro").unwrap().unwrap();
            let loop_ = py
                .import("asyncio")
                .unwrap()
                .call_method0("new_event_loop")
                .unwrap()
                .unbind();
            let result = run_maybe_coro(py, Ok(coro), Some(&loop_));
            let _ = loop_.call_method0(py, "close");
            assert!(result.is_ok(), "run_maybe_coro failed: {:?}", result);
        });
    }

    #[test]
    fn propagates_error_from_async_fn_body() {
        Python::attach(|py| {
            let locals = PyDict::new(py);
            py.run(
                pyo3::ffi::c_str!(
                    "async def f(): raise ValueError('boom')\ncoro = f()"
                ),
                None,
                Some(&locals),
            )
            .unwrap();
            let coro = locals.get_item("coro").unwrap().unwrap();
            let result = run_maybe_coro(py, Ok(coro), None);
            assert!(result.is_err());
            let err_str = result.unwrap_err().to_string();
            assert!(err_str.contains("boom"), "expected 'boom' in: {err_str}");
        });
    }

    #[test]
    fn propagates_call_error_without_touching_coro() {
        Python::attach(|py| {
            let err = pyo3::exceptions::PyRuntimeError::new_err("call failed");
            let result = run_maybe_coro(py, Err(err), None);
            assert!(result.is_err());
        });
    }

    // ── json_to_py ────────────────────────────────────────────────────────────

    #[test]
    fn json_null_becomes_none() {
        Python::attach(|py| {
            let v = json_to_py(py, &serde_json::Value::Null).unwrap();
            assert!(v.bind(py).is_none());
        });
    }

    #[test]
    fn json_bool_roundtrips() {
        Python::attach(|py| {
            let t = json_to_py(py, &serde_json::Value::Bool(true)).unwrap();
            let f = json_to_py(py, &serde_json::Value::Bool(false)).unwrap();
            assert!(t.bind(py).extract::<bool>().unwrap());
            assert!(!f.bind(py).extract::<bool>().unwrap());
        });
    }

    #[test]
    fn json_integer_roundtrips() {
        Python::attach(|py| {
            let v = json_to_py(py, &serde_json::json!(42)).unwrap();
            assert_eq!(v.bind(py).extract::<i64>().unwrap(), 42);
        });
    }

    #[test]
    fn json_float_roundtrips() {
        Python::attach(|py| {
            let v = json_to_py(py, &serde_json::json!(3.14)).unwrap();
            let f: f64 = v.bind(py).extract().unwrap();
            assert!((f - 3.14).abs() < 1e-10);
        });
    }

    #[test]
    fn json_string_roundtrips() {
        Python::attach(|py| {
            let v = json_to_py(py, &serde_json::json!("hello")).unwrap();
            assert_eq!(v.bind(py).extract::<String>().unwrap(), "hello");
        });
    }

    #[test]
    fn json_array_roundtrips() {
        Python::attach(|py| {
            let v = json_to_py(py, &serde_json::json!([1, 2, 3])).unwrap();
            let list: Vec<i64> = v.bind(py).extract().unwrap();
            assert_eq!(list, vec![1, 2, 3]);
        });
    }

    #[test]
    fn json_object_roundtrips() {
        Python::attach(|py| {
            let v = json_to_py(py, &serde_json::json!({"id": 1, "name": "bench"})).unwrap();
            let dict = v.bind(py).cast::<PyDict>().unwrap();
            let id: i64 = dict.get_item("id").unwrap().unwrap().extract().unwrap();
            let name: String = dict.get_item("name").unwrap().unwrap().extract().unwrap();
            assert_eq!(id, 1);
            assert_eq!(name, "bench");
        });
    }

    #[test]
    fn json_nested_object_roundtrips() {
        Python::attach(|py| {
            let v = json_to_py(
                py,
                &serde_json::json!({"user": {"id": 1, "roles": ["admin"]}}),
            )
            .unwrap();
            let dict = v.bind(py).cast::<PyDict>().unwrap();
            let user = dict.get_item("user").unwrap().unwrap();
            let user_dict = user.cast::<PyDict>().unwrap();
            let id: i64 = user_dict.get_item("id").unwrap().unwrap().extract().unwrap();
            assert_eq!(id, 1);
        });
    }

    // ── RustResponse ──────────────────────────────────────────────────────────

    #[test]
    fn rust_response_json_parses_object() {
        Python::attach(|py| {
            let resp = Py::new(
                py,
                RustResponse {
                    status_code: 200,
                    ok: true,
                    text: r#"{"id":1,"name":"bench"}"#.to_string(),
                    headers_map: HashMap::new(),
                    json_cache: None,
                },
            )
            .unwrap();
            let result = resp.borrow(py).json(py).unwrap();
            let dict = result.bind(py).cast::<PyDict>().unwrap();
            let id: i64 = dict.get_item("id").unwrap().unwrap().extract().unwrap();
            assert_eq!(id, 1);
        });
    }

    #[test]
    fn rust_response_json_errors_on_invalid() {
        Python::attach(|py| {
            let resp = Py::new(
                py,
                RustResponse {
                    status_code: 200,
                    ok: true,
                    text: "not json".to_string(),
                    headers_map: HashMap::new(),
                    json_cache: None,
                },
            )
            .unwrap();
            assert!(resp.borrow(py).json(py).is_err());
        });
    }

    #[test]
    fn rust_response_raise_for_status_ok() {
        Python::attach(|py| {
            let resp = Py::new(
                py,
                RustResponse {
                    status_code: 200,
                    ok: true,
                    text: String::new(),
                    headers_map: HashMap::new(),
                    json_cache: None,
                },
            )
            .unwrap();
            assert!(resp.borrow(py).raise_for_status().is_ok());
        });
    }

    #[test]
    fn rust_response_raise_for_status_error() {
        Python::attach(|py| {
            let resp = Py::new(
                py,
                RustResponse {
                    status_code: 404,
                    ok: false,
                    text: String::new(),
                    headers_map: HashMap::new(),
                    json_cache: None,
                },
            )
            .unwrap();
            assert!(resp.borrow(py).raise_for_status().is_err());
        });
    }

    // ── do_batch ──────────────────────────────────────────────────────────────

    fn make_test_client(base_url: &str) -> (tokio::runtime::Runtime, RustClient) {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let client = RustClient::new(
            base_url.to_string(),
            reqwest::Client::new(),
            rt.handle().clone(),
        );
        (rt, client)
    }

    #[test]
    fn batch_empty_list_returns_empty() {
        Python::attach(|py| {
            let (_rt, mut client) = make_test_client("http://localhost:9999");
            let requests = pyo3::types::PyList::empty(py);
            let result = client.do_batch(py, &requests);
            assert!(result.is_ok());
            assert!(result.unwrap().is_empty());
        });
    }

    #[test]
    fn batch_item_missing_path_returns_error() {
        Python::attach(|py| {
            let (_rt, mut client) = make_test_client("http://localhost:9999");
            // Dict with no "path" key.
            let d = PyDict::new(py);
            let requests = pyo3::types::PyList::empty(py);
            requests.append(d).unwrap();
            let result = client.do_batch(py, &requests);
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(msg.contains("missing 'path'"), "unexpected error: {msg}");
        });
    }

    #[test]
    fn batch_item_not_a_dict_returns_error() {
        Python::attach(|py| {
            let (_rt, mut client) = make_test_client("http://localhost:9999");
            // Item is a string, not a dict.
            let requests = pyo3::types::PyList::empty(py);
            requests.append("not a dict").unwrap();
            let result = client.do_batch(py, &requests);
            assert!(result.is_err());
        });
    }

    #[test]
    fn batch_default_method_is_get() {
        // Verify that omitting "method" defaults to GET without raising an error
        // (the actual HTTP call would fail against a non-running server, so we
        // only test that parse succeeds by checking no parse-phase error is raised
        // before py.detach).
        //
        // We use a port that is not listening so the HTTP error comes back from
        // the network layer, not from our argument parser — confirming that the
        // method default logic ran successfully.
        Python::attach(|py| {
            let (_rt, mut client) = make_test_client("http://127.0.0.1:19999");
            let d = PyDict::new(py);
            d.set_item("path", "/ping").unwrap();
            // No "method" key — should default to GET.
            let requests = pyo3::types::PyList::empty(py);
            requests.append(d).unwrap();
            let result = client.do_batch(py, &requests);
            // Connection refused → RuntimeError from the network, not a parse error.
            assert!(result.is_err());
            let msg = result.unwrap_err().to_string();
            assert!(!msg.contains("missing 'path'"), "got parse error, not network: {msg}");
        });
    }
}
