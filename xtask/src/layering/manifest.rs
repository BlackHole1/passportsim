//! Reads a crate's `Cargo.toml` (with the `toml` crate) into the facts the
//! layering rules need: dependencies of every kind, features and explicit target paths.

use std::collections::BTreeMap;

/// Kind of a dependency table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DepKind {
    /// `[dependencies]` or `[target.'..'.dependencies]`.
    Normal,
    /// `[dev-dependencies]`.
    Dev,
    /// `[build-dependencies]`.
    Build,
}

impl DepKind {
    /// Table name.
    pub fn label(self) -> &'static str {
        match self {
            DepKind::Normal => "dependencies",
            DepKind::Dev => "dev-dependencies",
            DepKind::Build => "build-dependencies",
        }
    }
}

/// One dependency entry.
#[derive(Clone, Debug)]
pub struct Dep {
    /// Key in the dependency table.
    pub key: String,
    /// Package name (`package = ..` of the entry or of the inherited workspace entry, else the key).
    pub package: String,
    /// Table kind.
    pub kind: DepKind,
    /// `optional = true`.
    pub optional: bool,
    /// The entry's own `features = [..]` list (not the inherited workspace entry's).
    pub features: Vec<String>,
    /// 1-based line of the entry in the manifest (1 when not found).
    pub line: usize,
}

/// An explicit target path from the manifest.
#[derive(Clone, Debug)]
pub struct Target {
    /// Path relative to the crate directory.
    pub path: String,
    /// A test, bench, example or build-script target (not part of the library build).
    pub test: bool,
}

/// The facts of one manifest.
#[derive(Clone, Debug, Default)]
pub struct Manifest {
    /// Package name.
    pub name: String,
    /// All dependencies, target-specific tables included.
    pub deps: Vec<Dep>,
    /// `[features]`.
    pub features: BTreeMap<String, Vec<String>>,
    /// Explicit target paths (`[lib]`, `[[bin]]`, `[[test]]`, `[[bench]]`, `[[example]]`,
    /// `package.build`).
    pub targets: Vec<Target>,
}

/// Parses a crate manifest. `workspace_deps` is the root `[workspace.dependencies]` table, used
/// to resolve `package = ..` renames of inherited entries.
pub fn parse(text: &str, workspace_deps: Option<&toml::Table>) -> Result<Manifest, String> {
    let table: toml::Table = text.parse().map_err(|e| format!("invalid TOML: {e}"))?;
    let package = table
        .get("package")
        .and_then(toml::Value::as_table)
        .ok_or("no [package] table")?;
    let name = package
        .get("name")
        .and_then(toml::Value::as_str)
        .ok_or("no package name")?
        .to_string();
    let mut deps = Vec::new();
    collect_deps(&table, text, workspace_deps, &mut deps);
    if let Some(targets) = table.get("target").and_then(toml::Value::as_table) {
        for cfg in targets.values().filter_map(toml::Value::as_table) {
            collect_deps(cfg, text, workspace_deps, &mut deps);
        }
    }
    let mut features = BTreeMap::new();
    if let Some(table) = table.get("features").and_then(toml::Value::as_table) {
        for (feature, list) in table {
            let items = list
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(toml::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            features.insert(feature.clone(), items);
        }
    }
    let mut targets = Vec::new();
    if let Some(path) = package.get("build").and_then(toml::Value::as_str) {
        targets.push(Target {
            path: path.to_string(),
            test: true,
        });
    }
    if let Some(path) = table
        .get("lib")
        .and_then(|l| l.get("path"))
        .and_then(toml::Value::as_str)
    {
        targets.push(Target {
            path: path.to_string(),
            test: false,
        });
    }
    for (section, test) in [
        ("bin", false),
        ("test", true),
        ("bench", true),
        ("example", true),
    ] {
        let entries = table.get(section).and_then(toml::Value::as_array);
        for entry in entries.into_iter().flatten() {
            if let Some(path) = entry.get("path").and_then(toml::Value::as_str) {
                targets.push(Target {
                    path: path.to_string(),
                    test,
                });
            }
        }
    }
    Ok(Manifest {
        name,
        deps,
        features,
        targets,
    })
}

fn collect_deps(table: &toml::Table, text: &str, ws: Option<&toml::Table>, out: &mut Vec<Dep>) {
    let sections = [
        ("dependencies", DepKind::Normal),
        ("dev-dependencies", DepKind::Dev),
        ("dev_dependencies", DepKind::Dev),
        ("build-dependencies", DepKind::Build),
        ("build_dependencies", DepKind::Build),
    ];
    for (section, kind) in sections {
        let Some(entries) = table.get(section).and_then(toml::Value::as_table) else {
            continue;
        };
        for (key, value) in entries {
            let field = |name: &str| value.as_table().and_then(|e| e.get(name));
            let inherited = field("workspace").and_then(toml::Value::as_bool) == Some(true);
            let ws_entry = ws
                .filter(|_| inherited)
                .and_then(|w| w.get(key))
                .and_then(toml::Value::as_table);
            let package = field("package")
                .or_else(|| ws_entry.and_then(|w| w.get("package")))
                .and_then(toml::Value::as_str)
                .unwrap_or(key)
                .to_string();
            out.push(Dep {
                key: key.clone(),
                package,
                kind,
                optional: field("optional").and_then(toml::Value::as_bool) == Some(true),
                features: string_list(field("features")),
                line: dep_line(text, kind, key),
            });
        }
    }
}

/// The strings of an array value, or nothing.
pub fn string_list(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(toml::Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Line of `key` inside a table of `kind`, by a textual scan of the manifest.
fn dep_line(text: &str, kind: DepKind, key: &str) -> usize {
    let mut in_section = false;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if let Some(header) = line.strip_prefix('[') {
            let header = header.trim_start_matches('[').trim_end_matches(']').trim();
            let header = header.replace('_', "-");
            if header.ends_with(&format!("{}.{key}", kind.label()))
                && header_kind(&header, key) == Some(kind)
            {
                return n + 1;
            }
            in_section = header_kind(&header, "") == Some(kind);
        } else if in_section
            && let Some(rest) = line.strip_prefix(key)
            && rest.starts_with([' ', '.', '='])
        {
            return n + 1;
        }
    }
    1
}

/// Kind of a table header, with an optional trailing `.key`.
fn header_kind(header: &str, key: &str) -> Option<DepKind> {
    let base = if key.is_empty() {
        header
    } else {
        header.strip_suffix(key)?.strip_suffix('.')?
    };
    if base.ends_with("dev-dependencies") {
        Some(DepKind::Dev)
    } else if base.ends_with("build-dependencies") {
        Some(DepKind::Build)
    } else if base.ends_with("dependencies") {
        Some(DepKind::Normal)
    } else {
        None
    }
}
