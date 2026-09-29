//! The pure plan builder: from a merged image or a `flash_args`-style segment list to the exact
//! list of writes, collecting every refusal (not only the first). Opens no port.
//!
//! Written: the bootloader in [0x0, 0x8000), the partition table in [0x8000, 0x9000) and each app
//! partition, trimmed of trailing 0xFF. Data partitions (`nvs`, `phy_init`, `cardid`, ...) are
//! never written; an image with non-0xFF bytes there is refused rather than silently dropped.

use pemu_loader::esp_image::EspImage;
use pemu_loader::partitions::{Partition, PartitionTable, ptype, subtype};
use pemu_loader::{hex, sha256};

use crate::md5;
use crate::rules::{
    APP_MAX, APP_OFFSET, BOOTLOADER_END, BOOTLOADER_OFFSET, BackupEvidence, CARDID_END,
    CARDID_NAME, CARDID_OFFSET, CARDID_SIZE, DeviceFacts, FLASH_SIZE, Refusal, Rule, TABLE_END,
    TABLE_LEN, TABLE_OFFSET, check_backup, check_identity, check_image_cardid, check_write_range,
    round_to_sectors, touches_cardid,
};

#[derive(Copy, Clone, Debug)]
pub struct InputSegment<'a> {
    pub name: &'a str,
    pub offset: u32,
    pub data: &'a [u8],
}

#[derive(Copy, Clone, Debug)]
pub enum ImageSource<'a> {
    /// A merged image starting at flash offset 0, padded or not.
    Merged(&'a [u8]),
    Segments(&'a [InputSegment<'a>]),
}

/// Who asked for the plan. `--erase-nvs` is honoured only from a human at the CLI.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    HumanCli,
    Tool,
}

#[derive(Clone, Debug)]
pub struct PlanRequest<'a> {
    pub image: ImageSource<'a>,
    /// `erase_flash` or `--erase-all` was asked for. Always refused.
    pub erase_all: bool,
    /// Explicit erase regions `(offset, size)`. Always refused: the only erase is `nvs`.
    pub erase_regions: Vec<(u32, u32)>,
    pub erase_nvs: bool,
    pub origin: Origin,
}

