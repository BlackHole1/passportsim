//! One clock: virtual time and the CPU cycle counter both derive from retired instructions plus
//! stalls and idle time credited since the last rebase,
//! `now(insns) = base_ps + (insns - base_insns) * ps_per_insn`. The cycle counter counts executed
//! time only (never WFI or light sleep) and keeps its fraction exactly, so rebases and stalls
//! accumulate no rounding error. All arithmetic saturates, so debug and release builds agree.

use serde::{Deserialize, Serialize};

use crate::time::{PS_PER_S, VTime};

pub const DEFAULT_CPU_HZ: u32 = 160_000_000;
pub const DEFAULT_CPI_MILLI: u32 = 1_000;

/// One SYSTIMER tick: 16 per µs, independent of the CPU frequency
/// (IDF `esp_hw_support/port/esp32c3/systimer.c`).
pub const SYSTIMER_TICK_PS: u64 = 62_500;
/// SYSTIMER counters are 52 bits wide: HI 20 bits, LO 32 bits.
pub const SYSTIMER_COUNTER_MASK: u64 = (1 << 52) - 1;

/// Picoseconds per CPU cycle at `cpu_hz` (0 counts as 1 Hz), floor. Exact for every C3 PLL and
/// XTAL frequency; RC_FAST (`pemu_soc_c3::periph::timg::RC_FAST_HZ`) floors.
/// UNVERIFIED: the rounding of a period that is not a whole number of picoseconds.
pub const fn ps_per_cycle(cpu_hz: u32) -> u64 {
    let hz = if cpu_hz == 0 { 1 } else { cpu_hz as u64 };
    PS_PER_S / hz
}

const fn ps_per_insn_for(cpu_hz: u32, cpi_milli: u32) -> u64 {
    let p = ps_per_cycle(cpu_hz) as u128 * cpi_milli as u128 / 1_000;
    if p == 0 {
        1
    } else if p > u64::MAX as u128 {
        u64::MAX
    } else {
        p as u64
    }
}

/// SYSTIMER counter at `now` for a counter that held `loaded` at `epoch`, wrapped to 52 bits;
/// `now` before `epoch` reads `loaded`.
pub fn systimer_count(now: VTime, epoch: VTime, loaded: u64) -> u64 {
    let ticks = now.0.saturating_sub(epoch.0) / SYSTIMER_TICK_PS;
    loaded.wrapping_add(ticks) & SYSTIMER_COUNTER_MASK
}

/// Earliest time a counter started at `epoch` has advanced `ticks` ticks; saturates.
pub fn systimer_deadline(epoch: VTime, ticks: u64) -> VTime {
    VTime(
        epoch
            .0
            .saturating_add(ticks.saturating_mul(SYSTIMER_TICK_PS)),
    )
}

const fn sat_u64(v: u128) -> u64 {
    if v > u64::MAX as u128 {
        u64::MAX
    } else {
        v as u64
    }
}

/// Maps retired instructions to virtual time and to the CPU cycle counter (CSR 0x7E2 / 0x802).
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct Clock {
    base_ps: u64,
    base_insns: u64,
    base_cc: u64,
    ps_per_insn: u64,
    cpi_milli: u32,
    stall_ps: u64,
    idle_ps: u64,
    /// Never 0.
    cpu_hz: u32,
    /// mpcmr COUNT_EN and mpcer CYCLE both set (CSR 0x7E1 / 0x7E0).
    counting: bool,
    /// Fraction of a cycle not yet in `base_cc`, in 10^-12 cycle; always below 10^12.
    cc_frac: u64,
}

impl Default for Clock {
    fn default() -> Self {
        Clock::new(DEFAULT_CPU_HZ, DEFAULT_CPI_MILLI)
    }
}

impl Clock {
    /// Clock at time 0 at `cpu_hz` (0 counts as 1 Hz), the cycle counter 0 and disabled: mpcer
    /// resets to 0 although mpcmr resets to 0b11; the ROM enables both before its first 0x802 read.
    /// UNVERIFIED: the reset values of the two enable bits.
    pub fn new(cpu_hz: u32, cpi_milli: u32) -> Self {
        let cpu_hz = cpu_hz.max(1);
        Clock {
            base_ps: 0,
            base_insns: 0,
            base_cc: 0,
            ps_per_insn: ps_per_insn_for(cpu_hz, cpi_milli),
            cpi_milli,
            stall_ps: 0,
            idle_ps: 0,
            cpu_hz,
            counting: false,
            cc_frac: 0,
        }
    }

    /// Picoseconds of executed instructions since the base point. Factors that fit in 32 bits
    /// skip `saturating_mul`, which on wasm32 calls `__multi3`; this runs on every clock read.
    fn exec_ps(&self, insns: u64) -> u64 {
        let n = insns.saturating_sub(self.base_insns);
        if (n | self.ps_per_insn) >> 32 == 0 {
            n * self.ps_per_insn
        } else {
            n.saturating_mul(self.ps_per_insn)
        }
    }

    /// Cycles since the base, as a count in 10^-12 cycle including `cc_frac`.
    fn cc_total(&self, insns: u64) -> u128 {
        self.cc_frac as u128 + self.exec_ps(insns) as u128 * self.cpu_hz as u128
    }

    /// Moves the base point to `insns` without changing `now` or `cycle_count` at `insns`.
    fn fold(&mut self, insns: u64) {
        let now = self.now(insns);
        if self.counting {
            let total = self.cc_total(insns);
            self.base_cc = self
                .base_cc
                .saturating_add(sat_u64(total / PS_PER_S as u128));
            self.cc_frac = (total % PS_PER_S as u128) as u64;
        }
        self.base_ps = now.0;
        self.base_insns = insns;
    }

    /// Virtual time after `insns` retired instructions; a count below the base point reads as it.
    pub fn now(&self, insns: u64) -> VTime {
        VTime(self.base_ps.saturating_add(self.exec_ps(insns)))
    }

    /// Full-width cycle counter; the CSR model truncates to 32 bits and applies COUNT_SAT.
    pub fn cycle_count(&self, insns: u64) -> u64 {
        if !self.counting {
            return self.base_cc;
        }
        let whole = sat_u64(self.cc_total(insns) / PS_PER_S as u128);
        self.base_cc.saturating_add(whole)
    }

