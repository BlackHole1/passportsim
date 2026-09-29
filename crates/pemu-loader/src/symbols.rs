//! Symbol tables of app, bootloader and ROM ELFs, local symbols included: looked up by name for
//! HLE binding, and by address for the magic-range proof and tripwires.

use std::collections::BTreeMap;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SymKind {
    /// `STT_NOTYPE` and anything not listed.
    NoType,
    Func,
    Object,
    Section,
    File,
    Tls,
}

/// The order is the preference of [`SymbolTable::lookup`].
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum SymBind {
    Global,
    Weak,
    Local,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SymSection {
    Undefined,
    Absolute,
    Common,
    Index(usize),
    /// Reserved or processor-specific section index.
    Other,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Symbol {
    pub name: String,
    pub addr: u32,
    pub size: u32,
    pub kind: SymKind,
    pub bind: SymBind,
    pub section: SymSection,
}

impl Symbol {
    /// A zero-size symbol covers nothing.
    pub fn covers(&self, addr: u32) -> bool {
        addr.wrapping_sub(self.addr) < self.size && addr >= self.addr
    }

    pub fn is_defined(&self) -> bool {
        self.section != SymSection::Undefined
    }
}

/// Local symbols stay in, so one name can map to several symbols.
#[derive(Clone, Debug, Default)]
pub struct SymbolTable {
    syms: Vec<Symbol>,
    /// Indexes of defined symbols, sorted by address, then table order.
    by_addr: Vec<usize>,
    by_name: BTreeMap<String, Vec<usize>>,
    /// Largest size of a defined symbol, bounding the backward scan of [`SymbolTable::covering`].
    max_size: u32,
}

impl SymbolTable {
    pub fn new(syms: Vec<Symbol>) -> SymbolTable {
        let mut by_addr: Vec<usize> = (0..syms.len()).filter(|&i| syms[i].is_defined()).collect();
        by_addr.sort_by_key(|&i| (syms[i].addr, i));
        let mut by_name: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, s) in syms.iter().enumerate() {
            by_name.entry(s.name.clone()).or_default().push(i);
        }
        let max_size = by_addr.iter().map(|&i| syms[i].size).max().unwrap_or(0);
        SymbolTable {
            syms,
            by_addr,
            by_name,
            max_size,
        }
    }

    pub fn len(&self) -> usize {
        self.syms.len()
    }

    pub fn is_empty(&self) -> bool {
        self.syms.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, Symbol> {
        self.syms.iter()
    }

    /// Defined or not, in table order.
    pub fn named<'a>(&'a self, name: &str) -> impl Iterator<Item = &'a Symbol> + 'a {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .map(move |&i| &self.syms[i])
    }

    /// The defined symbol with this name: global before weak before local, then table order.
    pub fn lookup(&self, name: &str) -> Option<&Symbol> {
        self.named(name)
            .filter(|s| s.is_defined())
            .min_by_key(|s| s.bind)
    }

    pub fn addr_of(&self, name: &str) -> Option<u32> {
        self.lookup(name).map(|s| s.addr)
    }

    /// Defined symbols whose address is exactly `addr`, in table order.
    pub fn at(&self, addr: u32) -> impl Iterator<Item = &Symbol> + '_ {
        let start = self.by_addr.partition_point(|&i| self.syms[i].addr < addr);
        self.by_addr[start..]
            .iter()
            .map(|&i| &self.syms[i])
            .take_while(move |s| s.addr == addr)
    }

    /// Defined symbols with a non-zero size that cover `addr`, in address order.
    pub fn covering(&self, addr: u32) -> Vec<&Symbol> {
        self.overlapping(addr, u64::from(addr) + 1)
    }

    /// Defined symbols with a non-zero size that overlap `[start, end)`, in address order.
    pub fn overlapping(&self, start: u32, end: u64) -> Vec<&Symbol> {
        let upper = self
            .by_addr
            .partition_point(|&i| u64::from(self.syms[i].addr) < end);
        let mut out = Vec::new();
        for &i in self.by_addr[..upper].iter().rev() {
            let s = &self.syms[i];
            if u64::from(s.addr) + u64::from(self.max_size) <= u64::from(start) {
                break;
            }
            if s.size > 0 && u64::from(s.addr) + u64::from(s.size) > u64::from(start) {
                out.push(s);
            }
        }
        out.reverse();
        out
    }

    /// Defined symbols of any size whose address lies in `[start, end)`, in address order.
    pub fn in_range(&self, start: u32, end: u64) -> impl Iterator<Item = &Symbol> + '_ {
        let lo = self.by_addr.partition_point(|&i| self.syms[i].addr < start);
        self.by_addr[lo..]
            .iter()
            .map(|&i| &self.syms[i])
            .take_while(move |s| u64::from(s.addr) < end)
    }

    /// Prefers global over weak over local.
    pub fn func_at(&self, addr: u32) -> Option<&Symbol> {
        self.covering(addr)
            .into_iter()
            .filter(|s| s.kind == SymKind::Func)
            .min_by_key(|s| s.bind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, addr: u32, size: u32, kind: SymKind, bind: SymBind) -> Symbol {
        Symbol {
            name: name.to_string(),
            addr,
            size,
            kind,
            bind,
            section: SymSection::Index(1),
        }
    }

    fn table() -> SymbolTable {
        let mut undef = sym("ext", 0, 0, SymKind::NoType, SymBind::Global);
        undef.section = SymSection::Undefined;
        SymbolTable::new(vec![
            sym("helper", 0x100, 0x10, SymKind::Func, SymBind::Local),
            sym("helper", 0x200, 0x10, SymKind::Func, SymBind::Local),
            sym("helper", 0x300, 0x08, SymKind::Func, SymBind::Global),
            sym("big", 0x080, 0x400, SymKind::Object, SymBind::Global),
            sym("marker", 0x110, 0, SymKind::NoType, SymBind::Global),
            undef,
        ])
    }

    #[test]
    fn lookup_prefers_global_and_keeps_locals() {
        let t = table();
        assert_eq!(t.named("helper").count(), 3);
        assert_eq!(t.addr_of("helper"), Some(0x300));
        assert_eq!(t.lookup("ext"), None);
        assert_eq!(t.named("ext").count(), 1);
    }

    #[test]
    fn covering_uses_sizes_and_skips_zero_size() {
        let t = table();
        let names: Vec<_> = t.covering(0x105).iter().map(|s| s.addr).collect();
        assert_eq!(names, vec![0x080, 0x100]);
        assert_eq!(t.func_at(0x105).map(|s| s.addr), Some(0x100));
        assert_eq!(t.func_at(0x110), None);
        assert_eq!(t.covering(0x480).len(), 0);
        assert_eq!(t.at(0x110).count(), 1);
        assert_eq!(t.in_range(0x100, 0x201).count(), 3);
        assert_eq!(t.overlapping(0x480, 0x500).len(), 0);
        assert_eq!(t.overlapping(0x47f, 0x500).len(), 1);
    }
}
