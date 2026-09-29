//! The `device` timing profile fit. `specs/timing-profiles.toml` carries, beside its
//! `[[constant]]` rows, the calibration plan:
//!
//! - `[[fit]]` rows name the constants solved for, with start value, finite-difference step,
//!   bounds and rounding quantum;
//! - `[[anchor]]` rows name device lines in boot order, each with its device timestamp and a role
//!   that belongs to the **phase ending at the anchor**: `fit` phases are minimised, `validation`
//!   phases are judged against [`crate::bands`], and an `excluded` anchor is used for neither,
//!   with its reason. A phase is never both.
//!
//! A `validation` anchor may carry `delta_band_ms` with a `band_basis`: a band from the device's
//! own spread over repeated boots of one build. It can only widen the standard allowance, and the
//! report marks it.
//!
//! The fit is damped Gauss-Newton least squares over the fit phases' residuals (emulated minus
//! device delta, ms). The caller hands [`fit`] a function that boots the emulator, which keeps
//! this crate free of the machine and the fit as deterministic as the emulator. Timestamps are
//! whole milliseconds, so the model is a step function: the steps move a phase by several ms,
//! and the fit keeps the best point it evaluated rather than the last.

use crate::bands::{ABSOLUTE, DELTA, Tolerance};
use crate::spec_toml::{self, Error as SpecError};

/// What a phase is used for (module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    /// The phase ending at this anchor is minimised by the fit.
    Fit,
    /// The phase ending at this anchor is judged by the delta and absolute bands.
    Validation,
    /// The anchor is used for nothing, for the reason given.
    Excluded(String),
}

/// One `[[anchor]]` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnchorRow {
    pub name: String,
    /// Substring of the normalized line; the first timestamped match wins.
    pub line: String,
    /// The device line it is, `L<n>`.
    pub dev: String,
    pub device_ms: u32,
    /// What the phase ending here is used for.
    pub role: Role,
    /// A delta band from the device's spread over repeated boots, in milliseconds either side of
    /// the device delta, with its evidence; `None` for the standard band.
    pub evidence_band: Option<EvidenceBand>,
}

/// A delta band derived from silicon evidence (module documentation).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceBand {
    /// Milliseconds either side of the device delta.
    pub delta_ms: u32,
    /// The boots it was derived from and the derivation.
    pub basis: String,
}

/// One `[[fit]]` row: a `[[constant]]` the fit solves for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FitParam {
    pub name: String,
    pub start: i64,
    /// The forward-difference step.
    pub step: i64,
    pub min: i64,
    pub max: i64,
    /// The result is rounded to a multiple of this.
    pub quantum: i64,
}

/// The calibration plan of `specs/timing-profiles.toml`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub params: Vec<FitParam>,
    pub anchors: Vec<AnchorRow>,
}