    /// Rebases on a CPU frequency change; `now` and `cycle_count` stay continuous at `insns`.
    pub fn rebase(&mut self, insns: u64, cpu_hz: u32) {
        self.fold(insns);
        self.cpu_hz = cpu_hz.max(1);
        self.ps_per_insn = ps_per_insn_for(self.cpu_hz, self.cpi_milli);
    }

    /// Credits a stall of `ps` to time; no instruction retires. The cycle counter counts it only
    /// when `counts_cycles` (a profile flag, UNVERIFIED whether silicon counts it) and enabled.
    pub fn stall(&mut self, insns: u64, ps: u64, counts_cycles: bool) {
        let _ = insns;
        self.base_ps = self.base_ps.saturating_add(ps);
        self.stall_ps = self.stall_ps.saturating_add(ps);
        if counts_cycles && self.counting {
            let total = self.cc_frac as u128 + ps as u128 * self.cpu_hz as u128;
            self.base_cc = self
                .base_cc
                .saturating_add(sat_u64(total / PS_PER_S as u128));
            self.cc_frac = (total % PS_PER_S as u128) as u64;
        }
    }

    /// Jumps time forward to `t` as idle time, with no instructions or cycles.
    pub fn idle_until(&mut self, insns: u64, t: VTime) {
        let now = self.now(insns);
        if t > now {
            let d = t.0 - now.0;
            self.base_ps = self.base_ps.saturating_add(d);
            self.idle_ps = self.idle_ps.saturating_add(d);
        }
    }

    /// The smallest `n >= 1` with `now(insns + n) >= t`.
    pub fn insns_until(&self, insns: u64, t: VTime) -> u64 {
        let now = self.now(insns);
        if t <= now {
            return 1;
        }
        (t.0 - now.0).div_ceil(self.ps_per_insn).max(1)
    }

    /// Loads the cycle counter (a write to CSR 0x7E2 or 0x802).
    pub fn set_cycle_count(&mut self, insns: u64, value: u64) {
        self.fold(insns);
        self.base_cc = value;
        self.cc_frac = 0;
    }

    /// Enables or freezes the cycle counter; the value is continuous at `insns`.
    pub fn set_counting(&mut self, insns: u64, enabled: bool) {
        if enabled != self.counting {
            self.fold(insns);
            self.counting = enabled;
        }
    }

    pub fn counting(&self) -> bool {
        self.counting
    }

    pub fn cpu_hz(&self) -> u32 {
        self.cpu_hz
    }

    pub fn cpi_milli(&self) -> u32 {
        self.cpi_milli
    }

    pub fn ps_per_insn(&self) -> u64 {
        self.ps_per_insn
    }

    pub fn stall_ps(&self) -> u64 {
        self.stall_ps
    }

    /// Total WFI and light-sleep time since power-on (`RunOutcome::idle_ps`).
    pub fn idle_ps(&self) -> u64 {
        self.idle_ps
    }
}

pub const TIMING_PROFILES_TOML: &str = include_str!("../../../specs/timing-profiles.toml");

/// The cache model that charges flash-cache fills (`pemu_soc_c3::cold`).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum CacheVariant {
    /// First data load per 4 KB page after an MMU write or a cache flush.
    ColdPage,
    /// A miss in the 16 KB, 8-way, 32-byte-line cache over fetches and data loads, LRU.
    Lru16k,
    /// The same cache, FIFO in each set as silicon does.
    Fifo16k,
}

impl CacheVariant {
    pub const fn as_str(self) -> &'static str {
        match self {
            CacheVariant::ColdPage => "cold_page",
            CacheVariant::Lru16k => "lru16k",
            CacheVariant::Fifo16k => "fifo16k",
        }
    }
}

/// Timing profile from `specs/timing-profiles.toml`: one field per `[[constant]]` row, with the
/// row's name ([`TimingProfile::parse_table`] refuses any mismatch). Durations are picoseconds.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TimingProfile {
    /// CPU cycles per instruction x 1000.
    pub cpi_milli: u32,
    pub cache_model: CacheVariant,
    /// One fill: a 32-byte line's flash transfer under `lru16k` (a miss waits only up to its own
    /// word), a 4 KB page's stall under `cold_page`; 0 charges nothing.
    pub cache_fill_ps: u64,
    pub cache_stall_counts_cycles: bool,
    /// `SHA_BUSY` time per 64-byte block.
    pub sha_block_ps: u64,
    pub aes_block_ps: u64,
    pub rsa_op_ps: u64,
    // Flash `WIP` times: page program, 4 KB sector erase, block erase, chip erase.
    pub flash_pp_ps: u64,
    pub flash_se_ps: u64,
    pub flash_be_ps: u64,
    pub flash_ce_ps: u64,
    /// Whether an SPI1 flash command keeps its trigger bit set for its bus time.
    pub spi1_clocked: bool,
    /// Whether SPI2 `trans_done` waits for the transaction's bits at the programmed clock.
    pub spi2_clocked: bool,
    pub spi2_overhead_ps: u64,
    /// Whether an I2C0 command list completes after its bus time at the programmed SCL period.
    pub i2c_clocked: bool,
    /// How long a committed USB Serial/JTAG IN packet waits for the host poll.
    pub usj_drain_ps: u64,
    /// From a `SYS_` class reset, which drops the USB link, to the host's first SOF.
    pub usj_enum_reset_ps: u64,
    /// From the USB link's return (a deep-sleep wake, a plug) to the host's first SOF.
    pub usj_enum_wake_ps: u64,
    /// Whether UART0 shifts one byte per baud period.
    pub uart_paced: bool,
    pub rtc_slow_hz: u32,
    // Time each BLE HLE `esp_bt_controller_*` handler spends after its log line.
    pub ble_init_ps: u64,
    pub ble_enable_ps: u64,
    /// Added to enable for an image that keeps its PHY calibration in NVS.
    pub ble_enable_nvs_cal_ps: u64,
    pub ble_disable_ps: u64,
    pub ble_deinit_ps: u64,
    // Cycles beyond one an instruction costs.
    /// A taken conditional branch.
    pub taken_branch_cycles: u32,
    /// `jal` and `jalr`, compressed forms included.
    pub jump_cycles: u32,
    /// A 32-bit instruction at `pc = 2 mod 4` reached by a redirect (branch, jump, `mret`).
    pub split_redirect_cycles: u32,
    /// An instruction that reads the destination of the load retired right before it.
    pub load_use_cycles: u32,
    /// `div`, `divu`, `rem`, `remu`, plus one cycle per quotient bit its operands give.
    pub div_base_cycles: u32,
    /// APB cycles of a load from an APB peripheral page beyond
    /// [`TimingProfile::mmio_cpu_cycles`], rounded up to whole CPU cycles.
    pub mmio_load_apb_cycles: u32,
    pub mmio_store_apb_cycles: u32,
    /// From a line's flash transfer start to its first word (`lru16k`); 0 makes every word
    /// arrive with the whole line.
    pub cache_first_word_ps: u64,
    /// CPU cycles before a miss's flash transfer starts (`lru16k`).
    pub cache_miss_cycles: u32,
    pub mulh_cycles: u32,
    /// CPU cycles of every MMIO access besides its APB cycles, the instruction's own included.
    pub mmio_cpu_cycles: u32,
    /// An 8-byte code fetch while a dense run of data accesses holds its SRAM bank (code and data
    /// both in SRAM Block 1).
    pub sram_bank_cycles: u32,
    /// SAR ADC one-shot from the `onetime_start` rising edge to done; 0 completes inside the
    /// starting access.
    pub adc_conversion_ps: u64,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProfileTableError(pub String);

