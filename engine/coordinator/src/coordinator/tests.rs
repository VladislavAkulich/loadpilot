use super::scheduler::{compute_phase_and_rps, pick_task};
use super::Phase;
use crate::plan::{HttpMethod, Mode, ScenarioPlan, TaskPlan};
use std::collections::HashMap;
use std::sync::Arc;

fn make_task(name: &str, weight: u32) -> TaskPlan {
    TaskPlan {
        name: name.to_string(),
        weight,
        url: "/".to_string(),
        method: HttpMethod::Get,
        headers: Arc::new(HashMap::new()),
        body_template: None,
    }
}

// ── compute_phase_and_rps ─────────────────────────────────────────────────

fn plan_for_mode(
    mode: Mode,
    rps: u64,
    duration: u64,
    ramp_up: u64,
    steps: u64,
) -> ScenarioPlan {
    ScenarioPlan {
        name: "T".into(),
        rps,
        duration_secs: duration,
        ramp_up_secs: ramp_up,
        mode,
        target_url: "http://localhost".into(),
        tasks: vec![],
        scenario_file: None,
        scenario_class: None,
        n_vusers: None,
        vuser_configs: vec![],
        steps,
    }
}

#[test]
fn ramp_before_ramp_scales_linearly() {
    let plan = plan_for_mode(Mode::Ramp, 100, 60, 10, 5);
    let (phase, rps) = compute_phase_and_rps(5.0, &plan);
    assert_eq!(phase, Phase::RampUp);
    assert!((rps - 50.0).abs() < 0.01);
}

#[test]
fn ramp_after_ramp_is_full() {
    let plan = plan_for_mode(Mode::Ramp, 100, 60, 10, 5);
    let (phase, rps) = compute_phase_and_rps(15.0, &plan);
    assert_eq!(phase, Phase::Steady);
    assert_eq!(rps, 100.0);
}

#[test]
fn ramp_start_is_zero() {
    let plan = plan_for_mode(Mode::Ramp, 100, 60, 10, 5);
    let (phase, rps) = compute_phase_and_rps(0.0, &plan);
    assert_eq!(phase, Phase::RampUp);
    assert_eq!(rps, 0.0);
}

#[test]
fn constant_always_full_rps() {
    let plan = plan_for_mode(Mode::Constant, 100, 60, 10, 5);
    for t in [0.0, 1.0, 30.0, 59.9] {
        let (phase, rps) = compute_phase_and_rps(t, &plan);
        assert_eq!(phase, Phase::Steady);
        assert_eq!(rps, 100.0);
    }
}

#[test]
fn step_first_step_is_fraction() {
    // 5 steps over 60s → each step = 12s. At t=5 (step 0): rps = 100*1/5 = 20.
    let plan = plan_for_mode(Mode::Step, 100, 60, 0, 5);
    let (_, rps) = compute_phase_and_rps(5.0, &plan);
    assert!((rps - 20.0).abs() < 0.01);
}

#[test]
fn step_last_step_is_full_rps() {
    // At t=55 (step 4): rps = 100*5/5 = 100.
    let plan = plan_for_mode(Mode::Step, 100, 60, 0, 5);
    let (_, rps) = compute_phase_and_rps(55.0, &plan);
    assert!((rps - 100.0).abs() < 0.01);
}

#[test]
fn spike_middle_third_is_peak() {
    // duration=60s → spike at 20s–40s.
    let plan = plan_for_mode(Mode::Spike, 100, 60, 0, 5);
    let (phase, rps) = compute_phase_and_rps(30.0, &plan);
    assert_eq!(phase, Phase::RampUp);
    assert_eq!(rps, 100.0);
}

#[test]
fn spike_final_third_is_recovery() {
    let plan = plan_for_mode(Mode::Spike, 100, 60, 0, 5);
    let (phase, rps) = compute_phase_and_rps(50.0, &plan);
    assert_eq!(phase, Phase::RampDown);
    assert!((rps - 20.0).abs() < 0.01);
}

#[test]
fn spike_first_third_is_baseline() {
    let plan = plan_for_mode(Mode::Spike, 100, 60, 0, 5);
    let (phase, rps) = compute_phase_and_rps(5.0, &plan);
    assert_eq!(phase, Phase::Steady);
    assert!((rps - 20.0).abs() < 0.01);
}

// ── pick_task ─────────────────────────────────────────────────────────────

#[test]
fn pick_task_empty_returns_none() {
    assert!(pick_task(&[], 0).is_none());
}

#[test]
fn pick_task_single_task_always_selected() {
    let tasks = vec![make_task("only", 1)];
    for i in 0..5 {
        assert_eq!(pick_task(&tasks, i).unwrap().name, "only");
    }
}

#[test]
fn pick_task_equal_weights_round_robin() {
    let tasks = vec![make_task("a", 1), make_task("b", 1)];
    assert_eq!(pick_task(&tasks, 0).unwrap().name, "a");
    assert_eq!(pick_task(&tasks, 1).unwrap().name, "b");
    assert_eq!(pick_task(&tasks, 2).unwrap().name, "a");
}

#[test]
fn pick_task_higher_weight_selected_more() {
    // weights: a=1, b=3 → total=4; slots 0→a, 1,2,3→b
    let tasks = vec![make_task("a", 1), make_task("b", 3)];
    let names: Vec<&str> = (0..4)
        .map(|i| pick_task(&tasks, i).unwrap().name.as_str())
        .collect();
    assert_eq!(names, vec!["a", "b", "b", "b"]);
}

#[test]
fn pick_task_wraps_around() {
    let tasks = vec![make_task("a", 1), make_task("b", 1)];
    assert_eq!(pick_task(&tasks, 4).unwrap().name, "a");
    assert_eq!(pick_task(&tasks, 5).unwrap().name, "b");
}