impl Plan {
    /// Parses the `[[fit]]` and `[[anchor]]` rows. Refuses no parameter, fewer fit phases than
    /// parameters, a bad role, an excluded anchor without a reason, anchors out of device order,
    /// a first anchor ending a fit phase (only the absolute band could check it), or an evidence
    /// band without a basis or off a validation phase.
    pub fn parse(text: &str) -> Result<Plan, SpecError> {
        let doc = spec_toml::parse(text)?;
        let int = |t: &spec_toml::Table, key: &str| -> Result<i64, SpecError> {
            t.pairs
                .get(key)
                .and_then(spec_toml::Value::as_int)
                .ok_or_else(|| SpecError::new(t.line, format!("`{key}` is not an integer")))
        };
        let mut params = Vec::new();
        for t in doc.array("fit") {
            let p = FitParam {
                name: t.str_field("name")?.to_string(),
                start: int(t, "start")?,
                step: int(t, "step")?,
                min: int(t, "min")?,
                max: int(t, "max")?,
                quantum: int(t, "quantum")?,
            };
            if p.step <= 0 || p.quantum <= 0 || p.min > p.max || !(p.min..=p.max).contains(&p.start)
            {
                return Err(SpecError::new(
                    t.line,
                    format!("fit row `{}` is inconsistent", p.name),
                ));
            }
            params.push(p);
        }
        let mut anchors = Vec::new();
        for t in doc.array("anchor") {
            let role = match t.str_field("role")? {
                "fit" => Role::Fit,
                "validation" => Role::Validation,
                "excluded" => Role::Excluded(t.str_field("reason")?.to_string()),
                other => return Err(SpecError::new(t.line, format!("unknown role `{other}`"))),
            };
            let device_ms = u32::try_from(int(t, "device_ms")?)
                .map_err(|_| SpecError::new(t.line, "`device_ms` is out of range"))?;
            let evidence_band = match t.pairs.get("delta_band_ms") {
                None => {
                    if t.pairs.contains_key("band_basis") {
                        return Err(SpecError::new(
                            t.line,
                            "`band_basis` without `delta_band_ms`",
                        ));
                    }
                    None
                }
                Some(v) => {
                    let delta_ms =
                        v.as_int()
                            .and_then(|v| u32::try_from(v).ok())
                            .ok_or_else(|| {
                                SpecError::new(t.line, "`delta_band_ms` is not a millisecond count")
                            })?;
                    if role != Role::Validation {
                        return Err(SpecError::new(
                            t.line,
                            "`delta_band_ms` belongs on a validation phase only",
                        ));
                    }
                    let basis = t.opt_str("band_basis")?.unwrap_or_default().trim();
                    if basis.is_empty() {
                        return Err(SpecError::new(
                            t.line,
                            "`delta_band_ms` needs a `band_basis` naming its evidence",
                        ));
                    }
                    Some(EvidenceBand {
                        delta_ms,
                        basis: basis.to_string(),
                    })
                }
            };
            anchors.push(AnchorRow {
                name: t.str_field("name")?.to_string(),
                line: t.str_field("line")?.to_string(),
                dev: t.str_field("dev")?.to_string(),
                device_ms,
                role,
                evidence_band,
            });
        }
        if params.is_empty() {
            return Err(SpecError::new(1, "the plan has no [[fit]] row"));
        }
        let used: Vec<&AnchorRow> = anchors
            .iter()
            .filter(|a| !matches!(a.role, Role::Excluded(_)))
            .collect();
        if used.first().is_some_and(|a| a.role == Role::Fit) {
            return Err(SpecError::new(1, "the first anchor cannot end a fit phase"));
        }
        if used.windows(2).any(|w| w[1].device_ms < w[0].device_ms) {
            return Err(SpecError::new(1, "the anchors are not in device order"));
        }
        let fit_phases = used.iter().filter(|a| a.role == Role::Fit).count();
        if fit_phases < params.len() {
            return Err(SpecError::new(
                1,
                format!(
                    "{fit_phases} fit phase(s) cannot determine {} parameter(s)",
                    params.len()
                ),
            ));
        }
        Ok(Plan { params, anchors })
    }

    pub fn start(&self) -> Vec<i64> {
        self.params.iter().map(|p| p.start).collect()
    }
}

/// One phase's residual: the delta between two consecutive anchors in use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseResidual {
    pub from: String,
    /// Later anchor, whose role the phase has.
    pub to: String,
    pub role: Role,
    pub device_ms: u32,
    /// Emulated delta, milliseconds, or `None` when an anchor never printed.
    pub emulated_ms: Option<u32>,
    /// The delta allowance: the standard one for the device delta, or the anchor's evidence band
    /// where that is wider.
    pub allowed_ms: u32,
    /// Whether `allowed_ms` is the anchor's evidence band.
    pub evidence_band: bool,
}

impl PhaseResidual {
    /// Emulated minus device, milliseconds.
    pub fn residual_ms(&self) -> Option<i64> {
        self.emulated_ms
            .map(|e| i64::from(e) - i64::from(self.device_ms))
    }