impl core::fmt::Display for ProfileTableError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "specs/timing-profiles.toml: {}", self.0)
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum RowValue {
    Int(u64),
    Text(String),
}

/// Reads a value of the TOML subset the table uses: a decimal integer (with `_` separators) or
/// a double-quoted string without escapes.
fn row_value(text: &str) -> Option<RowValue> {
    let text = text.trim();
    if let Some(body) = text.strip_prefix('"') {
        let end = body.find('"')?;
        return Some(RowValue::Text(body[..end].to_string()));
    }
    let digits: String = text
        .split('#')
        .next()?
        .trim()
        .chars()
        .filter(|c| *c != '_')
        .collect();
    digits.parse().ok().map(RowValue::Int)
}

impl TimingProfile {
    pub const NAMES: &'static [&'static str] = &[
        "cpi_milli",
        "cache_model",
        "cache_fill_ps",
        "cache_stall_counts_cycles",
        "sha_block_ps",
        "aes_block_ps",
        "rsa_op_ps",
        "flash_pp_ps",
        "flash_se_ps",
        "flash_be_ps",
        "flash_ce_ps",
        "spi1_clocked",
        "spi2_clocked",
        "spi2_overhead_ps",
        "i2c_clocked",
        "usj_drain_ps",
        "usj_enum_reset_ps",
        "usj_enum_wake_ps",
        "uart_paced",
        "rtc_slow_hz",
        "ble_init_ps",
        "ble_enable_ps",
        "ble_enable_nvs_cal_ps",
        "ble_disable_ps",
        "ble_deinit_ps",
        "taken_branch_cycles",
        "jump_cycles",
        "split_redirect_cycles",
        "load_use_cycles",
        "div_base_cycles",
        "mmio_load_apb_cycles",
        "mmio_store_apb_cycles",
        "cache_first_word_ps",
        "cache_miss_cycles",
        "mulh_cycles",
        "mmio_cpu_cycles",
        "sram_bank_cycles",
        "adc_conversion_ps",
    ];

    /// Sets numeric or flag row `name` (a flag takes 0 or 1), as `pemu_verify::calibrate` does.
    /// Refuses unknown names, `cache_model` and values the row cannot hold.
    pub fn set_by_name(&mut self, name: &str, value: u64) -> Result<(), ProfileTableError> {
        let flag = |v: u64| match v {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(ProfileTableError(format!("`{name}` is a flag, not {v}"))),
        };
        let small = |v: u64| {
            u32::try_from(v).map_err(|_| ProfileTableError(format!("`{name}` {v} is too large")))
        };
        match name {
            "cpi_milli" => self.cpi_milli = small(value)?,
            "cache_fill_ps" => self.cache_fill_ps = value,
            "cache_stall_counts_cycles" => self.cache_stall_counts_cycles = flag(value)?,
            "sha_block_ps" => self.sha_block_ps = value,
            "aes_block_ps" => self.aes_block_ps = value,
            "rsa_op_ps" => self.rsa_op_ps = value,
            "flash_pp_ps" => self.flash_pp_ps = value,
            "flash_se_ps" => self.flash_se_ps = value,
            "flash_be_ps" => self.flash_be_ps = value,
            "flash_ce_ps" => self.flash_ce_ps = value,
            "spi1_clocked" => self.spi1_clocked = flag(value)?,
            "spi2_clocked" => self.spi2_clocked = flag(value)?,
            "spi2_overhead_ps" => self.spi2_overhead_ps = value,
            "i2c_clocked" => self.i2c_clocked = flag(value)?,
            "usj_drain_ps" => self.usj_drain_ps = value,
            "usj_enum_reset_ps" => self.usj_enum_reset_ps = value,
            "usj_enum_wake_ps" => self.usj_enum_wake_ps = value,
            "uart_paced" => self.uart_paced = flag(value)?,
            "rtc_slow_hz" => self.rtc_slow_hz = small(value)?,
            "ble_init_ps" => self.ble_init_ps = value,
            "ble_enable_ps" => self.ble_enable_ps = value,
            "ble_enable_nvs_cal_ps" => self.ble_enable_nvs_cal_ps = value,
            "ble_disable_ps" => self.ble_disable_ps = value,
            "ble_deinit_ps" => self.ble_deinit_ps = value,
            "taken_branch_cycles" => self.taken_branch_cycles = small(value)?,
            "jump_cycles" => self.jump_cycles = small(value)?,
            "split_redirect_cycles" => self.split_redirect_cycles = small(value)?,
            "load_use_cycles" => self.load_use_cycles = small(value)?,
            "div_base_cycles" => self.div_base_cycles = small(value)?,
            "mmio_load_apb_cycles" => self.mmio_load_apb_cycles = small(value)?,
            "mmio_store_apb_cycles" => self.mmio_store_apb_cycles = small(value)?,
            "cache_first_word_ps" => self.cache_first_word_ps = value,
            "cache_miss_cycles" => self.cache_miss_cycles = small(value)?,
            "mulh_cycles" => self.mulh_cycles = small(value)?,
            "mmio_cpu_cycles" => self.mmio_cpu_cycles = small(value)?,
            "sram_bank_cycles" => self.sram_bank_cycles = small(value)?,
            "adc_conversion_ps" => self.adc_conversion_ps = value,
            other => {
                return Err(ProfileTableError(format!(
                    "`{other}` is not a numeric row of the timing profile"
                )));
            }
        }
        Ok(())
    }

    /// The inverse of [`TimingProfile::set_by_name`]; `None` for `cache_model` and unknown names.
    pub fn get_by_name(&self, name: &str) -> Option<u64> {
        let mut probe = self.clone();
        // A row reads back as the value that, set, leaves the profile unchanged.
        let current = |p: &TimingProfile| -> Option<u64> {
            Some(match name {
                "cpi_milli" => u64::from(p.cpi_milli),
                "cache_fill_ps" => p.cache_fill_ps,
                "cache_stall_counts_cycles" => u64::from(p.cache_stall_counts_cycles),
                "sha_block_ps" => p.sha_block_ps,
                "aes_block_ps" => p.aes_block_ps,
                "rsa_op_ps" => p.rsa_op_ps,
                "flash_pp_ps" => p.flash_pp_ps,
                "flash_se_ps" => p.flash_se_ps,
                "flash_be_ps" => p.flash_be_ps,
                "flash_ce_ps" => p.flash_ce_ps,
                "spi1_clocked" => u64::from(p.spi1_clocked),
                "spi2_clocked" => u64::from(p.spi2_clocked),
                "spi2_overhead_ps" => p.spi2_overhead_ps,
                "i2c_clocked" => u64::from(p.i2c_clocked),
                "usj_drain_ps" => p.usj_drain_ps,
                "usj_enum_reset_ps" => p.usj_enum_reset_ps,
                "usj_enum_wake_ps" => p.usj_enum_wake_ps,
                "uart_paced" => u64::from(p.uart_paced),
                "rtc_slow_hz" => u64::from(p.rtc_slow_hz),
                "ble_init_ps" => p.ble_init_ps,
                "ble_enable_ps" => p.ble_enable_ps,
                "ble_enable_nvs_cal_ps" => p.ble_enable_nvs_cal_ps,
                "ble_disable_ps" => p.ble_disable_ps,
                "ble_deinit_ps" => p.ble_deinit_ps,
                "taken_branch_cycles" => u64::from(p.taken_branch_cycles),
                "jump_cycles" => u64::from(p.jump_cycles),
                "split_redirect_cycles" => u64::from(p.split_redirect_cycles),
                "load_use_cycles" => u64::from(p.load_use_cycles),
                "div_base_cycles" => u64::from(p.div_base_cycles),
                "mmio_load_apb_cycles" => u64::from(p.mmio_load_apb_cycles),
                "mmio_store_apb_cycles" => u64::from(p.mmio_store_apb_cycles),
                "cache_first_word_ps" => p.cache_first_word_ps,
                "cache_miss_cycles" => u64::from(p.cache_miss_cycles),
                "mulh_cycles" => u64::from(p.mulh_cycles),
                "mmio_cpu_cycles" => u64::from(p.mmio_cpu_cycles),
                "sram_bank_cycles" => u64::from(p.sram_bank_cycles),
                "adc_conversion_ps" => p.adc_conversion_ps,
                _ => return None,
            })
        };
        let value = current(self)?;
        debug_assert!(probe.set_by_name(name, value).is_ok() && &probe == self);
        Some(value)
    }

    pub fn fast() -> &'static TimingProfile {
        &Self::table().0
    }

    pub fn device() -> &'static TimingProfile {
        &Self::table().1
    }

    fn table() -> &'static (TimingProfile, TimingProfile) {
        static TABLE: std::sync::OnceLock<(TimingProfile, TimingProfile)> =
            std::sync::OnceLock::new();
        TABLE.get_or_init(|| {
            TimingProfile::parse_table(TIMING_PROFILES_TOML)
                .unwrap_or_else(|err| panic!("the compiled-in table is checked by a test: {err}"))
        })
    }

    /// Parses the `fast` and `device` columns. Only `[[constant]]` rows are read (other tables
    /// belong to `pemu-verify`); unknown, repeated and missing names are refused.
    pub fn parse_table(text: &str) -> Result<(TimingProfile, TimingProfile), ProfileTableError> {
        type Row = (String, Option<RowValue>, Option<RowValue>);
        let mut rows: Vec<Row> = Vec::new();
        let mut in_constant = false;
        for (n, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if line.starts_with('[') {
                in_constant = line == "[[constant]]";
                if in_constant {
                    rows.push((String::new(), None, None));
                }
                continue;
            }
            if !in_constant {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                return Err(ProfileTableError(format!(
                    "line {}: not `key = value`",
                    n + 1
                )));
            };
            let row = rows
                .last_mut()
                .expect("a [[constant]] header opened the row");
            let parsed = row_value(value)
                .ok_or_else(|| ProfileTableError(format!("line {}: unreadable value", n + 1)));
            match key.trim() {
                "name" => match parsed? {
                    RowValue::Text(name) => row.0 = name,
                    RowValue::Int(_) => {
                        return Err(ProfileTableError(format!("line {}: name", n + 1)));
                    }
                },
                "fast" => row.1 = Some(parsed?),
                "device" => row.2 = Some(parsed?),
                _ => {}
            }
        }
        let column = |pick: fn(&Row) -> &Option<RowValue>,
                      label: &str|
         -> Result<TimingProfile, ProfileTableError> {
            let get = |name: &str| -> Result<&RowValue, ProfileTableError> {
                let mut found = rows.iter().filter(|r| r.0 == name);
                let row = found
                    .next()
                    .ok_or_else(|| ProfileTableError(format!("no `{name}` row")))?;
                if found.next().is_some() {
                    return Err(ProfileTableError(format!("`{name}` twice")));
                }
                pick(row)
                    .as_ref()
                    .ok_or_else(|| ProfileTableError(format!("`{name}` has no `{label}` value")))
            };
            let int = |name: &str| -> Result<u64, ProfileTableError> {
                match get(name)? {
                    RowValue::Int(v) => Ok(*v),
                    RowValue::Text(_) => Err(ProfileTableError(format!(
                        "`{name}` `{label}` is not an integer"
                    ))),
                }
            };
            let flag = |name: &str| -> Result<bool, ProfileTableError> {
                match int(name)? {
                    0 => Ok(false),
                    1 => Ok(true),
                    _ => Err(ProfileTableError(format!(
                        "`{name}` `{label}` is not 0 or 1"
                    ))),
                }
            };
            let small = |name: &str| -> Result<u32, ProfileTableError> {
                u32::try_from(int(name)?)
                    .map_err(|_| ProfileTableError(format!("`{name}` `{label}` is too large")))
            };
            let cache_model = match get("cache_model")? {
                RowValue::Text(t) if t == "cold_page" => CacheVariant::ColdPage,
                RowValue::Text(t) if t == "lru16k" => CacheVariant::Lru16k,
                RowValue::Text(t) if t == "fifo16k" => CacheVariant::Fifo16k,
                _ => {
                    return Err(ProfileTableError(format!(
                        "`cache_model` `{label}` is not cold_page, lru16k or fifo16k"
                    )));
                }
            };
            Ok(TimingProfile {
                cpi_milli: small("cpi_milli")?,
                cache_model,
                cache_fill_ps: int("cache_fill_ps")?,
                cache_stall_counts_cycles: flag("cache_stall_counts_cycles")?,
                sha_block_ps: int("sha_block_ps")?,
                aes_block_ps: int("aes_block_ps")?,
                rsa_op_ps: int("rsa_op_ps")?,
                flash_pp_ps: int("flash_pp_ps")?,
                flash_se_ps: int("flash_se_ps")?,
                flash_be_ps: int("flash_be_ps")?,
                flash_ce_ps: int("flash_ce_ps")?,
                spi1_clocked: flag("spi1_clocked")?,
                spi2_clocked: flag("spi2_clocked")?,
                spi2_overhead_ps: int("spi2_overhead_ps")?,
                i2c_clocked: flag("i2c_clocked")?,
                usj_drain_ps: int("usj_drain_ps")?,
                usj_enum_reset_ps: int("usj_enum_reset_ps")?,
                usj_enum_wake_ps: int("usj_enum_wake_ps")?,
                uart_paced: flag("uart_paced")?,
                rtc_slow_hz: small("rtc_slow_hz")?,
                ble_init_ps: int("ble_init_ps")?,
                ble_enable_ps: int("ble_enable_ps")?,
                ble_enable_nvs_cal_ps: int("ble_enable_nvs_cal_ps")?,
                ble_disable_ps: int("ble_disable_ps")?,
                ble_deinit_ps: int("ble_deinit_ps")?,
                taken_branch_cycles: small("taken_branch_cycles")?,
                jump_cycles: small("jump_cycles")?,
                split_redirect_cycles: small("split_redirect_cycles")?,
                load_use_cycles: small("load_use_cycles")?,
                div_base_cycles: small("div_base_cycles")?,
                mmio_load_apb_cycles: small("mmio_load_apb_cycles")?,
                mmio_store_apb_cycles: small("mmio_store_apb_cycles")?,
                cache_first_word_ps: int("cache_first_word_ps")?,
                cache_miss_cycles: small("cache_miss_cycles")?,
                mulh_cycles: small("mulh_cycles")?,
                mmio_cpu_cycles: small("mmio_cpu_cycles")?,
                sram_bank_cycles: small("sram_bank_cycles")?,
                adc_conversion_ps: int("adc_conversion_ps")?,
            })
        };
        for row in &rows {
            if !Self::NAMES.contains(&row.0.as_str()) {
                return Err(ProfileTableError(format!(
                    "`{}` is not a TimingProfile field",
                    row.0
                )));
            }
        }
        if rows.len() != Self::NAMES.len() {
            return Err(ProfileTableError(format!(
                "{} rows for {} fields",
                rows.len(),
                Self::NAMES.len()
            )));
        }
        if let Some(bad) = rows.iter().find(|r| r.2.is_none() || r.1.is_none()) {
            return Err(ProfileTableError(format!("`{}` needs both columns", bad.0)));
        }
        Ok((column(|r| &r.1, "fast")?, column(|r| &r.2, "device")?))
    }

    /// Canonical bytes for the identity hash: every field, little-endian at its own width.
    pub fn identity_bytes(&self) -> Vec<u8> {
        let TimingProfile {
            cpi_milli,
            cache_model,
            cache_fill_ps,
            cache_stall_counts_cycles,
            sha_block_ps,
            aes_block_ps,
            rsa_op_ps,
            flash_pp_ps,
            flash_se_ps,
            flash_be_ps,
            flash_ce_ps,
            spi1_clocked,
            spi2_clocked,
            spi2_overhead_ps,
            i2c_clocked,
            usj_drain_ps,
            usj_enum_reset_ps,
            usj_enum_wake_ps,
            uart_paced,
            rtc_slow_hz,
            ble_init_ps,
            ble_enable_ps,
            ble_enable_nvs_cal_ps,
            ble_disable_ps,
            ble_deinit_ps,
            taken_branch_cycles,
            jump_cycles,
            split_redirect_cycles,
            load_use_cycles,
            div_base_cycles,
            mmio_load_apb_cycles,
            mmio_store_apb_cycles,
            cache_first_word_ps,
            cache_miss_cycles,
            mulh_cycles,
            mmio_cpu_cycles,
            sram_bank_cycles,
            adc_conversion_ps,
        } = self;
        let mut out = b"pemu.timing-profile.v1".to_vec();
        out.extend_from_slice(&cpi_milli.to_le_bytes());
        out.push(match cache_model {
            CacheVariant::ColdPage => 0,
            CacheVariant::Lru16k => 1,
            CacheVariant::Fifo16k => 2,
        });
        for v in [
            *cache_fill_ps,
            *sha_block_ps,
            *aes_block_ps,
            *rsa_op_ps,
            *flash_pp_ps,
            *flash_se_ps,
            *flash_be_ps,
            *flash_ce_ps,
            *spi2_overhead_ps,
            *usj_drain_ps,
            *usj_enum_reset_ps,
            *usj_enum_wake_ps,
            *ble_init_ps,
            *ble_enable_ps,
            *ble_enable_nvs_cal_ps,
            *ble_disable_ps,
            *ble_deinit_ps,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&[
            u8::from(*cache_stall_counts_cycles),
            u8::from(*spi1_clocked),
            u8::from(*spi2_clocked),
            u8::from(*i2c_clocked),
            u8::from(*uart_paced),
        ]);
        out.extend_from_slice(&rtc_slow_hz.to_le_bytes());
        // Rows added later are appended below, never inserted, so older encodings stay prefixes.
        for v in [
            *taken_branch_cycles,
            *jump_cycles,
            *split_redirect_cycles,
            *load_use_cycles,
            *div_base_cycles,
            *mmio_load_apb_cycles,
            *mmio_store_apb_cycles,
        ] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&cache_first_word_ps.to_le_bytes());
        for v in [*cache_miss_cycles, *mulh_cycles, *mmio_cpu_cycles] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&sram_bank_cycles.to_le_bytes());
        out.extend_from_slice(&adc_conversion_ps.to_le_bytes());
        out
    }
}

