//! Field access types of `specs/c3-registers.csv` (`FieldAccess`) and their mapping from IDF
//! header access strings (TRM v1.4 Glossary "Access Types for Registers", p.893-894).

/// Access type of one field, the `pemu_core::regstore::FieldAccess` vocabulary.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Access {
    Rw,
    Ro,
    Wo,
    W1c,
    W1s,
    /// Write 1 triggers an event; reads 0.
    Wt,
    /// Self-clearing.
    Sc,
    /// Clear on read.
    Rc,
}

impl Access {
    /// Every access type, in `FieldAccess` order.
    pub const ALL: [Access; 8] = [
        Access::Rw,
        Access::Ro,
        Access::Wo,
        Access::W1c,
        Access::W1s,
        Access::Wt,
        Access::Sc,
        Access::Rc,
    ];

    /// CSV spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Access::Rw => "RW",
            Access::Ro => "RO",
            Access::Wo => "WO",
            Access::W1c => "W1C",
            Access::W1s => "W1S",
            Access::Wt => "WT",
            Access::Sc => "SC",
            Access::Rc => "RC",
        }
    }

    /// Variant name of `pemu_core::regstore::FieldAccess`, for the generated tables.
    pub fn variant(self) -> &'static str {
        match self {
            Access::Rw => "Rw",
            Access::Ro => "Ro",
            Access::Wo => "Wo",
            Access::W1c => "W1c",
            Access::W1s => "W1s",
            Access::Wt => "Wt",
            Access::Sc => "Sc",
            Access::Rc => "Rc",
        }
    }

    /// Parses the CSV spelling.
    pub fn parse(s: &str) -> Option<Access> {
        Access::ALL.into_iter().find(|a| a.as_str() == s)
    }
}

/// Result of mapping an IDF access string.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Mapped {
    pub access: Access,
    /// True when the IDF string maps exactly; false when the type is inferred (UNVERIFIED).
    pub exact: bool,
    /// Why the mapping is inferred; empty when exact. Never contains a comma.
    pub reason: &'static str,
}

const SS: &str = "SS (hardware sets the field) is not modeled by the access type";
const WTC: &str = "WTC: cleared by writing the corresponding WT field";
const WTC_SS: &str = "WTC: cleared by writing the corresponding WT field; SS: hardware sets it";
/// `WS` with `SC`: the self-clear is the modeled half, as for `R/W/SS/SC`. eFuse CMD
/// (`R/WS/SC`) is one of the seed busy-wait rows, whose contract is a write that reads
/// back 0, which W1S would not give.
const WS_SC: &str = "WS: any write sets; SC: hardware clears it after the effect";

/// Maps an IDF header access string (`R/W`, `R/WTC/SS`, ...) to an access type.
pub fn map_idf(raw: &str) -> Result<Mapped, String> {
    let key: String = raw.chars().filter(|c| !c.is_whitespace()).collect();
    let exact = |access| Mapped {
        access,
        exact: true,
        reason: "",
    };
    let inferred = |access, reason| Mapped {
        access,
        exact: false,
        reason,
    };
    let mapped = match key.as_str() {
        "R/W" | "RW" => exact(Access::Rw),
        "RO" => exact(Access::Ro),
        "WO" => exact(Access::Wo),
        "WT" => exact(Access::Wt),
        "R/W/SC" => exact(Access::Sc),
        "R" => inferred(Access::Ro, "bare R read as RO"),
        "WOD" => inferred(Access::Wo, "WOD is not in the TRM glossary; read as WO"),
        "R/W/SS" => inferred(Access::Rw, SS),
        "R/W/SS/SC" => inferred(Access::Sc, SS),
        "R/WTC/SS" | "R/SS/WTC" => inferred(Access::Ro, WTC_SS),
        "R/SC/WTC" | "R/SS/SC/WTC" => inferred(Access::Ro, WTC_SS),
        "R/W/WTC" => inferred(Access::Rw, WTC),
        "R/W/WTC/SS" | "R/W/SS/WTC" => inferred(Access::Rw, WTC_SS),
        "R/WC/SS" | "R/WC/SC" | "R/WC/SS/SC" => {
            inferred(Access::W1c, "WC: any write clears; read as W1C")
        }
        "R/WS/SS" => inferred(Access::W1s, "WS: any write sets; read as W1S"),
        "R/WS/SC" | "R/WS/SS/SC" => inferred(Access::Sc, WS_SC),
        "R/SS/RC" => inferred(Access::Rc, SS),
        "R/W1" => inferred(Access::Rw, "W1: write once is not modeled"),
        "WL" => inferred(Access::Rw, "WL: writable only while the lock is off"),
        _ => return Err(format!("unknown IDF access type `{raw}`")),
    };
    Ok(mapped)
}
