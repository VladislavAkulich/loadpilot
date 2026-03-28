/// PyO3 bridge — calls Python scenario callbacks from the Rust coordinator.
///
/// Architecture: one dedicated OS thread per VUser.
///   • `Python::attach` is called ONCE per VUser thread and kept active for
///     the entire test duration — no per-task Python thread-state overhead.
///   • The coordinator sends task requests through an `mpsc` channel and
///     awaits a `oneshot` reply — the tokio executor is never blocked.
///   • HTTP I/O inside RustClient calls `handle.block_on(...)` directly on
///     the VUser OS thread (valid; the thread is not a tokio worker thread).
mod client;
mod response;
mod vuser;

use anyhow::{anyhow, Result};
use pyo3::prelude::*;
use tokio::sync::mpsc;

use vuser::{vuser_thread_main, CallResult, VUserMsg};

// ── PythonBridge ───────────────────────────────────────────────────────────────

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
                .spawn(move || vuser_thread_main(instance, rx, base_url, http_client, rt_handle))
                .map_err(|e| anyhow!("failed to spawn VUser thread {}: {}", idx, e))?;

            senders.push(tx);
        }

        Ok(PythonBridge {
            senders,
            has_on_start,
            has_on_stop,
        })
    }

    pub fn n_vusers(&self) -> usize {
        self.senders.len()
    }

    pub async fn call_on_start(&self, idx: usize) -> Result<()> {
        if !self.has_on_start {
            return Ok(());
        }
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.senders[idx]
            .send(VUserMsg::OnStart(tx))
            .map_err(|_| anyhow!("VUser {} thread exited before on_start", idx))?;
        rx.await
            .map_err(|_| anyhow!("VUser {} on_start reply dropped", idx))?
    }

    pub async fn run_task(&self, idx: usize, name: String) -> Result<Vec<CallResult>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
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
        let (tx, rx) = tokio::sync::oneshot::channel();
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