    /// Whether the delta band holds.
    pub fn inside(&self) -> bool {
        self.emulated_ms
            .is_some_and(|e| e.abs_diff(self.device_ms) <= self.allowed_ms)
    }
}

/// One anchor's absolute residual.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AbsoluteResidual {
    pub anchor: String,
    pub device_ms: u32,
    /// Emulated timestamp, or `None` when it never printed.
    pub emulated_ms: Option<u32>,
    /// The absolute allowance.
    pub allowed_ms: u32,
}

impl AbsoluteResidual {
    pub fn inside(&self) -> bool {
        self.emulated_ms
            .is_some_and(|e| ABSOLUTE.allows(self.device_ms, e))
    }
}

/// The residuals of one run against the plan.
#[derive(Clone, Debug, PartialEq)]
pub struct Residuals {
    /// Every phase between consecutive anchors in use, fit and validation.
    pub phases: Vec<PhaseResidual>,
    /// Every anchor in use.
    pub absolutes: Vec<AbsoluteResidual>,
}

impl Residuals {
    /// Root mean square of the residuals of the phases with `role`, milliseconds; `None` when a
    /// phase of that role has no emulated delta.
    pub fn rms_ms(&self, role: &Role) -> Option<f64> {
        let r: Option<Vec<i64>> = self
            .phases
            .iter()
            .filter(|p| &p.role == role)
            .map(PhaseResidual::residual_ms)
            .collect();
        let r = r?;
        if r.is_empty() {
            return Some(0.0);
        }
        let ss: f64 = r.iter().map(|v| (*v as f64) * (*v as f64)).sum();
        Some((ss / r.len() as f64).sqrt())
    }

