//! Pure, bytes-in bundles and asset lists: the flash image a machine boots from, the `.pebundle`
//! container, the `corpus.toml` map and the small TOML reader both use. The file system and the
//! environment stay in `pemu-host::assets`.

use std::sync::Arc;

use crate::esp_image::MergedImage;
use crate::{LoadError, hex, parse_sha256_hex, sha256};

/// Flash bytes from address 0, shared because a fork shares the flash base. A shorter image is
/// the leading bytes with the rest erased; the SoC refuses a longer one.
#[derive(Clone, Default)]
pub struct FlashImage {
    bytes: Arc<[u8]>,
}

impl FlashImage {
    pub fn erased() -> FlashImage {
        FlashImage::default()
    }

    /// Validates through [`MergedImage::parse`] so a malformed image is a [`LoadError`], never a
    /// panic; the bytes are kept unchanged.
    ///
    /// UNVERIFIED signature: no design fixes this function's shape.
    pub fn from_merged(bytes: &[u8]) -> Result<FlashImage, LoadError> {
        MergedImage::parse(bytes)?;
        Ok(FlashImage {
            bytes: Arc::from(bytes),
        })
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Prints the length only, so a debug print never puts megabytes or a device's NVS into a log.
impl std::fmt::Debug for FlashImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlashImage")
            .field("len", &self.bytes.len())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum TomlEntry {
    Str(String),
    /// A one-line inline table of quoted strings.
    Inline(Vec<(String, String)>),
    /// Anything else, kept verbatim (numbers, booleans).
    Raw(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TomlTable {
    pub name: String,
    /// True for an `[[array of tables]]` header.
    pub array: bool,
    pub keys: Vec<(String, TomlEntry)>,
}

impl TomlTable {
    pub fn string(&self, key: &str) -> Option<&str> {
        match self.entry(key)? {
            TomlEntry::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn integer(&self, key: &str) -> Option<u64> {
        match self.entry(key)? {
            TomlEntry::Raw(s) => s.parse().ok(),
            _ => None,
        }
    }

    pub fn inline(&self, key: &str) -> Option<&[(String, String)]> {
        match self.entry(key)? {
            TomlEntry::Inline(pairs) => Some(pairs),
            _ => None,
        }
    }

    pub fn inline_string(&self, key: &str, sub: &str) -> Option<&str> {
        self.inline(key)?
            .iter()
            .find(|(k, _)| k == sub)
            .map(|(_, v)| v.as_str())
    }

    fn entry(&self, key: &str) -> Option<&TomlEntry> {
        self.keys.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }
}

/// The TOML subset the asset files use: `[table]` and `[[array]]` headers, `key = "string"`,
/// `key = 123` and one-line inline tables of strings with no comma or quote in a value. Other
/// lines are skipped, so a newer file with extra keys still reads.
///
/// UNVERIFIED placeholder: neither `pemu-loader` nor `pemu-host` may depend on the `toml` crate,
/// and no design names a replacement.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct TomlLite {
    tables: Vec<TomlTable>,
}

impl TomlLite {
    /// Never fails: unparsable lines are skipped.
    pub fn parse(text: &str) -> TomlLite {
        let mut doc = TomlLite::default();
        for line in text.lines().map(str::trim) {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix('[') {
                let (name, array) = match rest.strip_prefix('[') {
                    Some(inner) => (inner.trim_end_matches(']'), true),
                    None => (rest.trim_end_matches(']'), false),
                };
                doc.tables.push(TomlTable {
                    name: name.trim().to_owned(),
                    array,
                    keys: Vec::new(),
                });
                continue;
            }
            let (Some(table), Some((key, value))) = (doc.tables.last_mut(), line.split_once('='))
            else {
                continue;
            };
            table
                .keys
                .push((key.trim().to_owned(), parse_value(value.trim())));
        }
        doc
    }

    pub fn tables(&self) -> &[TomlTable] {
        &self.tables
    }

    /// The first table with this name.
    pub fn table(&self, name: &str) -> Option<&TomlTable> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// `table.key` as a string, such as the config key `rom.rev101`.
    pub fn string(&self, table: &str, key: &str) -> Option<&str> {
        self.table(table)?.string(key)
    }
}

fn parse_value(value: &str) -> TomlEntry {
    if let Some(text) = unquote(value) {
        return TomlEntry::Str(text.to_owned());
    }
    if let Some(inner) = value.strip_prefix('{').and_then(|v| v.strip_suffix('}')) {
        let pairs = inner
            .split(',')
            .filter_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                Some((key.trim().to_owned(), unquote(value.trim())?.to_owned()))
            })
            .collect();
        return TomlEntry::Inline(pairs);
    }
    TomlEntry::Raw(value.to_owned())
}

fn unquote(value: &str) -> Option<&str> {
    value.strip_prefix('"')?.strip_suffix('"')
}

pub const CORPUS_SHA_KEY: &str = "sha256";

/// The ids `corpus.toml` is expected to carry, so `doctor` reports a missing one instead of
/// staying silent. `rom101` and `rom3` are bundled (`assets/rom/pins.toml`); probe images are
/// built per probe, so their set is not fixed.
pub const CORPUS_IDS: [&str; 10] = [
    "rom0",
    "pk",
    "official",
    "goldminer",
    "probe-long",
    "qemu-oracle",
    "probe2",
    "scan3",
    "pkgatt",
    "demo",
];
pub const CORPUS_BIN: &str = "bin";
pub const CORPUS_ELF: &str = "elf";
pub const CORPUS_BOOT_ELF: &str = "boot_elf";
pub const CORPUS_PT: &str = "pt";

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CorpusFile {
    pub kind: String,
    /// Exactly as written, `~` included; `pemu-host::assets` expands it.
    pub path: String,
    pub sha256: Option<[u8; 32]>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CorpusEntry {
    pub id: String,
    pub files: Vec<CorpusFile>,
}

impl CorpusEntry {
    pub fn file(&self, kind: &str) -> Option<&CorpusFile> {
        self.files.iter().find(|f| f.kind == kind)
    }
}

/// `~/.config/passportsim/corpus.toml`: corpus ids mapped to paths and SHA-256. Parsed here so the
/// browser and tests share one reader; `pemu-host::assets` reads the file and applies overrides.
///
/// UNVERIFIED placeholder: the file and its keys are fixed, this type is not.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct CorpusMap {
    entries: Vec<CorpusEntry>,
}

impl CorpusMap {
    /// Every string key of an `[id]` table other than `sha256` is a file kind, with its digest in
    /// the `sha256` inline table. A table with no file key (a future `[meta]`, say) is skipped.
    pub fn parse(text: &str) -> CorpusMap {
        let doc = TomlLite::parse(text);
        let entries = doc
            .tables()
            .iter()
            .filter(|table| !table.array && !table.name.is_empty())
            .filter_map(|table| {
                let files: Vec<CorpusFile> = table
                    .keys
                    .iter()
                    .filter(|(key, _)| key != CORPUS_SHA_KEY)
                    .filter_map(|(key, value)| match value {
                        TomlEntry::Str(path) => Some(CorpusFile {
                            kind: key.clone(),
                            path: path.clone(),
                            sha256: table
                                .inline_string(CORPUS_SHA_KEY, key)
                                .and_then(parse_sha256_hex),
                        }),
                        _ => None,
                    })
                    .collect();
                (!files.is_empty()).then(|| CorpusEntry {
                    id: table.name.clone(),
                    files,
                })
            })
            .collect();
        CorpusMap { entries }
    }

    pub fn entries(&self) -> &[CorpusEntry] {
        &self.entries
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.id.as_str())
    }

    pub fn get(&self, id: &str) -> Option<&CorpusEntry> {
        self.entries.iter().find(|e| e.id == id)
    }

    /// The [`CORPUS_IDS`] this map does not list; `doctor` reports them, not as failures.
    pub fn missing_ids(&self) -> Vec<&'static str> {
        CORPUS_IDS
            .into_iter()
            .filter(|id| self.get(id).is_none())
            .collect()
    }
}

pub const BUNDLE_MAGIC: [u8; 8] = *b"PEBUNDL1";
/// [`BUNDLE_MAGIC`] and the little-endian manifest length.
pub const BUNDLE_HEADER_LEN: usize = 12;
pub const BUNDLE_FLASH: &str = "flash";
pub const BUNDLE_APP_ELF: &str = "app_elf";
pub const BUNDLE_BOOT_ELF: &str = "boot_elf";
pub const BUNDLE_PARTITION_TABLE: &str = "partition_table";
/// Importing an eFuse dump taints the machine.
pub const BUNDLE_EFUSE: &str = "efuse";

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct BundleFile {
    /// One of the `BUNDLE_*` roles, or a newer role this build does not know.
    pub role: String,
    /// Original file name, for messages and artifacts.
    pub name: String,
    /// Checked when the bundle is parsed.
    pub sha256: [u8; 32],
    pub offset: usize,
    pub len: usize,
}

/// A `.pebundle`: one file carrying a run's firmware assets, for `passportsim run`, the browser's
/// drag and drop and `xtask package`.
///
/// Layout: [`BUNDLE_MAGIC`], a little-endian `u32` manifest length, the manifest (the
/// [`TomlLite`] subset: an optional `[bundle]` table with `id` and `name`, one `[[file]]` table per
/// payload with `role`, `name`, `len`, `sha256`), then the payloads back to back in manifest
/// order. Every digest is verified on parse. UNVERIFIED placeholder: this layout is chosen here,
/// not fixed by a design.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Bundle<'a> {
    bytes: &'a [u8],
    id: Option<String>,
    name: Option<String>,
    files: Vec<BundleFile>,
}

