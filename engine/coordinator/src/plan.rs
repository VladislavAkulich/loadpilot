/// Shared plan structures deserialized from the JSON produced by the Python CLI.
use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Constant,
    #[default]
    Ramp,
    Step,
    Spike,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskPlan {
    pub name: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
    pub url: String,
    #[serde(default = "default_method")]
    pub method: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body_template: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_plan_json(extra: &str) -> String {
        format!(
            r#"{{"name":"S","rps":10,"duration_secs":60,"ramp_up_secs":10,"target_url":"http://localhost"{}}}"#,
            extra
        )
    }

    #[test]
    fn deserialize_minimal_plan() {
        let json = minimal_plan_json("");
        let plan: ScenarioPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan.name, "S");
        assert_eq!(plan.rps, 10);
        assert_eq!(plan.duration_secs, 60);
        assert_eq!(plan.ramp_up_secs, 10);
        assert_eq!(plan.target_url, "http://localhost");
        assert!(plan.tasks.is_empty());
        assert!(plan.scenario_file.is_none());
    }

    #[test]
    fn deserialize_plan_with_tasks() {
        let json = minimal_plan_json(r#","tasks":[{"name":"ping","url":"/ping","method":"GET"}]"#);
        let plan: ScenarioPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan.tasks.len(), 1);
        assert_eq!(plan.tasks[0].name, "ping");
        assert_eq!(plan.tasks[0].method, "GET");
        assert_eq!(plan.tasks[0].weight, 1); // default
    }

    #[test]
    fn deserialize_task_default_method_is_get() {
        let json = minimal_plan_json(r#","tasks":[{"name":"t","url":"/"}]"#);
        let plan: ScenarioPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan.tasks[0].method, "GET");
    }

    #[test]
    fn deserialize_plan_with_bridge_fields() {
        let json = minimal_plan_json(
            r#","scenario_file":"/a/b.py","scenario_class":"MyFlow","n_vusers":5"#,
        );
        let plan: ScenarioPlan = serde_json::from_str(&json).unwrap();
        assert_eq!(plan.scenario_file.unwrap(), "/a/b.py");
        assert_eq!(plan.scenario_class.unwrap(), "MyFlow");
        assert_eq!(plan.n_vusers.unwrap(), 5);
    }

    #[test]
    fn deserialize_invalid_json_fails() {
        let result: Result<ScenarioPlan, _> = serde_json::from_str("not json");
        assert!(result.is_err());
    }

    #[test]
    fn roundtrip_serialize_deserialize() {
        let json =
            minimal_plan_json(r#","tasks":[{"name":"t","url":"/","method":"POST","weight":3}]"#);
        let plan: ScenarioPlan = serde_json::from_str(&json).unwrap();
        let re_serialized = serde_json::to_string(&plan).unwrap();
        let plan2: ScenarioPlan = serde_json::from_str(&re_serialized).unwrap();
        assert_eq!(plan2.tasks[0].weight, 3);
        assert_eq!(plan2.tasks[0].method, "POST");
    }
}

fn default_weight() -> u32 {
    1
}

fn default_method() -> String {
    "GET".to_string()
}

/// Per-VUser pre-authenticated header set extracted by the coordinator's on_start
/// pre-auth pool. Agents rotate through these in pure Rust HTTP.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct VUserConfig {
    /// task_name → headers map produced after running on_start + task mock-probe.
    #[serde(default)]
    pub task_headers: HashMap<String, HashMap<String, String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ScenarioPlan {
    pub name: String,
    pub rps: u64,
    pub duration_secs: u64,
    pub ramp_up_secs: u64,
    #[serde(default)]
    pub mode: Mode,
    pub target_url: String,
    #[serde(default)]
    pub tasks: Vec<TaskPlan>,
    // PyO3 bridge — present only when the scenario has Python lifecycle callbacks.
    #[serde(default)]
    pub scenario_file: Option<String>,
    #[serde(default)]
    pub scenario_class: Option<String>,
    #[serde(default)]
    pub n_vusers: Option<u64>,
    /// Pre-auth pool for distributed mode. Each entry holds per-task headers
    /// produced by running on_start on the coordinator side.
    #[serde(default)]
    pub vuser_configs: Vec<VUserConfig>,
    /// Number of load steps for mode=step (default 5).
    #[serde(default = "default_steps")]
    pub steps: u64,
}

fn default_steps() -> u64 {
    5
}
