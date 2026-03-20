/// PyO3 bridge — calls Python scenario callbacks from the Rust coordinator.
///
/// Lifecycle:
///   1. `PythonBridge::new` loads the scenario file, instantiates N VUsers.
///   2. `call_on_start(i)` runs `vuser.on_start(real_LoadClient)` for VUser i.
///   3. `prepare_task(i, "task_name")` calls the task method with a MockClient,
///      captures (method, url, headers, body) and returns them as `RequestParams`.
///   4. `call_on_stop(i)` runs `vuser.on_stop(real_LoadClient)` for VUser i.
///
/// All Python calls acquire the GIL for their duration only; HTTP execution
/// happens in async reqwest without the GIL held.
use std::collections::HashMap;

use anyhow::{anyhow, Result};
use pyo3::prelude::*;

/// Parameters for a single HTTP request, extracted from a Python task callback.
pub struct RequestParams {
    pub method: String,
    pub url: String,
    pub headers: HashMap<String, String>,
    pub body: Option<String>,
}

/// Result of running a Python check method after the HTTP response is received.
pub struct CheckResult {
    /// false if an exception (e.g. AssertionError) was raised in check_{task_name}.
    pub success: bool,
    pub error: Option<String>,
}

struct VUser {
    instance: Py<PyAny>,
}

pub struct PythonBridge {
    vusers: Vec<VUser>,
    mock_client_cls: Py<PyAny>,
    base_url: String,
    has_on_start: bool,
    has_on_stop: bool,
}

impl PythonBridge {
    /// Load the scenario file, instantiate `n_vusers` VUser objects.
    pub fn new(
        scenario_file: &str,
        scenario_class: &str,
        n_vusers: usize,
        base_url: &str,
    ) -> Result<Self> {
        Python::with_gil(|py| {
            // Make the scenario's directory importable (mirrors Python CLI behaviour).
            let sys = py.import("sys")?;
            let path = sys.getattr("path")?;
            let dir = std::path::Path::new(scenario_file)
                .parent()
                .and_then(|p| p.to_str())
                .unwrap_or(".");
            path.call_method1("insert", (0, dir))?;

            // Load the scenario module via importlib.
            let util = py.import("importlib.util")?;
            let spec =
                util.call_method1("spec_from_file_location", ("_loadpilot_scenario", scenario_file))?;
            let module = util.call_method1("module_from_spec", (&spec,))?;
            spec.getattr("loader")?
                .call_method1("exec_module", (&module,))?;

            // Retrieve the concrete scenario class.
            let cls = module.getattr(scenario_class)?;

            // Inspect whether on_start / on_stop are overridden in the class.
            let cls_dict = cls.getattr("__dict__")?;
            let has_on_start = cls_dict.contains("on_start")?;
            let has_on_stop = cls_dict.contains("on_stop")?;

            // Load the MockClient from the loadpilot._bridge module.
            let bridge_mod = py.import("loadpilot._bridge")?;
            let mock_client_cls: Py<PyAny> = bridge_mod.getattr("MockClient")?.into_py(py);

            // Instantiate all VUsers upfront; on_start is called separately.
            let mut vusers = Vec::with_capacity(n_vusers);
            for _ in 0..n_vusers {
                let instance: Py<PyAny> = cls.call0()?.into_py(py);
                vusers.push(VUser { instance });
            }

            Ok(PythonBridge {
                vusers,
                mock_client_cls,
                base_url: base_url.to_string(),
                has_on_start,
                has_on_stop,
            })
        })
        .map_err(|e: PyErr| anyhow!("Python bridge init error: {}", e))
    }

    pub fn n_vusers(&self) -> usize {
        self.vusers.len()
    }

    /// Call `on_start(client)` for VUser at `idx`, using a real LoadClient
    /// so the callback can make actual HTTP requests (e.g. login).
    pub fn call_on_start(&self, idx: usize) -> Result<()> {
        if !self.has_on_start {
            return Ok(());
        }
        Python::with_gil(|py| {
            let client = self.make_real_client(py)?;
            self.vusers[idx]
                .instance
                .call_method1(py, "on_start", (client,))?;
            Ok(())
        })
        .map_err(|e: PyErr| anyhow!("on_start error (VUser {}): {}", idx, e))
    }