impl<'a> PlanRequest<'a> {
    pub fn write(image: ImageSource<'a>, origin: Origin) -> PlanRequest<'a> {
        PlanRequest {
            image,
            erase_all: false,
            erase_regions: Vec::new(),
            erase_nvs: false,
            origin,
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct PlannedWrite {
    /// `bootloader`, `partition-table`, or the app partition name.
    pub name: String,
    pub offset: u32,
    /// The bytes, trimmed of trailing 0xFF.
    pub data: Vec<u8>,
    pub sha256: [u8; 32],
}

impl PlannedWrite {
    fn new(name: &str, offset: u32, data: &[u8]) -> PlannedWrite {
        PlannedWrite {
            name: name.to_owned(),
            offset,
            data: data.to_vec(),
            sha256: sha256(data),
        }
    }

    /// `[start, end)` rounded out to sectors, the range the flasher stub erases.
    pub fn sectors(&self) -> (u64, u64) {
        round_to_sectors(
            u64::from(self.offset),
            u64::from(self.offset) + self.data.len() as u64,
        )
    }
}

impl core::fmt::Debug for PlannedWrite {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("PlannedWrite")
            .field("name", &self.name)
            .field("offset", &format_args!("{:#x}", self.offset))
            .field("len", &self.data.len())
            .finish()
    }
}

/// A plan: exactly what will be written, bound by one digest the confirmation names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
    pub writes: Vec<PlannedWrite>,
    /// The `nvs` erase `(offset, size)`, present only for a human `--erase-nvs`.
    pub erase_nvs: Option<(u32, u32)>,
    /// SHA-256 over every write (offset, length, digest) and the erase.
    pub plan_sha256: [u8; 32],
    /// The app descriptor's ELF SHA-256, which the boot check compares; `None` when unreadable.
    pub app_elf_sha256: Option<[u8; 32]>,
}

impl Plan {
    fn bind(writes: Vec<PlannedWrite>, erase_nvs: Option<(u32, u32)>) -> Plan {
        let mut text = String::new();
        for w in &writes {
            text.push_str(&format!(
                "write {:#x} {:#x} {}\n",
                w.offset,
                w.data.len(),
                hex(&w.sha256)
            ));
        }
        if let Some((offset, size)) = erase_nvs {
            text.push_str(&format!("erase {offset:#x} {size:#x}\n"));
        }
        let app_elf_sha256 = writes
            .iter()
            .find(|w| w.offset >= APP_OFFSET)
            .and_then(|w| app_elf_sha256(&w.data));
        Plan {
            plan_sha256: sha256(text.as_bytes()),
            writes,
            erase_nvs,
            app_elf_sha256,
        }
    }

    pub fn plan_sha256_hex(&self) -> String {
        hex(&self.plan_sha256)
    }
}

/// The ELF SHA-256 of an app image's descriptor, which starts the first segment.
///
/// `image` is trimmed of trailing 0xFF, so an image whose appended digest ends in 0xFF would no
/// longer parse (seen with `probe_campaign_timing`). The device reads 0xFF there after the sector
/// erase, so the image is padded back with 0xFF to the end of its last 4 KiB sector.
fn app_elf_sha256(image: &[u8]) -> Option<[u8; 32]> {
    let mut flash = image.to_vec();
    flash.resize(image.len().next_multiple_of(4096), 0xFF);
    let parsed = EspImage::parse(&flash).ok()?;
    let desc = parsed.app_desc(&flash).ok()??;
    desc.has_elf_sha256().then_some(desc.app_elf_sha256)
}

/// The candidate plan and every refusal; accepted only when `refusals` is empty.
#[derive(Clone, Debug)]
pub struct PlanOutcome {
    /// The candidate writes, for display; never executed while a refusal stands.
    pub plan: Plan,
    /// Sorted by rule then detail.
    pub refusals: Vec<Refusal>,
}

impl PlanOutcome {
    pub fn accepted(&self) -> Option<&Plan> {
        self.refusals.is_empty().then_some(&self.plan)
    }

    pub fn refused_by(&self, rule: Rule) -> bool {
        self.refusals.iter().any(|r| r.rule == rule)
    }

    fn finish(plan: Plan, mut refusals: Vec<Refusal>) -> PlanOutcome {
        refusals.sort_by(|a, b| (a.rule, &a.detail).cmp(&(b.rule, &b.detail)));
        refusals.dedup();
        PlanOutcome { plan, refusals }
    }
}

fn trim_ff(data: &[u8]) -> &[u8] {
    let end = data.iter().rposition(|&b| b != 0xFF).map_or(0, |i| i + 1);
    &data[..end]
}

fn clip(image: &[u8], start: u32, end: u64) -> &[u8] {
    let s = (start as usize).min(image.len());
    let e = (end.min(image.len() as u64) as usize).max(s);
    &image[s..e]
}

/// The partition layout checks `verify_firmware.py` makes.
fn verify_layout(table: &PartitionTable, out: &mut Vec<Refusal>) {
    let fail = |out: &mut Vec<Refusal>, detail: String| {
        out.push(Refusal::new(Rule::VerifyFirmware, detail));
    };
    if !table.has_md5 {
        fail(out, "the partition table has no MD5 row".to_owned());
    }
    for p in &table.entries {
        if p.offset < TABLE_END || p.end() > u64::from(FLASH_SIZE) {
            fail(
                out,
                format!("partition `{}` lies outside [0x9000, 0x800000)", p.name),
            );
        }
    }
    match table.find("factory") {
        Some(p) if (p.ptype, p.subtype, p.offset, p.size) == (0, 0, APP_OFFSET, APP_MAX) => {}
        _ => fail(
            out,
            "`factory` is not app/factory at 0x10000 size 0x300000".to_owned(),
        ),
    }
    match table.find(CARDID_NAME) {
        Some(p) if (p.ptype, p.subtype, p.offset, p.size) == (1, 2, CARDID_OFFSET, CARDID_SIZE) => {
        }
        _ => fail(
            out,
            "`cardid` is not data/nvs at 0x356000 size 0x4000".to_owned(),
        ),
    }
    for (i, a) in table.entries.iter().enumerate() {
        for b in &table.entries[i + 1..] {
            if u64::from(a.offset) < b.end() && u64::from(b.offset) < a.end() {
                fail(
                    out,
                    format!("partitions `{}` and `{}` overlap", a.name, b.name),
                );
            }
        }
    }
}

/// Builds the plan for `request` and collects every refusal. With `facts`, the device identity
/// rules are judged too, with no port.
pub fn plan_flash(request: &PlanRequest<'_>, facts: Option<&DeviceFacts>) -> PlanOutcome {
    let mut refusals = Vec::new();
    if let Some(facts) = facts {
        refusals.extend(check_identity(facts));
    }
    if request.erase_all {
        refusals.push(Refusal::new(
            Rule::EraseRequested,
            "`erase_flash` / `--erase-all` erases cardid and is never run",
        ));
    }
    for &(offset, size) in &request.erase_regions {
        let (s, e) = round_to_sectors(u64::from(offset), u64::from(offset) + u64::from(size));
        let what = if touches_cardid(s, e) {
            "covers cardid"
        } else {
            "is not an erase the planner runs (only `--erase-nvs` is)"
        };
        refusals.push(Refusal::new(
            Rule::EraseRequested,
            format!("an erase of [{s:#x}, {e:#x}) {what}"),
        ));
    }
    let (writes, table) = match request.image {
        ImageSource::Merged(image) => split_merged(image, &mut refusals),
        ImageSource::Segments(segments) => check_segments(segments, facts, &mut refusals),
    };
    refusals.extend(check_overlaps(&writes));
    if let Some(facts) = facts {
        refusals.extend(check_device_data(
            &writes,
            table.as_ref(),
            &facts.partitions,
        ));
    }
    let mut erase_nvs = None;
    if request.erase_nvs {
        if request.origin != Origin::HumanCli {
            refusals.push(Refusal::new(
                Rule::EraseNvsNotHuman,
                "`--erase-nvs` is typed by a person on the CLI; tools never erase NVS",
            ));
        }
        // `--erase-nvs` erases the device's own `nvs` entry, never moves it. Offline, the image's
        // entry stands in, and the plan is made again with the device's facts at Identify.
        let nvs = match facts {
            Some(facts) => facts
                .partitions
                .iter()
                .find(|p| p.name == "nvs" && p.ptype == ptype::DATA && p.subtype == subtype::NVS)
                .cloned(),
            None => table
                .as_ref()
                .and_then(|t| t.find("nvs"))
                .filter(|p| p.ptype == ptype::DATA && p.subtype == subtype::NVS)
                .cloned(),
        };
        match nvs {
            Some(nvs) => {
                refusals.extend(
                    check_write_range("nvs erase", nvs.offset, u64::from(nvs.size))
                        .into_iter()
                        .map(|r| Refusal::new(Rule::EraseRequested, r.detail)),
                );
                erase_nvs = Some((nvs.offset, nvs.size));
            }
            None => refusals.push(Refusal::new(
                Rule::EraseRequested,
                "`--erase-nvs` needs a data/nvs partition named `nvs` (on the device when it is known)",
            )),
        }
    }
    PlanOutcome::finish(Plan::bind(writes, erase_nvs), refusals)
}

/// Refuses writes whose sector-rounded ranges overlap: the last one would win, which is not what
/// the rules judged.
fn check_overlaps(writes: &[PlannedWrite]) -> Vec<Refusal> {
    let mut out = Vec::new();
    for (i, a) in writes.iter().enumerate() {
        for b in &writes[i + 1..] {
            let ((as_, ae), (bs, be)) = (a.sectors(), b.sectors());
            if as_ < be && bs < ae {
                out.push(Refusal::new(
                    Rule::SegmentOverlap,
                    format!(
                        "`{}` at {:#x} and `{}` at {:#x} overlap once rounded to sectors",
                        a.name, a.offset, b.name, b.offset
                    ),
                ));
            }
        }
    }
    out
}

/// The device-table rules: no write touches a device data partition, and the image's data
/// partitions equal the device's, so the new table cannot re-purpose `nvs`, `phy_init` or `cardid`.
fn check_device_data(
    writes: &[PlannedWrite],
    image_table: Option<&PartitionTable>,
    device: &[Partition],
) -> Vec<Refusal> {
    let mut out = Vec::new();
    // Custom types (0x40-0xFE) are protected too: a vendor may keep keys or calibration there.
    let device_data: Vec<&Partition> = device.iter().filter(|p| p.ptype != ptype::APP).collect();
    for w in writes {
        let (s, e) = w.sectors();
        for p in &device_data {
            if s < p.end() && u64::from(p.offset) < e {
                out.push(Refusal::new(
                    Rule::WritesDeviceData,
                    format!(
                        "`{}` rounds out to [{s:#x}, {e:#x}), which touches the device's non-app partition `{}`",
                        w.name, p.name
                    ),
                ));
            }
        }
    }
    let Some(image_table) = image_table else {
        return out;
    };
    let key = |p: &Partition| (p.ptype, p.subtype, p.offset, p.size);
    let image_data: Vec<&Partition> = image_table
        .entries
        .iter()
        .filter(|p| p.ptype != ptype::APP)
        .collect();
    for d in &device_data {
        if !image_data
            .iter()
            .any(|i| i.name == d.name && key(i) == key(d))
        {
            out.push(Refusal::new(
                Rule::DataLayoutMismatch,
                format!(
                    "the image table does not keep the device's non-app partition `{}` at {:#x} size {:#x}",
                    d.name, d.offset, d.size
                ),
            ));
        }
    }
    for i in &image_data {
        if !device_data
            .iter()
            .any(|d| d.name == i.name && key(i) == key(d))
        {
            out.push(Refusal::new(
                Rule::DataLayoutMismatch,
                format!(
                    "the image table adds or changes non-app partition `{}`, which the device does not have",
                    i.name
                ),
            ));
        }
    }
    out
}

/// `flash_device --dry-run`: the plan, the identity rules against `facts`, and the backup rules.
/// No port is opened.
pub fn dry_run(
    request: &PlanRequest<'_>,
    facts: &DeviceFacts,
    backup: Option<&BackupEvidence>,
) -> PlanOutcome {
    let outcome = plan_flash(request, Some(facts));
    let mut refusals = outcome.refusals;
    refusals.extend(check_backup(backup));
    PlanOutcome::finish(outcome.plan, refusals)
}

fn split_merged(
    image: &[u8],
    out: &mut Vec<Refusal>,
) -> (Vec<PlannedWrite>, Option<PartitionTable>) {
    if image.len() as u64 > u64::from(FLASH_SIZE) {
        out.push(Refusal::new(
            Rule::BeyondFlash,
            format!(
                "the merged image is {:#x} bytes, more than {FLASH_SIZE:#x}",
                image.len()
            ),
        ));
    }
    let table = match PartitionTable::from_flash(image) {
        Ok(table) => table,
        Err(e) => {
            out.push(Refusal::new(
                Rule::VerifyFirmware,
                format!("no readable partition table at 0x8000: {e}"),
            ));
            return (Vec::new(), None);
        }
    };
    verify_layout(&table, out);
    out.extend(check_image_cardid(&table.entries));
    let mut writes = Vec::new();
    // Everything outside the covered ranges must be 0xFF, or the plan would silently drop it.
    let mut covered: Vec<(u64, u64, Option<&Partition>)> = vec![
        (
            u64::from(BOOTLOADER_OFFSET),
            u64::from(BOOTLOADER_END),
            None,
        ),
        (u64::from(TABLE_OFFSET), u64::from(TABLE_END), None),
    ];
    let boot = trim_ff(clip(image, BOOTLOADER_OFFSET, u64::from(BOOTLOADER_END)));
    if boot.is_empty() {
        out.push(Refusal::new(
            Rule::VerifyFirmware,
            "the merged image has no bootloader at 0x0",
        ));
    } else {
        writes.push(PlannedWrite::new("bootloader", BOOTLOADER_OFFSET, boot));
    }
    let pt = trim_ff(clip(image, TABLE_OFFSET, u64::from(TABLE_END)));
    writes.push(PlannedWrite::new("partition-table", TABLE_OFFSET, pt));
    for p in table.entries.iter().filter(|p| p.ptype == ptype::APP) {
        covered.push((u64::from(p.offset), p.end(), Some(p)));
        let data = trim_ff(clip(image, p.offset, p.end()));
        if data.is_empty() {
            continue;
        }
        if data.first() != Some(&0xE9) {
            out.push(Refusal::new(
                Rule::VerifyFirmware,
                format!(
                    "app partition `{}` does not start with an image magic 0xE9",
                    p.name
                ),
            ));
        }
        if data.len() as u64 > u64::from(APP_MAX) {
            out.push(Refusal::new(
                Rule::AppTooLarge,
                format!(
                    "app `{}` is {:#x} bytes, above {APP_MAX:#x}",
                    p.name,
                    data.len()
                ),
            ));
        }
        writes.push(PlannedWrite::new(&p.name, p.offset, data));
    }
    covered.sort_by_key(|c| c.0);
    check_dropped(image, &covered, &table, out);
    for w in &writes {
        out.extend(check_write_range(&w.name, w.offset, w.data.len() as u64));
    }
    (writes, Some(table))
}

/// Refuses non-0xFF bytes outside the covered ranges, naming the data partition they are in.
fn check_dropped(
    image: &[u8],
    covered: &[(u64, u64, Option<&Partition>)],
    table: &PartitionTable,
    out: &mut Vec<Refusal>,
) {
    let mut cursor = 0u64;
    let len = image.len() as u64;
    let mut gaps = Vec::new();
    for &(s, e, _) in covered {
        if s > cursor {
            gaps.push((cursor, s.min(len)));
        }
        cursor = cursor.max(e);
    }
    if cursor < len {
        gaps.push((cursor, len));
    }
    for (s, e) in gaps {
        if s >= e {
            continue;
        }
        let bytes = &image[s as usize..e as usize];
        let Some(first) = bytes.iter().position(|&b| b != 0xFF) else {
            continue;
        };
        let at = s + first as u64;
        let place = table
            .entries
            .iter()
            .find(|p| u64::from(p.offset) <= at && at < p.end())
            .map_or_else(
                || "outside every partition".to_owned(),
                |p| format!("in data partition `{}`", p.name),
            );
        let cardid = if touches_cardid(at, at + 1) {
            " (the cardid window, which is never written)"
        } else {
            ""
        };
        out.push(Refusal::new(
            Rule::DataDropped,
            format!("non-0xFF bytes at {at:#x} {place}{cardid}; the plan would not write them"),
        ));
    }
    if len > u64::from(CARDID_OFFSET) {
        let window = clip(image, CARDID_OFFSET, u64::from(CARDID_END));
        if window.iter().any(|&b| b != 0xFF) {
            out.push(Refusal::new(
                Rule::VerifyFirmware,
                "the merged image reaches cardid and its window is not all 0xFF",
            ));
        }
    }
}

/// Judges a `flash_args`-style segment list: only the bootloader, the partition table and app
/// partitions of the table (the segment table if present, else the device's).
fn check_segments(
    segments: &[InputSegment<'_>],
    facts: Option<&DeviceFacts>,
    out: &mut Vec<Refusal>,
) -> (Vec<PlannedWrite>, Option<PartitionTable>) {
    let tables = segments.iter().filter(|s| s.offset == TABLE_OFFSET).count();
    if tables != 1 {
        out.push(Refusal::new(
            Rule::SegmentOverlap,
            format!("{tables} partition table segments at 0x8000; exactly one is required"),
        ));
    }
    let boots = segments
        .iter()
        .filter(|s| s.offset == BOOTLOADER_OFFSET)
        .count();
    if boots > 1 {
        out.push(Refusal::new(
            Rule::SegmentOverlap,
            format!("{boots} bootloader segments at 0x0; at most one is allowed"),
        ));
    }
    let table = match segments.iter().find(|s| s.offset == TABLE_OFFSET) {
        Some(seg) => match PartitionTable::parse(seg.data) {
            Ok(table) => {
                verify_layout(&table, out);
                out.extend(check_image_cardid(&table.entries));
                Some(table)
            }
            Err(e) => {
                out.push(Refusal::new(
                    Rule::VerifyFirmware,
                    format!("segment `{}` is not a partition table: {e}", seg.name),
                ));
                None
            }
        },
        None => facts.map(|f| PartitionTable {
            entries: f.partitions.clone(),
            has_md5: true,
        }),
    };
    if table.is_none() && !segments.iter().any(|s| s.offset == TABLE_OFFSET) {
        out.push(Refusal::new(
            Rule::VerifyFirmware,
            "no partition table segment and no device table to place the app by",
        ));
    }
    let mut writes = Vec::new();
    let mut sorted: Vec<&InputSegment<'_>> = segments.iter().collect();
    sorted.sort_by_key(|s| s.offset);
    for seg in sorted {
        let len = seg.data.len() as u64;
        let end = u64::from(seg.offset) + len;
        let app = table.as_ref().and_then(|t| {
            t.entries
                .iter()
                .find(|p| p.ptype == ptype::APP && p.offset == seg.offset)
        });
        let allowed = match seg.offset {
            BOOTLOADER_OFFSET => end <= u64::from(BOOTLOADER_END),
            TABLE_OFFSET => len <= u64::from(TABLE_LEN),
            _ => app.is_some(),
        };
        if !allowed {
            out.push(Refusal::new(
                Rule::SegmentNotAllowed,
                format!(
                    "segment `{}` at {:#x} ({len:#x} bytes) is not the bootloader, the partition table or an app partition",
                    seg.name, seg.offset
                ),
            ));
        }
        if let Some(app) = app {
            if len > u64::from(APP_MAX) || len > u64::from(app.size) {
                out.push(Refusal::new(
                    Rule::AppTooLarge,
                    format!(
                        "app `{}` is {len:#x} bytes, above {:#x}",
                        seg.name,
                        APP_MAX.min(app.size)
                    ),
                ));
            }
            if seg.data.first() != Some(&0xE9) {
                out.push(Refusal::new(
                    Rule::VerifyFirmware,
                    format!("app `{}` does not start with an image magic 0xE9", seg.name),
                ));
            }
        }
        out.extend(check_write_range(seg.name, seg.offset, len));
        writes.push(PlannedWrite::new(seg.name, seg.offset, seg.data));
    }
    (writes, table)
}

/// Encodes a partition table as `PartitionTable::parse` reads it, with the MD5 row
/// `verify_firmware.py` requires, padded with 0xFF to 0xC00 bytes.
pub fn encode_partition_table(entries: &[Partition]) -> Vec<u8> {
    let mut t = Vec::with_capacity(TABLE_LEN as usize);
    for p in entries {
        t.extend_from_slice(&[0xAA, 0x50, p.ptype, p.subtype]);
        t.extend_from_slice(&p.offset.to_le_bytes());
        t.extend_from_slice(&p.size.to_le_bytes());
        let mut name = [0u8; 16];
        let n = p.name.len().min(16);
        name[..n].copy_from_slice(&p.name.as_bytes()[..n]);
        t.extend_from_slice(&name);
        t.extend_from_slice(&p.flags.to_le_bytes());
    }
    let digest = md5::digest(&t);
    t.extend_from_slice(&[0xEB, 0xEB]);
    t.extend_from_slice(&[0xFF; 14]);
    t.extend_from_slice(&digest);
    t.resize(TABLE_LEN as usize, 0xFF);
    t
}
