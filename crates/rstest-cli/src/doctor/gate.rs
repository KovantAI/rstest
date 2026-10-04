//! The `--doctor-fail-on` threshold gate: fail the run when a doctor metric
//! breaches a threshold. Pure evaluator over `DoctorReport`, no new analysis,
//! so any non-GitHub CI can gate too.

use super::DoctorReport;
use crate::reporting::sink::Sink;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Op {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl Op {
    fn parse(s: &str) -> Option<(Op, usize)> {
        // Two-char operators first so `<=` isn't misread as `<`.
        for (tok, op) in [
            ("<=", Op::Le),
            (">=", Op::Ge),
            ("==", Op::Eq),
            ("!=", Op::Ne),
            ("<", Op::Lt),
            (">", Op::Gt),
        ] {
            if let Some(pos) = s.find(tok) {
                return Some((op, pos));
            }
        }
        None
    }

    fn symbol(self) -> &'static str {
        match self {
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Gt => ">",
            Op::Ge => ">=",
            Op::Eq => "==",
            Op::Ne => "!=",
        }
    }

    fn test(self, lhs: f64, rhs: f64) -> bool {
        match self {
            Op::Lt => lhs < rhs,
            Op::Le => lhs <= rhs,
            Op::Gt => lhs > rhs,
            Op::Ge => lhs >= rhs,
            Op::Eq => lhs == rhs,
            Op::Ne => lhs != rhs,
        }
    }
}

/// The metrics a gate condition can name. `wait_*` resolve whenever CPU time
/// was measured (whether or not the WAIT-BOUND section was shown), and
/// `long_pole_seconds` at any worker count. The pool-only metrics
/// (`parallel_efficiency`, `realized_speedup`, `imbalance_pct`) resolve to
/// `None` without a multi-worker pool, so their condition is skipped, not
/// false-failed.
const METRICS: &[&str] = &[
    "wall_seconds",
    "test_time_seconds",
    "cpu_time_seconds",
    "tests",
    "workers",
    "wait_pct",
    "wait_seconds",
    "parallel_efficiency", // == efficiency_pct
    "efficiency_pct",
    "realized_speedup",
    "imbalance_pct",
    "long_pole_seconds",
];

/// One parsed `--doctor-fail-on` condition. Parsing validates the metric name
/// and threshold up front (before the run) so a typo fails fast rather than
/// silently never firing - the exact bug class this feature exists to kill.
#[derive(Clone, Debug)]
pub struct GateCondition {
    raw: String,
    metric: String,
    op: Op,
    threshold: f64,
}

/// Parse and validate every `--doctor-fail-on` spec, or return the first
/// error. Call before running so a bad condition aborts immediately.
pub fn parse_conditions(specs: &[String], sink: &mut Sink) -> anyhow::Result<Vec<GateCondition>> {
    specs.iter().map(|s| parse_condition(s, sink)).collect()
}

fn parse_condition(spec: &str, sink: &mut Sink) -> anyhow::Result<GateCondition> {
    let (op, pos) = Op::parse(spec).ok_or_else(|| {
        anyhow::anyhow!(
            "--doctor-fail-on '{spec}': no comparison operator (use one of < <= > >= == !=), \
             e.g. 'parallel_efficiency<30'"
        )
    })?;
    let metric = spec[..pos].trim().to_string();
    let rhs = spec[pos + op.symbol().len()..].trim();
    if !METRICS.contains(&metric.as_str()) {
        anyhow::bail!(
            "--doctor-fail-on '{spec}': unknown metric '{metric}'. Known metrics: {}",
            METRICS.join(", ")
        );
    }
    let threshold: f64 = rhs.parse().map_err(|_| {
        anyhow::anyhow!("--doctor-fail-on '{spec}': threshold '{rhs}' is not a number")
    })?;
    // `NaN` / `inf` parse as f64 but make a gate that never (`>NaN`) or always
    // (`!=nan`) fires: reject them like any other non-number.
    if !threshold.is_finite() {
        anyhow::bail!("--doctor-fail-on '{spec}': threshold '{rhs}' is not a finite number");
    }
    // Exact == / != is reliable only on integer-valued metrics; on a float
    // metric it almost never matches and would silently never fire. Warn
    // rather than reject - someone may still want it on `tests`/`workers`.
    if matches!(op, Op::Eq | Op::Ne) && !matches!(metric.as_str(), "tests" | "workers") {
        sink.warn(&format!(
            "rstest: --doctor-fail-on '{spec}': exact {} on the floating-point \
             metric '{metric}' rarely matches; a threshold (< / >) is usually meant",
            op.symbol()
        ));
    }
    Ok(GateCondition {
        raw: spec.to_string(),
        metric,
        op,
        threshold,
    })
}