impl Default for TimingProfile {
    fn default() -> TimingProfile {
        TimingProfile::fast().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MHZ: u32 = 1_000_000;

    fn counting_clock(cpu_hz: u32, cpi_milli: u32) -> Clock {
        let mut c = Clock::new(cpu_hz, cpi_milli);
        c.set_counting(0, true);
        c
    }

    #[test]
    fn default_is_160_mhz_cpi_1_with_the_counter_off() {
        let mut c = Clock::default();
        assert_eq!(c.ps_per_insn(), 6_250);
        assert_eq!(c.cpi_milli(), 1_000);
        assert_eq!(c.cpu_hz(), 160 * MHZ);
        assert_eq!(c.now(0), VTime(0));
        assert_eq!(c.now(1), VTime(6_250));
        assert_eq!(c, Clock::new(DEFAULT_CPU_HZ, DEFAULT_CPI_MILLI));
        assert!(!c.counting());
        assert_eq!(c.cycle_count(160_000_000), 0);
        c.set_counting(0, true);
        assert_eq!(c.cycle_count(1), 1);
        assert_eq!(c.now(160_000_000), VTime(PS_PER_S));
        assert_eq!(c.cycle_count(160_000_000), 160_000_000);
    }

    #[test]
    fn rebase_is_continuous_and_changes_the_rate() {
        let mut c = counting_clock(40 * MHZ, 1_000);
        assert_eq!(c.ps_per_insn(), 25_000);
        let (t0, cc0) = (c.now(1_000), c.cycle_count(1_000));
        c.rebase(1_000, 80 * MHZ);
        assert_eq!((c.now(1_000), c.cycle_count(1_000)), (t0, cc0));
        assert_eq!(c.ps_per_insn(), 12_500);
        assert_eq!(c.now(1_010).0 - t0.0, 10 * 12_500);
        c.rebase(1_010, 160 * MHZ);
        assert_eq!(c.now(1_020).0 - t0.0, 10 * 12_500 + 10 * 6_250);
        assert_eq!(c.cycle_count(1_020), cc0 + 20);
        c.rebase(1_020, 0);
        assert_eq!(c.cpu_hz(), 1);
        assert_eq!(c.ps_per_insn(), PS_PER_S);
    }

    #[test]
    fn cpi_scales_time_per_instruction() {
        let c = counting_clock(160 * MHZ, 2_500);
        assert_eq!(c.ps_per_insn(), 15_625);
        assert_eq!(c.cycle_count(2), 5);
        assert_eq!(Clock::new(160 * MHZ, 0).ps_per_insn(), 1);
    }

    #[test]
    fn stall_moves_time_and_counts_cycles_only_when_flagged() {
        let mut c = counting_clock(DEFAULT_CPU_HZ, DEFAULT_CPI_MILLI);
        c.stall(10, 205_000_000, false);
        assert_eq!(c.now(10), VTime(10 * 6_250 + 205_000_000));
        assert_eq!(c.cycle_count(10), 10);
        c.stall(10, 1_000_000, true); // 1 µs at 160 MHz is 160 cycles
        assert_eq!(c.cycle_count(10), 170);
        assert_eq!(c.stall_ps(), 206_000_000);
        // 6_250 stalls of 1 ps make one cycle.
        let mut d = counting_clock(DEFAULT_CPU_HZ, DEFAULT_CPI_MILLI);
        for _ in 0..6_249 {
            d.stall(0, 1, true);
        }
        assert_eq!(d.cycle_count(0), 0);
        d.stall(0, 1, true);
        assert_eq!(d.cycle_count(0), 1);
        d.set_counting(0, false);
        d.stall(0, 1_000_000, true);
        assert_eq!(d.cycle_count(0), 1);
    }

    #[test]
    fn idle_moves_time_but_not_instructions_or_cycles() {
        let mut c = counting_clock(DEFAULT_CPU_HZ, DEFAULT_CPI_MILLI);
        let t = VTime::from_ms(10);
        c.idle_until(100, t);
        assert_eq!(c.now(100), t);
        assert_eq!(c.cycle_count(100), 100);
        assert_eq!(c.idle_ps(), 10_000_000_000 - 100 * 6_250);
        c.idle_until(100, VTime(5));
        c.idle_until(100, t);
        assert_eq!(c.now(100), t);
        assert_eq!(c.idle_ps(), 10_000_000_000 - 100 * 6_250);
        assert_eq!(c.now(101), VTime(t.0 + 6_250));
    }

    #[test]
    fn insns_until_is_ceil_and_at_least_one() {
        let c = Clock::default();
        assert_eq!(c.insns_until(0, VTime(0)), 1);
        assert_eq!(c.insns_until(4, VTime(1)), 1);
        assert_eq!(c.insns_until(0, VTime(6_250)), 1);
        assert_eq!(c.insns_until(0, VTime(6_251)), 2);
        assert_eq!(c.insns_until(0, VTime(12_500)), 2);
        assert_eq!(c.insns_until(2, VTime(12_501)), 1);
        assert_eq!(c.insns_until(0, VTime(u64::MAX)), u64::MAX.div_ceil(6_250));
    }

    #[test]
    fn counter_enable_and_load() {
        let mut c = counting_clock(DEFAULT_CPU_HZ, DEFAULT_CPI_MILLI);
        c.set_counting(50, false);
        assert_eq!(c.cycle_count(1_000), 50);
        assert_eq!(c.now(1_000), VTime(1_000 * 6_250));
        c.set_counting(1_000, true);
        assert_eq!(c.cycle_count(1_010), 60);
        c.set_cycle_count(1_010, 0xFFFF_FFF0);
        assert_eq!(c.cycle_count(1_026), 0xFFFF_FFF0 + 16);
        assert_eq!(c.now(1_026), VTime(1_026 * 6_250));
    }

    #[test]
    fn huge_values_saturate_without_panicking() {
        let mut c = counting_clock(DEFAULT_CPU_HZ, DEFAULT_CPI_MILLI);
        assert_eq!(c.now(u64::MAX), VTime(u64::MAX));
        c.stall(0, u64::MAX, true);
        c.idle_until(0, VTime(u64::MAX));
        assert_eq!(c.now(0), VTime(u64::MAX));
        assert!(c.cycle_count(u64::MAX) > 0);
    }

    #[test]
    fn systimer_formula_16_ticks_per_us() {
        let e = VTime(0);
        // IDF systimer.c: us_to_ticks is us * 16, ticks_to_us is ticks / 16.
        for us in [0u64, 1, 999, 1_000, 123_456_789] {
            assert_eq!(systimer_count(VTime::from_us(us), e, 0), us * 16);
            assert_eq!(systimer_count(VTime::from_us(us), e, 0) / 16, us);
        }
        assert_eq!(systimer_count(VTime(62_499), e, 0), 0);
        assert_eq!(systimer_count(VTime(62_500), e, 0), 1);
        assert_eq!(systimer_count(VTime::from_ms(1), e, 0), 16_000);
        let ep = VTime::from_ms(7);
        assert_eq!(systimer_count(VTime(ep.0 + 62_500 * 3), ep, 40), 43);
        assert_eq!(systimer_count(VTime(0), ep, 40), 40);
        assert_eq!(systimer_count(VTime(62_500), e, SYSTIMER_COUNTER_MASK), 0);
        for k in [1u64, 16, 16_000, 1 << 40] {
            let d = systimer_deadline(ep, k);
            assert_eq!(systimer_count(d, ep, 0), k);
            assert_eq!(systimer_count(VTime(d.0 - 1), ep, 0), k - 1);
        }
        let mut c = Clock::new(80 * MHZ, 1_000);
        c.rebase(80_000, 160 * MHZ);
        let t = c.now(80_000 + 160_000);
        assert_eq!(t, VTime::from_ms(2));
        assert_eq!(systimer_count(t, e, 0), 32_000);
    }

    #[test]
    fn postcard_round_trip_keeps_time_and_cycles() {
        let mut c = counting_clock(80 * MHZ, 1_750);
        c.stall(1_000, 3_333, true);
        c.rebase(1_000, 160 * MHZ);
        c.idle_until(1_000, VTime::from_us(500));
        c.stall(1_000, 7, false);
        c.set_counting(1_500, false);
        c.set_counting(2_000, true);

        let bytes = postcard::to_allocvec(&c).expect("encode");
        let r: Clock = postcard::from_bytes(&bytes).expect("decode");
        assert_eq!(r, c);
        assert_eq!(r.now(2_000), c.now(2_000));
        assert_eq!(r.cycle_count(2_000), c.cycle_count(2_000));
        assert_eq!(
            r.insns_until(2_000, VTime::from_ms(1)),
            c.insns_until(2_000, VTime::from_ms(1))
        );
        assert!(r.counting());
        assert_eq!(r.cpu_hz(), 160 * MHZ);
        assert_eq!(r.cpi_milli(), 1_750);
        assert_eq!((r.stall_ps(), r.idle_ps()), (c.stall_ps(), c.idle_ps()));
        let mut r2 = r;
        let mut c2 = c;
        for _ in 0..1_000 {
            r2.stall(2_000, 1, true);
            c2.stall(2_000, 1, true);
            assert_eq!(r2.cycle_count(2_000), c2.cycle_count(2_000));
        }
        assert_eq!(r2.now(3_000), c2.now(3_000));
    }

    /// Every golden was recorded under the `fast` column, so a table edit fails here first.
    #[test]
    fn the_fast_column_is_the_recorded_behaviour() {
        let fast = TimingProfile::fast();
        assert_eq!(
            *fast,
            TimingProfile {
                cpi_milli: DEFAULT_CPI_MILLI,
                cache_model: CacheVariant::ColdPage,
                cache_fill_ps: 0,
                cache_stall_counts_cycles: false,
                sha_block_ps: 0,
                aes_block_ps: 0,
                rsa_op_ps: 0,
                flash_pp_ps: 0,
                flash_se_ps: 0,
                flash_be_ps: 0,
                flash_ce_ps: 0,
                spi1_clocked: false,
                spi2_clocked: false,
                spi2_overhead_ps: 0,
                i2c_clocked: false,
                usj_drain_ps: 0,
                usj_enum_reset_ps: 0,
                usj_enum_wake_ps: 0,
                uart_paced: false,
                rtc_slow_hz: 136_000,
                ble_init_ps: 0,
                ble_enable_ps: 0,
                ble_enable_nvs_cal_ps: 0,
                ble_disable_ps: 0,
                ble_deinit_ps: 0,
                taken_branch_cycles: 0,
                jump_cycles: 0,
                split_redirect_cycles: 0,
                load_use_cycles: 0,
                div_base_cycles: 0,
                mmio_load_apb_cycles: 0,
                mmio_store_apb_cycles: 0,
                cache_first_word_ps: 0,
                cache_miss_cycles: 0,
                mulh_cycles: 0,
                mmio_cpu_cycles: 0,
                sram_bank_cycles: 0,
                adc_conversion_ps: 0,
            }
        );
        assert_eq!(TimingProfile::default(), *fast);
        assert_ne!(TimingProfile::device(), fast);
        assert_ne!(
            TimingProfile::device().identity_bytes(),
            fast.identity_bytes()
        );
    }

    #[test]
    fn every_numeric_row_reads_and_writes_by_name() {
        let device = TimingProfile::device();
        for name in TimingProfile::NAMES {
            if *name == "cache_model" {
                assert_eq!(device.get_by_name(name), None);
                assert!(device.clone().set_by_name(name, 0).is_err());
                continue;
            }
            let value = device.get_by_name(name).expect("a numeric row");
            let mut p = device.clone();
            p.set_by_name(name, value).expect("the value it holds");
            assert_eq!(&p, device, "{name}");
            let other = if value == 0 { 1 } else { 0 };
            p.set_by_name(name, other).expect("0 and 1 fit every row");
            assert_eq!(p.get_by_name(name), Some(other), "{name}");
            assert_ne!(&p, device, "{name}");
        }
        let mut p = device.clone();
        assert!(p.set_by_name("spi2_clocked", 2).is_err(), "a flag");
        assert!(
            p.set_by_name("cpi_milli", u64::MAX).is_err(),
            "a 32-bit row"
        );
        assert!(p.set_by_name("no_such_row", 1).is_err());
    }

    #[test]
    fn the_table_parser_refuses_what_it_cannot_map_onto_a_field() {
        let good = TIMING_PROFILES_TOML;
        assert!(TimingProfile::parse_table(good).is_ok());
        let extra = format!("{good}\n[[constant]]\nname = \"warp\"\nfast = 0\ndevice = 1\n");
        assert!(TimingProfile::parse_table(&extra).is_err());
        let cut = good.replacen("name = \"rsa_op_ps\"", "name = \"cpi_milli\"", 1);
        assert!(TimingProfile::parse_table(&cut).is_err());
        let flag = good.replacen(
            "name = \"uart_paced\"\nunit = \"bool\"\nfast = 0",
            "name = \"uart_paced\"\nunit = \"bool\"\nfast = 2",
            1,
        );
        assert_ne!(flag, good);
        assert!(TimingProfile::parse_table(&flag).is_err());
        // Other tables are skipped, not refused.
        let anchored = format!("{good}\n[[anchor]]\nname = \"x\"\nfast = 3\n");
        assert_eq!(
            TimingProfile::parse_table(&anchored),
            TimingProfile::parse_table(good)
        );
    }

    struct XorShift(u64);

    impl XorShift {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            self.0 = x;
            x
        }

        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }

