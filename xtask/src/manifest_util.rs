//! Helpers shared by the hand-written TOML manifests of `xtask probes` and `xtask riscv-tests`:
//! file digests, string escaping and typed getters whose errors name the manifest file.

use std::path::Path;

/// SHA-256 of a file, as lowercase hex, with its size.
pub fn digest_file(path: &Path) -> Result<(String, u64), String> {
    let bytes = std::fs::read(path)
        .map_err(|err| format!("cannot read {}: {}", path.display(), err.kind()))?;
    Ok((
        pemu_loader::hex(&pemu_loader::sha256(&bytes)),
        bytes.len() as u64,
    ))
}

/// Backslashes and double quotes escaped for a TOML basic string.
pub fn escape(text: &str) -> String {
    text.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The string `key` of `table`, in the manifest `file`.
pub fn string(table: &toml::Table, key: &str, file: &str) -> Result<String, String> {
    table
        .get(key)
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{file} has no string `{key}`"))
}

/// The non-negative integer `key` of `table`, in the manifest `file`.
pub fn integer(table: &toml::Table, key: &str, file: &str) -> Result<u64, String> {
    table
        .get(key)
        .and_then(|value| value.as_integer())
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| format!("{file} has no whole number `{key}`"))
}

/// The array of tables `[[key]]` of `table`, in the manifest `file`.
pub fn array<'a>(
    table: &'a toml::Table,
    key: &str,
    file: &str,
) -> Result<Vec<&'a toml::Table>, String> {
    let Some(value) = table.get(key) else {
        return Err(format!("{file} has no `[[{key}]]`"));
    };
    value
        .as_array()
        .ok_or_else(|| format!("`{key}` of {file} is not an array"))?
        .iter()
        .map(|item| {
            item.as_table()
                .ok_or_else(|| format!("an entry of `[[{key}]]` is not a table"))
        })
        .collect()
}