/// The value a metric resolves to for this report, or `None` if the backing
/// section didn't apply to the run.
fn metric_value(report: &DoctorReport, metric: &str) -> Option<f64> {
    match metric {
        "wall_seconds" => Some(report.wall_seconds),
        "test_time_seconds" => Some(report.test_time_seconds),
        "cpu_time_seconds" => Some(report.cpu_time_seconds),
        "tests" => Some(report.tests as f64),
        "workers" => Some(report.workers as f64),
        // The measured wait, not the WAIT-BOUND section: that one is hidden
        // below its display threshold, and a gate must still see the value.
        "wait_pct" => report.measured_wait.map(|(_, pct)| pct),
        "wait_seconds" => report.measured_wait.map(|(secs, _)| secs),
        "parallel_efficiency" | "efficiency_pct" => report
            .parallel_efficiency
            .as_ref()
            .map(|p| p.efficiency_pct),
        "realized_speedup" => report
            .parallel_efficiency
            .as_ref()
            .map(|p| p.realized_speedup),
        "imbalance_pct" => report.parallel_efficiency.as_ref().map(|p| p.imbalance_pct),
        "long_pole_seconds" => report.long_pole_seconds,
        _ => None,
    }
}

/// Outcome of gating a report: human-readable messages, split into conditions
/// that fired (breaches → the run must fail) and conditions that couldn't be
/// evaluated because their section was absent (skipped → not a failure).
pub struct GateOutcome {
    pub breaches: Vec<String>,
    pub skipped: Vec<String>,
}

impl GateOutcome {
    /// The closing line for a gate run with no breach: never claims "all
    /// passed" when some condition could not be evaluated.
    pub fn pass_summary(&self, total: usize) -> String {
        let skipped = self.skipped.len();
        if skipped == 0 {
            format!("all {total} condition(s) passed")
        } else {
            format!(
                "{} condition(s) passed, {skipped} skipped (not measured for this run)",
                total.saturating_sub(skipped)
            )
        }
    }
}

