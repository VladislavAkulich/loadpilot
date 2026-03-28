/// `RustClient` — HTTP client exposed to Python task methods via PyO3.
use std::collections::HashMap;
use std::time::Instant;

use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::plan::HttpMethod;

use super::response::{json_to_py, parse_json_body, RustResponse};

#[pyclass]
pub struct RustClient {
    base_url: String,
    http_client: reqwest::Client,
    rt_handle: tokio::runtime::Handle,
    pub calls: Vec<(u64, u16)>,
    pub(crate) last_response: Option<Py<RustResponse>>,
}

impl RustClient {
    pub fn new(
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
        let p = if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{}", path)
        };
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
        let http_method = HttpMethod::try_from(method)
            .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;

        let t0 = Instant::now();
        // Release the GIL while waiting for the HTTP response so other VUser
        // threads can run their Python code concurrently. All captured values
        // are Send Rust types — no Python objects cross the allow_threads boundary.
        let http_result = py.detach(|| {
            handle.block_on(async move {
                let mut req = match http_method {
                    HttpMethod::Get => client.get(&url),
                    HttpMethod::Post => client.post(&url),
                    HttpMethod::Put => client.put(&url),
                    HttpMethod::Patch => client.patch(&url),
                    HttpMethod::Delete => client.delete(&url),
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
            })
        });

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
                let response = Py::new(py, RustResponse::new(status, resp_headers, body_text, json_cache))?;
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
    pub(crate) fn do_batch(
        &mut self,
        py: Python<'_>,
        requests: &Bound<'_, pyo3::types::PyList>,
    ) -> PyResult<Vec<Py<RustResponse>>> {
        // Parse all request specs while holding the GIL — nothing async yet.
        struct ReqSpec {
            method: HttpMethod,
            url: String,
            headers: HashMap<String, String>,
            body: Option<String>,
            is_form: bool,
        }

        let specs: Vec<ReqSpec> = requests
            .iter()
            .map(|item| {
                let d = item.cast::<PyDict>()?;
                let method_str = d
                    .get_item("method")?
                    .map(|v| v.extract::<String>())
                    .transpose()?
                    .unwrap_or_else(|| "GET".to_string());
                let method = HttpMethod::try_from(method_str.as_str())
                    .map_err(|e| pyo3::exceptions::PyValueError::new_err(e.to_string()))?;
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

                Ok(ReqSpec {
                    method,
                    url,
                    headers,
                    body,
                    is_form,
                })
            })
            .collect::<PyResult<_>>()?;

        let client = self.http_client.clone();
        let handle = self.rt_handle.clone();

        type HttpOutcome = Result<
            (
                u64,
                u16,
                HashMap<String, String>,
                String,
                Option<serde_json::Value>,
            ),
            (u64, String),
        >;

        // Release the GIL for all concurrent HTTP. All captured values are Send
        // Rust types — no Python objects cross the py.detach boundary.
        let outcomes: Vec<HttpOutcome> = py.detach(|| {
            handle.block_on(async move {
                let mut set = tokio::task::JoinSet::new();
                for spec in specs {
                    let c = client.clone();
                    set.spawn(async move {
                        let t0 = Instant::now();
                        let mut req = match spec.method {
                            HttpMethod::Get => c.get(&spec.url),
                            HttpMethod::Post => c.post(&spec.url),
                            HttpMethod::Put => c.put(&spec.url),
                            HttpMethod::Patch => c.patch(&spec.url),
                            HttpMethod::Delete => c.delete(&spec.url),
                        };
                        for (k, v) in &spec.headers {
                            req = req.header(k.as_str(), v.as_str());
                        }
                        if let Some(ref b) = spec.body {
                            req = if spec.is_form {
                                req.header("Content-Type", "application/x-www-form-urlencoded")
                                    .body(b.clone())
                            } else {
                                req.header("Content-Type", "application/json")
                                    .body(b.clone())
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
                    let resp = Py::new(py, RustResponse::new(status, resp_headers, body_text, json_cache))?;
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

// ── HTTP kwargs helpers ────────────────────────────────────────────────────────

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

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use pyo3::prelude::*;
    use pyo3::types::PyDict;

    use super::*;

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
            assert!(
                !msg.contains("missing 'path'"),
                "got parse error, not network: {msg}"
            );
        });
    }
}