impl<'a> Bundle<'a> {
    pub fn parse(bytes: &'a [u8]) -> Result<Bundle<'a>, LoadError> {
        let magic = crate::slice(bytes, 0, BUNDLE_MAGIC.len(), "pebundle")?;
        if magic != BUNDLE_MAGIC {
            return Err(LoadError::BadMagic {
                what: "pebundle",
                offset: 0,
                found: crate::le_u32(bytes, 0, "pebundle")?,
            });
        }
        let manifest_len = crate::le_u32(bytes, 8, "pebundle manifest length")? as usize;
        let manifest = crate::slice(bytes, BUNDLE_HEADER_LEN, manifest_len, "pebundle manifest")?;
        let text = core::str::from_utf8(manifest).map_err(|e| LoadError::Malformed {
            what: "pebundle manifest",
            detail: e.to_string(),
        })?;
        let doc = TomlLite::parse(text);
        let meta = doc.table("bundle");
        let mut offset = BUNDLE_HEADER_LEN + manifest_len;
        let mut files = Vec::new();
        for table in doc.tables().iter().filter(|t| t.array && t.name == "file") {
            let field = |key: &str| LoadError::Malformed {
                what: "pebundle manifest",
                detail: format!("[[file]] without {key}"),
            };
            let role = table.string("role").ok_or_else(|| field("role"))?;
            let len = table.integer("len").ok_or_else(|| field("len"))? as usize;
            let sha256 = table
                .string(CORPUS_SHA_KEY)
                .and_then(parse_sha256_hex)
                .ok_or_else(|| field("sha256"))?;
            let data = crate::slice(bytes, offset, len, "pebundle payload")?;
            if sha256 != crate::sha256(data) {
                return Err(LoadError::Malformed {
                    what: "pebundle payload",
                    detail: format!("{role}: SHA-256 is not {}", hex(&sha256)),
                });
            }
            files.push(BundleFile {
                role: role.to_owned(),
                name: table.string("name").unwrap_or(role).to_owned(),
                sha256,
                offset,
                len,
            });
            offset += len;
        }
        Ok(Bundle {
            bytes,
            id: meta.and_then(|t| t.string("id")).map(str::to_owned),
            name: meta.and_then(|t| t.string("name")).map(str::to_owned),
            files,
        })
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn files(&self) -> &[BundleFile] {
        &self.files
    }

    /// The first file of a role.
    pub fn file(&self, role: &str) -> Option<&BundleFile> {
        self.files.iter().find(|f| f.role == role)
    }

    pub fn data(&self, file: &BundleFile) -> &'a [u8] {
        &self.bytes[file.offset..file.offset + file.len]
    }

    pub fn role_data(&self, role: &str) -> Option<&'a [u8]> {
        self.file(role).map(|f| self.data(f))
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct BundleInput<'a> {
    pub role: &'a str,
    pub name: &'a str,
    pub bytes: &'a [u8],
}

/// Writes the format [`Bundle::parse`] reads.
pub fn build(id: Option<&str>, name: Option<&str>, files: &[BundleInput<'_>]) -> Vec<u8> {
    let mut manifest = String::from("[bundle]\n");
    if let Some(id) = id {
        manifest.push_str(&format!("id = \"{id}\"\n"));
    }
    if let Some(name) = name {
        manifest.push_str(&format!("name = \"{name}\"\n"));
    }
    for file in files {
        manifest.push_str(&format!(
            "\n[[file]]\nrole = \"{}\"\nname = \"{}\"\nlen = {}\nsha256 = \"{}\"\n",
            file.role,
            file.name,
            file.bytes.len(),
            hex(&sha256(file.bytes))
        ));
    }
    let mut out = Vec::with_capacity(BUNDLE_HEADER_LEN + manifest.len());
    out.extend_from_slice(&BUNDLE_MAGIC);
    out.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
    out.extend_from_slice(manifest.as_bytes());
    for file in files {
        out.extend_from_slice(file.bytes);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CORPUS: &str = r#"
# Generated by the bootstrap.
[pk]
bin = "~/Library/Application Support/passportsim/corpus/pk/FoloToy-AI-Passport-8MB.bin"
elf = "~/Library/Application Support/passportsim/corpus/pk/FoloToy-AI-Passport.elf"
sha256 = { bin = "1a2aff102b82523b8992981c9ff80bbebbd60247f8a004d9f5bf5181efb5958a", elf = "7d711d0b56d2f051e291ece7b3d7f5850463f575faeba1389e172f6b6ce1f2fd" }

[goldminer]
bin = "~/corpus/goldminer/goldminer-sanitized-8MB.bin"
sha256 = { bin = "bf950fe5280cb4d0d29f3fded5ebdc15d0557f173df254d0b65509abc2cf98f9" }
"#;

    #[test]
    fn corpus_map_reads_ids_paths_and_digests() {
        let map = CorpusMap::parse(CORPUS);
        assert_eq!(map.ids().collect::<Vec<_>>(), ["pk", "goldminer"]);
        let pk = map.get("pk").expect("pk");
        assert_eq!(pk.files.len(), 2);
        let bin = pk.file(CORPUS_BIN).expect("bin");
        assert!(bin.path.ends_with("FoloToy-AI-Passport-8MB.bin"));
        assert_eq!(
            bin.sha256.map(|s| hex(&s)[..16].to_owned()),
            Some("1a2aff102b82523b".to_owned())
        );
        assert_eq!(
            pk.file(CORPUS_ELF)
                .and_then(|f| f.sha256)
                .map(|s| hex(&s)[..16].to_owned()),
            Some("7d711d0b56d2f051".to_owned())
        );
        assert!(pk.file(CORPUS_BOOT_ELF).is_none());
        assert!(map.get("nope").is_none());
        assert_eq!(map.get("goldminer").expect("goldminer").files.len(), 1);
    }

    #[test]
    fn missing_plan_ids_are_listed_and_meta_tables_are_not_ids() {
        let map = CorpusMap::parse(CORPUS);
        assert!(!map.missing_ids().contains(&"pk"));
        assert!(!map.missing_ids().contains(&"goldminer"));
        for id in ["rom0", "official", "probe2", "qemu-oracle", "demo"] {
            assert!(map.missing_ids().contains(&id), "{id}");
        }
        assert_eq!(map.missing_ids().len(), CORPUS_IDS.len() - 2);
        assert!(!CORPUS_IDS.contains(&"rom101") && !CORPUS_IDS.contains(&"rom3"));
        let meta = CorpusMap::parse("[meta]\nversion = 2\n\n[pk]\nbin = \"/tmp/pk.bin\"\n");
        assert_eq!(meta.ids().collect::<Vec<_>>(), ["pk"]);
    }

    #[test]
    fn from_merged_validates_the_flash_bytes() {
        assert!(matches!(
            FlashImage::from_merged(&[]),
            Err(LoadError::Truncated { .. })
        ));
        let junk = vec![0u8; 0x20_000];
        assert!(matches!(
            FlashImage::from_merged(&junk),
            Err(LoadError::BadMagic { .. })
        ));
    }

    #[test]
    fn toml_lite_reads_config_keys_and_arrays() {
        let doc = TomlLite::parse("[rom]\nrev101 = \"/tmp/a.elf\"\nrev3 = \"/tmp/b.elf\"\nn = 7\n");
        assert_eq!(doc.string("rom", "rev101"), Some("/tmp/a.elf"));
        assert_eq!(doc.string("rom", "rev3"), Some("/tmp/b.elf"));
        assert_eq!(doc.table("rom").and_then(|t| t.integer("n")), Some(7));
        assert_eq!(doc.string("rom", "missing"), None);
        assert_eq!(doc.string("none", "rev3"), None);
        let arrays = TomlLite::parse("[[file]]\nrole = \"flash\"\n[[file]]\nrole = \"app_elf\"\n");
        assert_eq!(arrays.tables().len(), 2);
        assert!(arrays.tables().iter().all(|t| t.array && t.name == "file"));
    }

    #[test]
    fn a_pebundle_round_trips_and_checks_its_digests() {
        let flash = vec![0xFFu8; 64];
        let elf = b"\x7fELF-not-really".to_vec();
        let bytes = build(
            Some("abc123"),
            Some("official demo"),
            &[
                BundleInput {
                    role: BUNDLE_FLASH,
                    name: "merged.bin",
                    bytes: &flash,
                },
                BundleInput {
                    role: BUNDLE_APP_ELF,
                    name: "app.elf",
                    bytes: &elf,
                },
            ],
        );
        let bundle = Bundle::parse(&bytes).expect("bundle");
        assert_eq!(bundle.id(), Some("abc123"));
        assert_eq!(bundle.name(), Some("official demo"));
        assert_eq!(bundle.files().len(), 2);
        assert_eq!(bundle.role_data(BUNDLE_FLASH), Some(&flash[..]));
        assert_eq!(bundle.role_data(BUNDLE_APP_ELF), Some(&elf[..]));
        assert_eq!(bundle.role_data(BUNDLE_EFUSE), None);
        assert_eq!(
            bundle.file(BUNDLE_APP_ELF).map(|f| f.name.as_str()),
            Some("app.elf")
        );
        assert_eq!(bundle.file(BUNDLE_FLASH).map(|f| f.len), Some(64));

        let mut broken = bytes.clone();
        let at = bundle.file(BUNDLE_FLASH).expect("flash").offset;
        broken[at] ^= 1;
        assert!(matches!(
            Bundle::parse(&broken),
            Err(LoadError::Malformed {
                what: "pebundle payload",
                ..
            })
        ));
        let mut bad_magic = bytes.clone();
        bad_magic[7] = b'0';
        assert!(matches!(
            Bundle::parse(&bad_magic),
            Err(LoadError::BadMagic { .. })
        ));
        assert!(matches!(
            Bundle::parse(&bytes[..bytes.len() - 1]),
            Err(LoadError::Truncated { .. })
        ));
        assert!(Bundle::parse(&[]).is_err());
    }

    #[test]
    fn a_manifest_without_a_required_key_is_refused() {
        let manifest = "[bundle]\n\n[[file]]\nrole = \"flash\"\nname = \"x.bin\"\n";
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&BUNDLE_MAGIC);
        bytes.extend_from_slice(&(manifest.len() as u32).to_le_bytes());
        bytes.extend_from_slice(manifest.as_bytes());
        assert!(matches!(
            Bundle::parse(&bytes),
            Err(LoadError::Malformed {
                what: "pebundle manifest",
                ..
            })
        ));
    }
}
