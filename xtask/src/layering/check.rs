//! Applies the layering rules to an in-memory workspace.

use std::collections::BTreeSet;

use super::manifest::{self, Dep, DepKind, Manifest};
use super::policy::{self, DEVICE_CRATE, DEVICE_FEATURE, ThirdParty};
use super::source::Gate;
use super::{Report, Rule, Violation, WorkspaceInput, clippy, walk};

/// Checks every rule over `ws`.
pub fn check(ws: &WorkspaceInput) -> Result<Report, String> {
    let root: toml::Table = ws
        .root_manifest
        .parse()
        .map_err(|e| format!("root Cargo.toml: invalid TOML: {e}"))?;
    let ws_deps = root
        .get("workspace")
        .and_then(|w| w.get("dependencies"))
        .and_then(toml::Value::as_table);
    let mut manifests = Vec::new();
    for c in &ws.crates {
        let m = manifest::parse(&c.manifest, ws_deps)
            .map_err(|e| format!("{}/Cargo.toml: {e}", c.dir))?;
        manifests.push(m);
    }
    let members: BTreeSet<&str> = manifests.iter().map(|m| m.name.as_str()).collect();
    let reach = policy::reachable();
    let mut out = Vec::new();
    let mut files = 0;

    for (c, m) in ws.crates.iter().zip(&manifests) {
        let name = m.name.as_str();
        let mut push = |rule, file: String, line, detail: String| {
            out.push(Violation {
                rule,
                krate: name.to_string(),
                file,
                line,
                detail,
            });
        };
        let manifest_file = join(&c.dir, "Cargo.toml");
        let row = policy::crate_policy(name);
        if row.is_none() {
            push(
                Rule::CrateTable,
                manifest_file.clone(),
                1,
                "crate is missing from the crate table in xtask/src/layering/policy.rs".to_string(),
            );
        }
        let core = row.is_none_or(|r| r.core);

        for dep in &m.deps {
            let pkg = dep.package.as_str();
            let kind = dep.kind.label();
            if members.contains(pkg) || pkg.starts_with("pemu-") {
                if !reach.get(name).is_some_and(|r| r.contains(pkg)) {
                    let detail = format!(
                        "`{pkg}` ({kind}) is not reachable from `{name}` in the layering graph"
                    );
                    push(Rule::CrateEdge, manifest_file.clone(), dep.line, detail);
                }
                continue;
            }
            let allowed = match row.map(|r| r.third_party) {
                Some(ThirdParty::Any) => true,
                Some(ThirdParty::Only(list)) => list.contains(&pkg),
                None => false,
            };
            if !allowed {
                let detail = format!(
                    "third-party `{pkg}` ({kind}) is not allowed for `{name}` (see `CRATES` in `xtask/src/layering/policy.rs`)"
                );
                push(Rule::ThirdParty, manifest_file.clone(), dep.line, detail);
            }
            if policy::DEVICE_ONLY_DEPS.contains(&(name, pkg)) && !serial_dep_allowed(m, dep) {
                let detail = format!(
                    "`{pkg}` in `{name}` outside an optional dependency enabled only by feature `{DEVICE_FEATURE}` (layering rules 1 and 5)"
                );
                push(Rule::SerialDevice, manifest_file.clone(), dep.line, detail);
            }
            if policy::WINDOWS_BINDINGS.contains(&pkg) {
                for feature in device_windows_features(&dep.features) {
                    let detail = format!(
                        "`{pkg}` feature `{feature}` in the dependency's own list; it may be enabled only through feature `{DEVICE_FEATURE}`"
                    );
                    push(Rule::SerialDevice, manifest_file.clone(), dep.line, detail);
                }
            }
            if policy::is_serial_crate(pkg) && !serial_dep_allowed(m, dep) {
                let detail = format!(
                    "serial-port crate `{pkg}` outside an optional dependency of `{DEVICE_CRATE}` enabled only by feature `{DEVICE_FEATURE}` (layering rule 5)"
                );
                push(Rule::SerialDevice, manifest_file.clone(), dep.line, detail);
            }
        }

        // The serial-device features of the Windows bindings, only through feature `device`, and
        // only in the crates allowed each one.
        for (feature, items) in &m.features {
            for item in items {
                let Some((dep_key, wanted)) = item.split_once('/') else {
                    continue;
                };
                let dep_key = dep_key.trim_end_matches('?');
                let binding = m
                    .deps
                    .iter()
                    .find(|d| d.key == dep_key)
                    .map_or(dep_key, |d| d.package.as_str());
                if !policy::WINDOWS_BINDINGS.contains(&binding) {
                    continue;
                }
                let Some((_, crates)) = policy::DEVICE_WINDOWS_FEATURES
                    .iter()
                    .find(|(f, _)| *f == wanted)
                else {
                    continue;
                };
                if feature != DEVICE_FEATURE || !crates.contains(&name) {
                    let detail = format!(
                        "`{binding}` feature `{wanted}` enabled by feature `{feature}` of `{name}`; only feature `{DEVICE_FEATURE}` of {} may enable it",
                        crates.join(" or ")
                    );
                    push(Rule::SerialDevice, manifest_file.clone(), 1, detail);
                }
            }
        }

        let device_ok = |g: Gate| name == DEVICE_CRATE && g.device;
        let scans = walk::scan_crate(&m.targets, &c.files);
        files += scans.len();
        for (path, scan) in &scans {
            let file = join(&c.dir, path);
            for f in &scan.std_uses {
                if core && !f.gate.test && !device_ok(f.gate) {
                    let detail = format!(
                        "`{}` in non-test code of a core crate (layering rule 1)",
                        f.what
                    );
                    push(Rule::CoreStdApi, file.clone(), f.line, detail);
                }
            }
            for f in &scan.device_refs {
                if !f.gate.test && !device_ok(f.gate) {
                    let detail = format!(
                        "{} outside `{DEVICE_CRATE}` feature `{DEVICE_FEATURE}` (layering rule 5)",
                        f.what
                    );
                    push(Rule::SerialDevice, file.clone(), f.line, detail);
                }
            }
            // The call site that opens a port, not only a port named in source. A short audited
            // list of files may still name these (`policy::PORT_OPEN_EXEMPT`).
            if !policy::port_open_exempt(name, &path.replace('\\', "/")) {
                for f in &scan.port_opens {
                    if !f.gate.test && !device_ok(f.gate) {
                        let detail = format!(
                            "{} outside `{DEVICE_CRATE}` feature `{DEVICE_FEATURE}` (layering rule 5)",
                            f.what
                        );
                        push(Rule::SerialDevice, file.clone(), f.line, detail);
                    }
                }
            }
        }

        if core {
            let clippy_file = join(&c.dir, "clippy.toml");
            for problem in
                clippy::check_core(c.clippy_toml.as_deref(), ws.root_clippy_toml.as_deref())
            {
                push(Rule::ClippyConfig, clippy_file.clone(), 1, problem);
            }
        }
    }
    // The workspace entry of a Windows binding is inherited by every crate that names it, so a
    // serial-device feature there would be on in every build.
    for (key, value) in ws_deps.into_iter().flatten() {
        let package = value
            .get("package")
            .and_then(toml::Value::as_str)
            .unwrap_or(key);
        if !policy::WINDOWS_BINDINGS.contains(&package) {
            continue;
        }
        for feature in device_windows_features(&manifest::string_list(value.get("features"))) {
            out.push(Violation {
                rule: Rule::SerialDevice,
                krate: "workspace".to_string(),
                file: "Cargo.toml".to_string(),
                line: 1,
                detail: format!(
                    "`{package}` feature `{feature}` in `[workspace.dependencies]`; it may be enabled only through feature `{DEVICE_FEATURE}`"
                ),
            });
        }
    }
    for problem in clippy::check_root(ws.root_clippy_toml.as_deref()) {
        out.push(Violation {
            rule: Rule::ClippyConfig,
            krate: "workspace".to_string(),
            file: "clippy.toml".to_string(),
            line: 1,
            detail: problem,
        });
    }
    out.sort_by(|a, b| (a.rule, &a.file, a.line).cmp(&(b.rule, &b.file, b.line)));
    Ok(Report {
        violations: out,
        crates: ws.crates.len(),
        files,
    })
}