        /// A C3 CPU frequency (PLL 160/80, XTAL 40 divided, RC_FAST) or any 1-200 MHz.
        fn cpu_hz(&mut self) -> u32 {
            const C3_HZ: [u32; 7] = [
                160 * MHZ,
                80 * MHZ,
                40 * MHZ,
                20 * MHZ,
                10 * MHZ,
                2 * MHZ,
                17_735_424,
            ];
            if self.below(2) == 0 {
                C3_HZ[self.below(C3_HZ.len() as u64) as usize]
            } else {
                MHZ + self.below(199 * MHZ as u64) as u32
            }
        }
    }

    #[test]
    fn clock_consistency_random_triples() {
        const S: u128 = PS_PER_S as u128;
        let mut rng = XorShift(0x9E37_79B9_7F4A_7C15);
        for _ in 0..10_000 {
            let hz1 = rng.cpu_hz();
            let cpi = 250 + rng.below(3_751) as u32; // CPI 0.25 to 4
            let vt = VTime(rng.below(1 << 50)); // up to about 13 days
            let mut c = counting_clock(hz1, cpi);
            let ppi1 = (PS_PER_S / hz1 as u64 * cpi as u64 / 1_000).max(1);
            assert_eq!(c.ps_per_insn(), ppi1);

            let idle = rng.below(vt.0 + 1);
            c.idle_until(0, VTime(idle));
            let n = c.insns_until(0, vt);
            assert!(n >= 1 && c.now(n) >= vt);
            assert!(n == 1 || c.now(n - 1) < vt);
            let t = c.now(n);
            assert_eq!(t.0, idle + n * ppi1);

            let exec1 = n as u128 * ppi1 as u128;
            assert_eq!(c.cycle_count(n) as u128, exec1 * hz1 as u128 / S);
            // Close to CPI per instruction; the per-cycle period floors.
            let by_cpi = n as u128 * cpi as u128 / 1_000;
            let slack = 2 + n as u128 * (hz1 as u128 * (1_000 + cpi as u128) / 1_000) / S;
            assert!(c.cycle_count(n) as u128 <= by_cpi + 1);
            assert!(c.cycle_count(n) as u128 + slack >= by_cpi);

            let st = systimer_count(t, VTime(0), 0);
            assert_eq!(st, t.0 / SYSTIMER_TICK_PS);
            assert!((16 * t.as_us()).abs_diff(st) < 16);

            let hz2 = rng.cpu_hz();
            let before = (c.now(n), c.cycle_count(n));
            c.rebase(n, hz2);
            assert_eq!((c.now(n), c.cycle_count(n)), before);
            let ppi2 = c.ps_per_insn();
            let m = 1 + rng.below(1 << 20);
            let stall = rng.below(1 << 30);
            let counted = rng.below(2) == 0;
            c.stall(n + m, stall, counted);
            let idle2 = rng.below(1 << 40);
            let t2 = VTime(c.now(n + m).0 + idle2);
            c.idle_until(n + m, t2);
            let k = 1 + rng.below(1 << 10);
            assert_eq!(
                c.now(n + m + k).0,
                t.0 + m * ppi2 + stall + idle2 + k * ppi2
            );
            let exec2 = (m + k) as u128 * ppi2 as u128 * hz2 as u128
                + if counted {
                    stall as u128 * hz2 as u128
                } else {
                    0
                };
            assert_eq!(
                c.cycle_count(n + m + k) as u128,
                (exec1 * hz1 as u128 + exec2) / S
            );
            assert_eq!(c.idle_ps(), idle + idle2);
            assert_eq!(c.stall_ps(), stall);
        }
    }
}
