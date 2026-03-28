/// VUser thread types, lifecycle helpers, and async utilities.
use anyhow::{anyhow, Result};
use pyo3::prelude::*;
use tokio::sync::oneshot;

use super::client::RustClient;

// ── Metrics for one HTTP call ──────────────────────────────────────────────────

pub struct CallResult {
    pub elapsed_ms: u64,
    #[allow(dead_code)]
    pub status_code: u16,
    pub success: bool,
    pub error: Option<String>,
}

// ── VUser worker message ───────────────────────────────────────────────────────

pub(super) enum VUserMsg {
    OnStart(oneshot::Sender<Result<()>>),
    Task {
        name: String,
        reply: oneshot::Sender<Result<Vec<CallResult>>>,
    },
    OnStop(oneshot::Sender<Result<()>>),
    Shutdown,
}

// ── VUser worker thread ────────────────────────────────────────────────────────

/// Entry point for each VUser's dedicated OS thread.
///
/// `Python::attach` is called per-message. A single `asyncio` event loop is
/// created once at thread start and reused across all tasks, so coroutine
/// startup overhead is paid once per VUser rather than per task.
pub(super) fn vuser_thread_main(
    instance: Py<PyAny>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<VUserMsg>,
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
                let result =
                    Python::attach(|py| do_on_start(py, &instance, &base_url, event_loop.as_ref()));
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
                let result =
                    Python::attach(|py| do_on_stop(py, &instance, &base_url, event_loop.as_ref()));
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

// ── Per-VUser lifecycle helpers ────────────────────────────────────────────────

fn do_on_start(
    py: Python<'_>,
    instance: &Py<PyAny>,
    base_url: &str,
    event_loop: Option<&Py<PyAny>>,
) -> Result<()> {
    let client = make_real_client(py, base_url)?;
    let call_result = instance
        .call_method1(py, "on_start", (client,))
        .map(|p| p.into_bound(py));
    run_maybe_coro(py, call_result, event_loop).map_err(|e| anyhow!("on_start error: {}", e))
}

fn do_on_stop(
    py: Python<'_>,
    instance: &Py<PyAny>,
    base_url: &str,
    event_loop: Option<&Py<PyAny>>,
) -> Result<()> {
    let client = make_real_client(py, base_url)?;
    let call_result = instance
        .call_method1(py, "on_stop", (client,))
        .map(|p| p.into_bound(py));
    run_maybe_coro(py, call_result, event_loop).map_err(|e| anyhow!("on_stop error: {}", e))
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
    let last_status: Option<u16> = rc.last_response.as_ref().map(|r| r.borrow(py).status_code);
    let last_json: Option<Py<PyAny>> = rc
        .last_response
        .as_ref()
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
                    tracing::warn!(task = task_name, error = %e, "check failed");
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
            let success = if i == n - 1 && check_failed {
                false
            } else {
                status < 400
            };
            CallResult {
                elapsed_ms,
                status_code: status,
                success,
                error: None,
            }
        })
        .collect())
}

fn make_real_client<'py>(py: Python<'py>, base_url: &str) -> PyResult<Bound<'py, PyAny>> {
    let loadpilot = py.import("loadpilot")?;
    let client_cls = loadpilot.getattr("LoadClient")?;
    client_cls.call1((base_url,))
}

// ── Async helpers ──────────────────────────────────────────────────────────────

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
    let is_coro: bool = asyncio.call_method1("iscoroutine", (&ret,))?.extract()?;
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

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use pyo3::prelude::*;
    use pyo3::types::PyDict;

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
                pyo3::ffi::c_str!("async def f(): raise ValueError('boom')\ncoro = f()"),
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
}