/// A serial-port crate is allowed only as an optional normal dependency of `DEVICE_CRATE` that
/// feature `DEVICE_FEATURE` enables and no other feature does.
fn serial_dep_allowed(m: &Manifest, dep: &Dep) -> bool {
    let enables = |items: &Vec<String>| {
        items.iter().any(|i| {
            i == &dep.key
                || i == &format!("dep:{}", dep.key)
                || i.starts_with(&format!("{}/", dep.key))
                || i.starts_with(&format!("{}?/", dep.key))
        })
    };
    m.name == DEVICE_CRATE
        && dep.optional
        && dep.kind == DepKind::Normal
        && m.features.get(DEVICE_FEATURE).is_some_and(enables)
        && m.features
            .iter()
            .all(|(feature, items)| feature == DEVICE_FEATURE || !enables(items))
}

/// The entries of `features` that are serial-device features of a Windows binding
/// (`policy::DEVICE_WINDOWS_FEATURES`).
fn device_windows_features(features: &[String]) -> Vec<&str> {
    features
        .iter()
        .map(String::as_str)
        .filter(|f| policy::DEVICE_WINDOWS_FEATURES.iter().any(|(d, _)| d == f))
        .collect()
}

/// Joins a repository-relative directory and a relative path with `/`.
fn join(dir: &str, path: &str) -> String {
    if dir.is_empty() {
        path.to_string()
    } else {
        format!("{dir}/{path}")
    }
}