    /// The validation phases outside the delta band, and, only when every delta holds, the
    /// anchors outside the absolute band.
    pub fn band_failures(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .phases
            .iter()
            .filter(|p| p.role == Role::Validation && !p.inside())
            .map(|p| {
                format!(
                    "delta {} -> {}: device {} ms, emulated {}, allowed +/-{} ms{}",
                    p.from,
                    p.to,
                    p.device_ms,
                    p.emulated_ms
                        .map_or("missing".to_string(), |e| format!("{e} ms")),
                    p.allowed_ms,
                    if p.evidence_band {
                        " (band from silicon evidence)"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        if !out.is_empty() {
            return out;
        }
        for a in &self.absolutes {
            if !a.inside() {
                out.push(format!(
                    "absolute {}: device {} ms, emulated {}, allowed +/-{} ms",
                    a.anchor,
                    a.device_ms,
                    a.emulated_ms
                        .map_or("missing".to_string(), |e| format!("{e} ms")),
                    a.allowed_ms
                ));
            }
        }
        out
    }
}

/// The residuals of `emulated` (one timestamp per anchor of the plan, in plan order, `None`
/// where the line never printed) against the plan's device timestamps.
pub fn residuals(plan: &Plan, emulated: &[Option<u32>]) -> Residuals {
    let used: Vec<(&AnchorRow, Option<u32>)> = plan
        .anchors
        .iter()
        .zip(emulated.iter().copied().chain(std::iter::repeat(None)))
        .filter(|(a, _)| !matches!(a.role, Role::Excluded(_)))
        .collect();
    let allowance = |t: Tolerance, ms: u32| t.allowance_ms(ms);
    let phases = used
        .windows(2)
        .map(|w| {
            let (a, ea) = w[0];
            let (b, eb) = w[1];
            let device_ms = b.device_ms - a.device_ms;
            let arch = allowance(DELTA, device_ms);
            let evidence = b
                .evidence_band
                .as_ref()
                .map(|e| e.delta_ms)
                .filter(|&e| e > arch);
            PhaseResidual {
                from: a.name.clone(),
                to: b.name.clone(),
                role: b.role.clone(),
                device_ms,
                emulated_ms: ea.zip(eb).map(|(x, y)| y.saturating_sub(x)),
                allowed_ms: evidence.unwrap_or(arch),
                evidence_band: evidence.is_some(),
            }
        })
        .collect();
    let absolutes = used
        .iter()
        .map(|(a, e)| AbsoluteResidual {
            anchor: a.name.clone(),
            device_ms: a.device_ms,
            emulated_ms: *e,
            allowed_ms: allowance(ABSOLUTE, a.device_ms),
        })
        .collect();
    Residuals { phases, absolutes }
}

/// The result of [`fit`].
#[derive(Clone, Debug, PartialEq)]
pub struct Fitted {
    pub values: Vec<i64>,
    /// The residuals at those values.
    pub residuals: Residuals,
    pub evaluations: usize,
}

/// Sum of squared fit-phase residuals, or `None` when a fit phase has no emulated delta.
fn cost(r: &Residuals) -> Option<f64> {
    r.phases
        .iter()
        .filter(|p| p.role == Role::Fit)
        .map(|p| p.residual_ms().map(|v| (v as f64) * (v as f64)))
        .sum()
}

fn fit_vector(r: &Residuals) -> Option<Vec<f64>> {
    r.phases
        .iter()
        .filter(|p| p.role == Role::Fit)
        .map(|p| p.residual_ms().map(|v| v as f64))
        .collect()
}

fn clamp_round(p: &FitParam, v: f64) -> i64 {
    let q = p.quantum as f64;
    let rounded = ((v / q).round() * q) as i64;
    rounded.clamp(p.min, p.max)
}

/// Solves `a x = b` for a small square system by Gaussian elimination with partial pivoting;
/// `None` when it is singular.
fn solve(mut a: Vec<Vec<f64>>, mut b: Vec<f64>) -> Option<Vec<f64>> {
    let n = b.len();
    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| a[i][col].abs().total_cmp(&a[j][col].abs()))?;
        if a[pivot][col].abs() < 1e-12 {
            return None;
        }
        a.swap(col, pivot);
        b.swap(col, pivot);
        for row in col + 1..n {
            let f = a[row][col] / a[col][col];
            let pivot_row = a[col].clone();
            for (k, v) in a[row].iter_mut().enumerate().skip(col) {
                *v -= f * pivot_row[k];
            }
            b[row] -= f * b[col];
        }
    }
    let mut x = vec![0.0; n];
    for row in (0..n).rev() {
        let s: f64 = (row + 1..n).map(|k| a[row][k] * x[k]).sum();
        x[row] = (b[row] - s) / a[row][row];
    }
    Some(x)
}

pub const MAX_ITERATIONS: usize = 8;

/// Fits the plan's parameters. `run` boots the emulator under the `device` profile with the
/// given values and returns one emulated timestamp per plan anchor, in plan order. Returns the
/// best point evaluated; errors when the start leaves a fit phase without an emulated delta.
pub fn fit(plan: &Plan, run: &mut dyn FnMut(&[i64]) -> Vec<Option<u32>>) -> Result<Fitted, String> {
    let n = plan.params.len();
    let mut evaluations = 0usize;
    let mut eval = |x: &[i64]| {
        evaluations += 1;
        residuals(plan, &run(x))
    };
    let mut x = plan.start();
    let mut rx = eval(&x);
    let mut best_cost = cost(&rx).ok_or("a fit phase has no emulated delta at the start point")?;
    let mut lambda = 1e-3;
    for _ in 0..MAX_ITERATIONS {
        let r0 = fit_vector(&rx).ok_or("a fit phase lost its emulated delta")?;
        // Forward-difference Jacobian in milliseconds per `step`: the parameters differ by ten
        // orders of magnitude, so the normal equations are solved in step units.
        let mut jac = vec![vec![0.0; n]; r0.len()];
        for (j, p) in plan.params.iter().enumerate() {
            let mut xj = x.clone();
            let up = (x[j] + p.step).min(p.max);
            let h = if up == x[j] { -p.step } else { up - x[j] };
            xj[j] = x[j] + h;
            let rj = fit_vector(&eval(&xj)).ok_or("a fit phase lost its emulated delta")?;
            for (i, row) in jac.iter_mut().enumerate() {
                row[j] = (rj[i] - r0[i]) * p.step as f64 / h as f64;
            }
        }
        // Normal equations with Levenberg-Marquardt damping on the diagonal.
        let jtj: Vec<Vec<f64>> = (0..n)
            .map(|a| {
                (0..n)
                    .map(|b| jac.iter().map(|row| row[a] * row[b]).sum())
                    .collect()
            })
            .collect();
        let jtr: Vec<f64> = (0..n)
            .map(|a| -jac.iter().zip(&r0).map(|(row, r)| row[a] * r).sum::<f64>())
            .collect();
        let mut improved = false;
        for _ in 0..4 {
            let mut damped = jtj.clone();
            for (k, row) in damped.iter_mut().enumerate() {
                row[k] += lambda * jtj[k][k].max(1e-12);
            }
            let Some(delta) = solve(damped, jtr.clone()) else {
                lambda *= 10.0;
                continue;
            };
            let candidate: Vec<i64> = plan
                .params
                .iter()
                .zip(&x)
                .zip(&delta)
                .map(|((p, xi), d)| clamp_round(p, *xi as f64 + d * p.step as f64))
                .collect();
            if candidate == x {
                break;
            }
            let rc = eval(&candidate);
            match cost(&rc) {
                Some(c) if c < best_cost => {
                    best_cost = c;
                    x = candidate;
                    rx = rc;
                    lambda = (lambda / 10.0).max(1e-6);
                    improved = true;
                    break;
                }
                _ => lambda *= 10.0,
            }
        }
        if !improved {
            break;
        }
    }
    // Round the start point too, so a fit that never moved still reports quantized values.
    let rounded: Vec<i64> = plan
        .params
        .iter()
        .zip(&x)
        .map(|(p, v)| clamp_round(p, *v as f64))
        .collect();
    if rounded != x {
        x = rounded;
        rx = eval(&x);
    }
    Ok(Fitted {
        values: x,
        residuals: rx,
        evaluations,
    })
}

/// The residual report: the fitted values, every phase with role, deltas, residual and band, the
/// absolute anchors, then the RMS per role.
pub fn render_report(plan: &Plan, fitted: &Fitted) -> String {
    let mut out = String::new();
    out.push_str("fit:\n");
    for (p, v) in plan.params.iter().zip(&fitted.values) {
        out.push_str(&format!("  {} = {v}\n", p.name));
    }
    out.push_str(&format!("  boots run: {}\n", fitted.evaluations));
    out.push_str("phases (role, device ms, emulated ms, residual, allowed, inside):\n");
    for p in &fitted.residuals.phases {
        let role = match &p.role {
            Role::Fit => "fit",
            Role::Validation => "validation",
            Role::Excluded(_) => "excluded",
        };
        out.push_str(&format!(
            "  {:<10} {:>5} {:>5} {:>+5} +/-{:<4} {:<5} {} -> {}{}\n",
            role,
            p.device_ms,
            p.emulated_ms.map_or(-1, i64::from),
            p.residual_ms().unwrap_or(0),
            p.allowed_ms,
            p.inside(),
            p.from,
            p.to,
            if p.evidence_band {
                " (band from silicon evidence)"
            } else {
                ""
            }
        ));
    }
    out.push_str("absolute (device ms, emulated ms, allowed, inside):\n");
    for a in &fitted.residuals.absolutes {
        out.push_str(&format!(
            "  {:>5} {:>5} +/-{:<4} {:<5} {}\n",
            a.device_ms,
            a.emulated_ms.map_or(-1, i64::from),
            a.allowed_ms,
            a.inside(),
            a.anchor
        ));
    }
    for a in &plan.anchors {
        if let Role::Excluded(reason) = &a.role {
            out.push_str(&format!("excluded {} ({}): {reason}\n", a.name, a.dev));
        }
    }
    let rms = |role| {
        fitted
            .residuals
            .rms_ms(&role)
            .map_or("n/a".to_string(), |v| format!("{v:.2} ms"))
    };
    out.push_str(&format!(
        "rms: fit {}, validation {}\n",
        rms(Role::Fit),
        rms(Role::Validation)
    ));
    out
}

/// Rewrites the `device = <n>` line of each named `[[constant]]` row of a timing profile table,
/// leaving every other byte as it was. Errors when a name has no row or its row has no integer
/// `device` line.
pub fn write_device_values(text: &str, values: &[(&str, i64)]) -> Result<String, String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    for (name, value) in values {
        let header = format!("name = \"{name}\"");
        let at = lines
            .iter()
            .position(|l| l.trim() == header)
            .ok_or_else(|| format!("no [[constant]] row `{name}`"))?;
        let device = lines[at..]
            .iter()
            .take_while(|l| !l.trim().is_empty())
            .position(|l| l.trim_start().starts_with("device = "))
            .ok_or_else(|| format!("row `{name}` has no device line"))?;
        let line = &mut lines[at + device];
        let old = line.trim_start().trim_start_matches("device = ").trim();
        if old.parse::<i64>().is_err() {
            return Err(format!(
                "row `{name}` has a non-integer device value `{old}`"
            ));
        }
        *line = format!("device = {value}");
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN: &str = r#"
[[constant]]
name = "a"
fast = 0
device = 5

[[fit]]
name = "a"
start = 0
step = 10
min = 0
max = 1000
quantum = 1

[[anchor]]
name = "start"
line = "one"
dev = "L1"
device_ms = 10
role = "validation"

[[anchor]]
name = "middle"
line = "two"
dev = "L2"
device_ms = 110
role = "fit"

[[anchor]]
name = "noise"
line = "three"
dev = "L3"
device_ms = 115
role = "excluded"
reason = "a test row"

[[anchor]]
name = "end"
line = "four"
dev = "L4"
device_ms = 160
role = "validation"
"#;

    /// A linear model the fit must solve exactly: the fit phase is `a / 2` ms long, so the
    /// device's 100 ms needs `a = 200`; the validation phase follows `a / 4`, 50 ms at the fit.
    #[test]
    fn the_fit_solves_a_linear_model_and_reports_the_split() {
        let plan = Plan::parse(PLAN).expect("the plan parses");
        assert_eq!(plan.params.len(), 1);
        let mut run = |x: &[i64]| {
            let a = x[0] as u32;
            vec![
                Some(10),
                Some(10 + a / 2),
                Some(0),
                Some(10 + a / 2 + a / 4),
            ]
        };
        let fitted = fit(&plan, &mut run).expect("the fit runs");
        assert_eq!(fitted.values, vec![200]);
        let r = &fitted.residuals;
        assert_eq!(r.phases.len(), 2, "the excluded anchor makes no phase");
        assert_eq!(r.phases[0].role, Role::Fit);
        assert_eq!(r.phases[0].residual_ms(), Some(0));
        assert_eq!(r.phases[1].role, Role::Validation);
        assert_eq!(
            (r.phases[1].device_ms, r.phases[1].emulated_ms),
            (50, Some(50))
        );
        assert!(r.band_failures().is_empty());
        let report = render_report(&plan, &fitted);
        assert!(report.contains("a = 200"), "{report}");
        assert!(
            report.contains("excluded noise (L3): a test row"),
            "{report}"
        );
    }

    #[test]
    fn a_validation_phase_outside_its_band_is_a_failure_and_absolutes_wait() {
        let plan = Plan::parse(PLAN).expect("the plan parses");
        let r = residuals(&plan, &[Some(10), Some(110), None, Some(200)]);
        let failures = r.band_failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].starts_with("delta middle -> end"));
        // Every delta holds (the fit phase is not judged), so the late start shows as absolutes.
        let r = residuals(&plan, &[Some(40), Some(140), None, Some(190)]);
        let failures = r.band_failures();
        assert_eq!(failures.len(), 2, "{failures:?}");
        assert!(failures[0].starts_with("absolute start"), "{failures:?}");
    }

