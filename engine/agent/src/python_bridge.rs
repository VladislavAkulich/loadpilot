#![allow(dead_code)]
/// PyO3 Python bridge — v2 placeholder.
///
/// In v2, this module will use PyO3 to call Python task callbacks from within
/// the Rust worker hot path, enabling dynamic data generation and stateful
/// virtual users without pre-generating a data pool.
///
/// For the MVP the bridge is not compiled (no pyo3 dependency) — Rust workers
/// execute static HTTP requests defined in the ScenarioPlan JSON.

/// Placeholder type representing a Python callable task.
/// In v2 this will be `pyo3::PyObject`.
pub struct PythonTask {
    pub name: String,
}

impl PythonTask {
    /// Invoke the Python task callback (v2).
    ///
    /// Returns the HTTP request parameters extracted from the Python call.
    pub async fn invoke(&self) -> anyhow::Result<PythonTaskResult> {
        // v2: acquire GIL, call Python function, extract request params
        Err(anyhow::anyhow!("Python bridge not available in MVP"))
    }
}

pub struct PythonTaskResult {
    pub method: String,
    pub url: String,
    pub headers: std::collections::HashMap<String, String>,
    pub body: Option<String>,
}
