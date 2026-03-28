/// `RustResponse` — HTTP response object exposed to Python task methods.
/// Also provides JSON utilities used by the rest of the bridge.
use std::collections::HashMap;

use pyo3::prelude::*;
use pyo3::types::PyDict;

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
    pub(crate) json_cache: Option<Py<PyAny>>,
}

impl RustResponse {
    pub(crate) fn new(
        status_code: u16,
        headers_map: HashMap<String, String>,
        text: String,
        json_cache: Option<Py<PyAny>>,
    ) -> Self {
        RustResponse {
            status_code,
            ok: status_code < 400,
            text,
            headers_map,
            json_cache,
        }
    }
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

/// Parse a response body as JSON if it looks like an object or array.
/// Called inside `py.detach()` — pure Rust, no GIL needed.
pub(crate) fn parse_json_body(body: &str) -> Option<serde_json::Value> {
    let trimmed = body.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        serde_json::from_str(body).ok()
    } else {
        None
    }
}

pub(crate) fn json_to_py(py: Python<'_>, value: &serde_json::Value) -> PyResult<Py<PyAny>> {
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
                Ok(n.as_f64()
                    .unwrap_or(0.0)
                    .into_pyobject(py)?
                    .into_any()
                    .unbind())
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use pyo3::prelude::*;
    use pyo3::types::PyDict;

    use super::*;

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
            let id: i64 = user_dict
                .get_item("id")
                .unwrap()
                .unwrap()
                .extract()
                .unwrap();
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
}