    /// Call `on_stop(client)` for VUser at `idx`.
    pub fn call_on_stop(&self, idx: usize) -> Result<()> {
        if !self.has_on_stop {
            return Ok(());
        }
        Python::with_gil(|py| {
            let client = self.make_real_client(py)?;
            self.vusers[idx]
                .instance
                .call_method1(py, "on_stop", (client,))?;
            Ok(())
        })
        .map_err(|e: PyErr| anyhow!("on_stop error (VUser {}): {}", idx, e))
    }

    /// Call `task_name(vuser, mock_client)` and return the captured HTTP params.
    ///
    /// The MockClient intercepts the first HTTP call the task makes and records
    /// its method, path, headers and body. Rust executes the real request.
    pub fn prepare_task(&self, vuser_idx: usize, task_name: &str) -> Result<RequestParams> {
        Python::with_gil(|py| {
            let mock = self.mock_client_cls.call0(py)?;

            self.vusers[vuser_idx]
                .instance
                .call_method1(py, task_name, (mock.bind(py),))?;

            // get_call() → (method, path, headers, body)
            let call = mock.call_method0(py, "get_call")?;
            let (method, path, headers, body): (
                Option<String>,
                Option<String>,
                HashMap<String, String>,
                Option<String>,
            ) = call.extract(py)?;

            let method = method.unwrap_or_else(|| "GET".to_string());
            let path = path.unwrap_or_else(|| "/".to_string());
            let url = format!("{}{}", self.base_url.trim_end_matches('/'), path);

            Ok(RequestParams { method, url, headers, body })
        })
        .map_err(|e: PyErr| {
            anyhow!("Task '{}' error (VUser {}): {}", task_name, vuser_idx, e)
        })
    }

    /// Called after reqwest executes the HTTP request.  Passes the real response
    /// to an optional `check_{task_name}(self, response)` method on the VUser.
    ///
    /// If no check method is defined the result is derived from the HTTP status
    /// code alone (status < 400 → success), preserving the existing behaviour.
    pub fn run_check(
        &self,
        vuser_idx: usize,
        task_name: &str,
        status_code: u16,
        headers: HashMap<String, String>,
        body: String,
    ) -> Result<CheckResult> {
        Python::with_gil(|py| {
            let check_name = format!("check_{}", task_name);
            // Bind to get the Bound<'py> API — hasattr/call_method1 behave
            // exactly like their Python counterparts without GIL surprises.
            let vuser = self.vusers[vuser_idx].instance.bind(py);

            // hasattr returns Ok(false) when the attribute is missing (never
            // raises), so this is safe even for classes with no check methods.
            let has_check = vuser.hasattr(check_name.as_str())?;
            if !has_check {
                return Ok(CheckResult {
                    success: status_code < 400,
                    error: None,
                });
            }

            // Build a CheckResponse Python object from the real response data.
            let bridge_mod = py.import("loadpilot._bridge")?;
            let check_resp_cls = bridge_mod.getattr("CheckResponse")?;
            let py_headers = headers.into_py(py);
            let resp = check_resp_cls.call1((status_code as i64, py_headers, body))?;

            match vuser.call_method1(check_name.as_str(), (resp,)) {
                Ok(_) => Ok(CheckResult { success: true, error: None }),
                Err(e) => Ok(CheckResult {
                    success: false,
                    error: Some(e.to_string()),
                }),
            }
        })
        .map_err(|e: PyErr| {
            anyhow!(
                "run_check error (VUser {}, task '{}'): {}",
                vuser_idx,
                task_name,
                e
            )
        })
    }

    // ---- private helpers ----

    fn make_real_client<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let loadpilot = py.import("loadpilot")?;
        let client_cls = loadpilot.getattr("LoadClient")?;
        client_cls.call1((&self.base_url,))
    }
}