pub fn evaluate(report: &DoctorReport, conditions: &[GateCondition]) -> GateOutcome {
    let mut breaches = Vec::new();
    let mut skipped = Vec::new();
    for c in conditions {
        match metric_value(report, &c.metric) {
            Some(v) if c.op.test(v, c.threshold) => breaches.push(format!(
                "{} = {:.2} {} {:.2} ({})",
                c.metric,
                v,
                c.op.symbol(),
                c.threshold,
                c.raw
            )),
            Some(_) => {}
            None => skipped.push(format!(
                "'{}' not measured for this run (metric absent); condition skipped",
                c.raw
            )),
        }
    }
    GateOutcome { breaches, skipped }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::report;
    use super::*;

    #[test]
    fn gate_parse_rejects_unknown_metric_and_bad_grammar() {
        assert!(
            parse_conditions(&["parallel_efficiency<30".into()], &mut Sink::captured().0).is_ok()
        );
        // unknown metric
        let e = parse_conditions(&["bogus<30".into()], &mut Sink::captured().0)
            .unwrap_err()
            .to_string();
        assert!(e.contains("unknown metric 'bogus'"), "{e}");
        // no operator
        let e = parse_conditions(&["wait_pct 50".into()], &mut Sink::captured().0)
            .unwrap_err()
            .to_string();
        assert!(e.contains("no comparison operator"), "{e}");
        // non-numeric threshold
        let e = parse_conditions(&["wait_pct>lots".into()], &mut Sink::captured().0)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a number"), "{e}");
        // non-finite thresholds parse as f64 but are rejected
        for spec in [
            "wait_pct>NaN",
            "tests!=inf",
            "wall_seconds<-inf",
            "tests>infinity",
        ] {
            let e = parse_conditions(&[spec.into()], &mut Sink::captured().0)
                .unwrap_err()
                .to_string();
            assert!(e.contains("not a finite number"), "{spec}: {e}");
        }
    }

    #[test]
    fn gate_parse_handles_two_char_operators() {
        let c = parse_condition("efficiency_pct<=30", &mut Sink::captured().0).unwrap();
        assert_eq!(c.metric, "efficiency_pct");
        assert_eq!(c.op, Op::Le);
        assert!((c.threshold - 30.0).abs() < 1e-9);
    }

    #[test]
    fn gate_breaches_and_passes() {
        let r = report(12); // efficiency_pct 82.5, wait_pct 80.0, wall 9.0
        let conds = parse_conditions(
            &[
                "parallel_efficiency<90".into(), // 82.5 < 90 -> breach
                "wait_pct>50".into(),            // 80 > 50 -> breach
                "wall_seconds>100".into(),       // 9 > 100 -> pass
            ],
            &mut Sink::captured().0,
        )
        .unwrap();
        let out = evaluate(&r, &conds);
        assert_eq!(out.breaches.len(), 2, "{:?}", out.breaches);
        assert!(out.skipped.is_empty());
        assert!(out.breaches[0].contains("parallel_efficiency = 82.50 < 90.00"));
    }

    #[test]
    fn every_known_metric_resolves_on_a_full_report() {
        // Guards METRICS vs metric_value drift: a name added to METRICS but not
        // to metric_value would resolve to None even on a fully-populated
        // report and silently always-skip. `report(12)` has every section.
        let r = report(12);
        for name in METRICS {
            let c = parse_condition(&format!("{name}>=0"), &mut Sink::captured().0).unwrap();
            let out = evaluate(&r, std::slice::from_ref(&c));
            assert!(
                out.skipped.is_empty(),
                "metric '{name}' is in METRICS but did not resolve (metric_value drift)"
            );
        }
    }

    #[test]
    fn gate_skips_absent_section_never_fails() {
        // A run with no parallel_efficiency section and no CPU measurement:
        // gating those metrics must skip, not fail.
        let mut r = report(4);
        r.parallel_efficiency = None;
        r.wait_bound = None;
        r.measured_wait = None;
        let conds = parse_conditions(
            &[
                "parallel_efficiency<30".into(),
                "wait_pct>1".into(),
                "tests>100".into(),
            ],
            &mut Sink::captured().0,
        )
        .unwrap();
        let out = evaluate(&r, &conds);
        assert!(out.breaches.is_empty(), "{:?}", out.breaches);
        assert_eq!(out.skipped.len(), 2);
        assert_eq!(
            out.pass_summary(conds.len()),
            "1 condition(s) passed, 2 skipped (not measured for this run)"
        );
    }

    #[test]
    fn wait_and_long_pole_gate_without_their_sections() {
        // Wait below the WAIT-BOUND display threshold (no section) and a
        // single-worker run (no parallel_efficiency): the measured values
        // still gate.
        let mut r = report(4);
        r.parallel_efficiency = None;
        r.wait_bound = None;
        r.measured_wait = Some((0.8, 44.0));
        r.long_pole_seconds = Some(0.8);
        let conds = parse_conditions(
            &[
                "wait_seconds>0.5".into(),
                "wait_pct>10".into(),
                "long_pole_seconds>0.5".into(),
            ],
            &mut Sink::captured().0,
        )
        .unwrap();
        let out = evaluate(&r, &conds);
        assert!(out.skipped.is_empty(), "{:?}", out.skipped);
        assert_eq!(out.breaches.len(), 3, "{:?}", out.breaches);
        assert_eq!(out.pass_summary(3), "all 3 condition(s) passed");
    }
}