    #[test]
    fn an_evidence_band_widens_a_validation_phase_and_needs_its_basis() {
        let banded = PLAN.replace(
            "device_ms = 160\nrole = \"validation\"\n",
            "device_ms = 160\nrole = \"validation\"\ndelta_band_ms = 12\nband_basis = \"boots\"\n",
        );
        let plan = Plan::parse(&banded).expect("the banded plan parses");
        assert_eq!(
            plan.anchors[3].evidence_band,
            Some(EvidenceBand {
                delta_ms: 12,
                basis: "boots".into()
            })
        );
        // 50 ms device delta: the standard band allows 10, the evidence 12.
        let r = residuals(&plan, &[Some(10), Some(110), None, Some(172)]);
        assert_eq!(
            (r.phases[1].allowed_ms, r.phases[1].evidence_band),
            (12, true)
        );
        assert!(r.band_failures().is_empty(), "{:?}", r.band_failures());
        let r = residuals(&plan, &[Some(10), Some(110), None, Some(173)]);
        let failures = r.band_failures();
        assert!(
            failures[0].ends_with("allowed +/-12 ms (band from silicon evidence)"),
            "{failures:?}"
        );
        // A band narrower than the standard one changes nothing: the evidence only widens.
        let narrow = banded.replace("delta_band_ms = 12", "delta_band_ms = 3");
        let r = residuals(
            &Plan::parse(&narrow).expect("parses"),
            &[Some(10), Some(110), None, Some(170)],
        );
        assert_eq!(
            (r.phases[1].allowed_ms, r.phases[1].evidence_band),
            (10, false)
        );
        assert!(r.band_failures().is_empty());
        // Refused: no basis, an empty basis, a basis alone, a band on a fit phase.
        assert!(Plan::parse(&banded.replace("band_basis = \"boots\"\n", "")).is_err());
        assert!(
            Plan::parse(&banded.replace("band_basis = \"boots\"", "band_basis = \" \"")).is_err()
        );
        assert!(Plan::parse(&banded.replace("delta_band_ms = 12\n", "")).is_err());
        let on_fit = PLAN.replace(
            "device_ms = 110\nrole = \"fit\"\n",
            "device_ms = 110\nrole = \"fit\"\ndelta_band_ms = 12\nband_basis = \"boots\"\n",
        );
        assert!(Plan::parse(&on_fit).is_err());
    }

    #[test]
    fn a_plan_that_cannot_determine_its_parameters_is_refused() {
        let under = PLAN.replace("role = \"fit\"", "role = \"validation\"");
        assert!(Plan::parse(&under).is_err());
        let first_fit = PLAN.replacen("role = \"validation\"", "role = \"fit\"", 1);
        assert!(Plan::parse(&first_fit).is_err());
        let no_reason = PLAN.replace("reason = \"a test row\"\n", "");
        assert!(Plan::parse(&no_reason).is_err());
    }

    #[test]
    fn writing_device_values_changes_only_their_lines() {
        let out = write_device_values(PLAN, &[("a", 200)]).expect("the row exists");
        assert!(out.contains("device = 200"));
        assert_eq!(out.lines().count(), PLAN.lines().count());
        assert_eq!(
            out.replace("device = 200", "device = 5"),
            PLAN,
            "nothing else moved"
        );
        assert!(write_device_values(PLAN, &[("b", 1)]).is_err());
    }
}
