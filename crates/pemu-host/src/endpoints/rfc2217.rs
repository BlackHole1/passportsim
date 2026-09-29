//! The server side of RFC 2217 (Telnet Com Port Control Option) over Telnet (RFC 854, 855, 856,
//! 858), as a pure codec: client bytes go in; guest data, line-state changes and the server's
//! answers come out.
//!
//! `SET-CONTROL` DTR and RTS become the (DTR, RTS) pair of `InputEvent::UsbLine`, forwarded only
//! on a change. Settings a USB CDC function cannot honour (baud, parity, flow control) are
//! acknowledged with the value asked for. Every `SET-*` is answered (client code plus 100), or a
//! waiting client stalls to its timeout, about 215 ms per esptool command.
//!
//! `PURGE-DATA` reaches one transport's queue ([`PurgeDirection`]), since a pty and a TCP client
//! may share an instance and esptool's connect purges five times over. A purge is not a barrier:
//! bytes not yet fanned out arrive after it, as on a real port.
//!
//! [`ResetBridge`] turns a reset pattern into the reset the chip needs, as Espressif's
//! `esp_rfc2217_server` does because reset lines do not survive network latency; the chip model
//! keeps TRM Table 30.3-2 exactly.

pub const IAC: u8 = 255;
pub const DONT: u8 = 254;
pub const DO: u8 = 253;
pub const WONT: u8 = 252;
pub const WILL: u8 = 251;
pub const SB: u8 = 250;
pub const SE: u8 = 240;

pub const OPT_BINARY: u8 = 0;
pub const OPT_SGA: u8 = 3;
pub const OPT_COM_PORT: u8 = 44;

pub mod cmd {
    /// `SET-BAUDRATE`, a 4-byte network-order value.
    pub const SET_BAUDRATE: u8 = 1;
    pub const SET_DATASIZE: u8 = 2;
    pub const SET_PARITY: u8 = 3;
    pub const SET_STOPSIZE: u8 = 4;
    pub const SET_CONTROL: u8 = 5;
    pub const NOTIFY_LINESTATE: u8 = 6;
    pub const NOTIFY_MODEMSTATE: u8 = 7;
    pub const FLOWCONTROL_SUSPEND: u8 = 8;
    pub const FLOWCONTROL_RESUME: u8 = 9;
    pub const SET_LINESTATE_MASK: u8 = 10;
    pub const SET_MODEMSTATE_MASK: u8 = 11;
    pub const PURGE_DATA: u8 = 12;
}

/// `PURGE-DATA` values, in pyserial's spelling ([`PurgeDirection`]).
pub mod purge {
    pub const RECEIVE: u8 = 1;
    pub const TRANSMIT: u8 = 2;
    pub const BOTH: u8 = 3;
}

pub const SERVER_OFFSET: u8 = 100;

/// `SET-CONTROL` values this codec acts on.
pub mod control {
    pub const DTR_REQUEST: u8 = 7;
    pub const DTR_ON: u8 = 8;
    pub const DTR_OFF: u8 = 9;
    pub const RTS_REQUEST: u8 = 10;
    pub const RTS_ON: u8 = 11;
    pub const RTS_OFF: u8 = 12;
    pub const FLOW_REQUEST: u8 = 0;
    pub const BREAK_REQUEST: u8 = 4;
    /// BREAK off, the state reported for a BREAK request.
    pub const BREAK_OFF: u8 = 6;
    /// No flow control, the setting reported for a flow-control request.
    pub const FLOW_NONE: u8 = 1;
}

/// The (DTR, RTS) pair as a pyserial client sets it (equal to the CDC `SET_CONTROL_LINE_STATE`
/// bits).
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct LineState {
    pub dtr: bool,
    pub rts: bool,
}

/// Which buffer a `PURGE-DATA` value names, in the client's direction: "receive" (1) is the
/// guest-to-client queue, "transmit" (2) the client-to-guest one, 3 both. Any other value names no
/// buffer and is still echoed; pyserial's reference server sends nothing, and its client
/// tolerates either.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PurgeDirection {
    /// Value 1, `PURGE_RECEIVE_BUFFER`: guest bytes the client has not read.
    FromGuest,
    /// Value 2, `PURGE_TRANSMIT_BUFFER`: client bytes the guest has not taken.
    ToGuest,
    Both,
}

impl PurgeDirection {
    pub const fn parse(value: u8) -> Option<PurgeDirection> {
        match value {
            purge::RECEIVE => Some(PurgeDirection::FromGuest),
            purge::TRANSMIT => Some(PurgeDirection::ToGuest),
            purge::BOTH => Some(PurgeDirection::Both),
            _ => None,
        }
    }

    pub const fn clears_from_guest(self) -> bool {
        matches!(self, PurgeDirection::FromGuest | PurgeDirection::Both)
    }

    pub const fn clears_to_guest(self) -> bool {
        matches!(self, PurgeDirection::ToGuest | PurgeDirection::Both)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Data(Vec<u8>),
    Line(LineState),
    Purge(PurgeDirection),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fed {
    /// Data, line changes and purges interleaved exactly as the client sent them.
    pub events: Vec<Event>,
    pub data: Vec<u8>,
    /// Bytes sent back to the client: negotiation answers and command replies.
    pub reply: Vec<u8>,
    /// Line-state changes in order, each different from the one before.
    pub lines: Vec<LineState>,
}

impl Fed {
    fn push_data(&mut self, b: u8) {
        self.data.push(b);
        match self.events.last_mut() {
            Some(Event::Data(run)) => run.push(b),
            _ => self.events.push(Event::Data(vec![b])),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Parse {
    Data,
    Iac,
    Negotiate(u8),
    Sub,
    SubIac,
}

/// Longest subnegotiation body kept: RFC 2217 values are at most 4 bytes, so longer is a client
/// error and is truncated rather than grown without bound.
const MAX_SUB: usize = 64;

#[derive(Clone, Debug)]
pub struct Rfc2217 {
    parse: Parse,
    sub: Vec<u8>,
    line: LineState,
    local: [bool; 256],
    remote: [bool; 256],
    baud: u32,
}

impl Default for Rfc2217 {
    fn default() -> Rfc2217 {
        Rfc2217::new(LineState::default())
    }
}

fn supported(option: u8) -> bool {
    matches!(option, OPT_BINARY | OPT_SGA | OPT_COM_PORT)
}

impl Rfc2217 {
    /// A session whose last reported line state is the one the machine holds, so a first
    /// `SET-CONTROL` that repeats it is not an event.
    pub fn new(line: LineState) -> Rfc2217 {
        Rfc2217 {
            parse: Parse::Data,
            sub: Vec::new(),
            line,
            local: [false; 256],
            remote: [false; 256],
            baud: 115_200,
        }
    }

    pub fn line(&self) -> LineState {
        self.line
    }

    /// The baud rate the client last set, acknowledged and ignored.
    pub fn baud(&self) -> u32 {
        self.baud
    }

    pub fn com_port_active(&self) -> bool {
        self.remote[usize::from(OPT_COM_PORT)] || self.local[usize::from(OPT_COM_PORT)]
    }

    /// Consumes client bytes. Parser state carries across calls, so a command split over two reads
    /// decodes the same.
    pub fn feed(&mut self, input: &[u8]) -> Fed {
        let mut fed = Fed::default();
        for &byte in input {
            self.parse = match (self.parse, byte) {
                (Parse::Data, IAC) => Parse::Iac,
                (Parse::Data, b) => {
                    fed.push_data(b);
                    Parse::Data
                }
                (Parse::Iac, IAC) => {
                    fed.push_data(IAC);
                    Parse::Data
                }
                (Parse::Iac, WILL | WONT | DO | DONT) => Parse::Negotiate(byte),
                (Parse::Iac, SB) => {
                    self.sub.clear();
                    Parse::Sub
                }
                // NOP, data mark, break, go-ahead and the rest carry nothing for a serial port.
                (Parse::Iac, _) => Parse::Data,
                (Parse::Negotiate(verb), option) => {
                    self.negotiate(verb, option, &mut fed.reply);
                    Parse::Data
                }
                (Parse::Sub, IAC) => Parse::SubIac,
                (Parse::Sub, b) => {
                    if self.sub.len() < MAX_SUB {
                        self.sub.push(b);
                    }
                    Parse::Sub
                }
                (Parse::SubIac, SE) => {
                    self.subnegotiation(&mut fed);
                    Parse::Data
                }
                (Parse::SubIac, b) => {
                    // `IAC IAC` inside a subnegotiation is a literal 255.
                    if self.sub.len() < MAX_SUB {
                        self.sub.push(b);
                    }
                    Parse::Sub
                }
            };
        }
        fed
    }

    /// Answers option negotiation, replying only when the state changes so two conforming ends
    /// never loop.
    fn negotiate(&mut self, verb: u8, option: u8, reply: &mut Vec<u8>) {
        let i = usize::from(option);
        match verb {
            WILL => {
                if supported(option) {
                    if !self.remote[i] {
                        self.remote[i] = true;
                        reply.extend([IAC, DO, option]);
                    }
                } else {
                    reply.extend([IAC, DONT, option]);
                }
            }
            WONT => {
                if self.remote[i] {
                    self.remote[i] = false;
                    reply.extend([IAC, DONT, option]);
                }
            }
            DO => {
                if supported(option) {
                    if !self.local[i] {
                        self.local[i] = true;
                        reply.extend([IAC, WILL, option]);
                    }
                } else {
                    reply.extend([IAC, WONT, option]);
                }
            }
            _ => {
                if self.local[i] {
                    self.local[i] = false;
                    reply.extend([IAC, WONT, option]);
                }
            }
        }
    }

    fn subnegotiation(&mut self, fed: &mut Fed) {
        let Some((&option, body)) = self.sub.split_first() else {
            return;
        };
        if option != OPT_COM_PORT {
            return;
        }
        let Some((&command, value)) = body.split_first() else {
            return;
        };
        let answer: Vec<u8> = match command {
            cmd::SET_BAUDRATE => {
                if let Ok(bytes) = <[u8; 4]>::try_from(value)
                    && u32::from_be_bytes(bytes) != 0
                {
                    self.baud = u32::from_be_bytes(bytes);
                }
                self.baud.to_be_bytes().to_vec()
            }
            cmd::SET_CONTROL => {
                let Some(&v) = value.first() else { return };
                vec![self.control(v, fed)]
            }
            cmd::PURGE_DATA => {
                // A value naming no buffer discards nothing but is echoed, so the client is not
                // left waiting for its timeout.
                if let Some(&v) = value.first()
                    && let Some(direction) = PurgeDirection::parse(v)
                {
                    fed.events.push(Event::Purge(direction));
                }
                value.to_vec()
            }
            cmd::SET_DATASIZE
            | cmd::SET_PARITY
            | cmd::SET_STOPSIZE
            | cmd::SET_LINESTATE_MASK
            | cmd::SET_MODEMSTATE_MASK => value.to_vec(),
            // Flow control suspend and resume have no reply; notifications flow server to client.
            _ => return,
        };
        fed.reply
            .extend([IAC, SB, OPT_COM_PORT, command + SERVER_OFFSET]);
        for b in answer {
            fed.reply.push(b);
            if b == IAC {
                fed.reply.push(IAC);
            }
        }
        fed.reply.extend([IAC, SE]);
    }

    fn control(&mut self, value: u8, fed: &mut Fed) -> u8 {
        let mut next = self.line;
        let answer = match value {
            control::DTR_ON => {
                next.dtr = true;
                value
            }
            control::DTR_OFF => {
                next.dtr = false;
                value
            }
            control::RTS_ON => {
                next.rts = true;
                value
            }
            control::RTS_OFF => {
                next.rts = false;
                value
            }
            control::DTR_REQUEST => match self.line.dtr {
                true => control::DTR_ON,
                false => control::DTR_OFF,
            },
            control::RTS_REQUEST => match self.line.rts {
                true => control::RTS_ON,
                false => control::RTS_OFF,
            },
            control::FLOW_REQUEST => control::FLOW_NONE,
            control::BREAK_REQUEST => control::BREAK_OFF,
            // Flow-control and BREAK settings are acknowledged and ignored: a CDC function has no
            // such lines.
            other => other,
        };
        if next != self.line {
            self.line = next;
            fed.lines.push(next);
            fed.events.push(Event::Line(next));
        }
        answer
    }
}

/// Appends guest bytes to `out` with every 255 doubled, as the Telnet data stream requires.
pub fn escape_into(data: &[u8], out: &mut Vec<u8>) {
    for &b in data {
        out.push(b);
        if b == IAC {
            out.push(IAC);
        }
    }
}

/// The server's reset translation: client (DTR, RTS) changes in, chip line states out.
///
/// esptool sends `ClassicReset` (`D0|R1|W0.1|D1|R0|W0.05|D0`) on every URL port, written for the
/// auto-reset circuit where RTS drives EN and DTR drives GPIO9. Fed row by row into TRM Table
/// 30.3-2, its (RTS 1, DTR 0) row resets at once with whatever download flag is set, which after a
/// Windows open (0, 0) is clear, so the ROM boots flash (`Wrong boot mode detected (0xa)`). The
/// chip is right; the fix belongs in a server between client and chip.
///
/// So entry into (RTS 1, DTR 0) is held until the next change says which reset was meant:
///
/// - GPIO9 low while EN is held, (1, 1), or EN released straight into GPIO9 low, (0, 1), is the
///   enter-bootloader pattern: the server drives `USBJTAGSerialReset`'s rows (0, 1), (1, 1),
///   (1, 0), then the client's state, so one reset lands in download mode from any state.
/// - EN released with GPIO9 high, (0, 0), is a plain reset: the held (1, 0), then (0, 0).
///
/// Every other change is forwarded as it is.
#[derive(Clone, Debug)]
pub struct ResetBridge {
    chip: LineState,
    /// The client entered (RTS 1, DTR 0) and the chip has not been told yet.
    held: bool,
}

/// (RTS 1, DTR 0): EN low, GPIO9 high.
const EN_LOW: LineState = LineState {
    dtr: false,
    rts: true,
};

/// The `USBJTAGSerialReset` rows that set the download flag and reset with it: (RTS 0, DTR 1),
/// (1, 1), (1, 0).
const DOWNLOAD_ROWS: [LineState; 3] = [
    LineState {
        dtr: true,
        rts: false,
    },
    LineState {
        dtr: true,
        rts: true,
    },
    EN_LOW,
];

impl ResetBridge {
    pub fn new(chip: LineState) -> ResetBridge {
        ResetBridge { chip, held: false }
    }

    /// The line states to drive the chip through for one client change; empty while a reset is
    /// held.
    pub fn client(&mut self, line: LineState) -> Vec<LineState> {
        let mut out = Vec::new();
        if self.held {
            if line == EN_LOW {
                return out;
            }
            self.held = false;
            if line.dtr {
                out.extend(DOWNLOAD_ROWS);
            } else {
                out.push(EN_LOW);
            }
            out.push(line);
        } else if line == EN_LOW {
            self.held = true;
            return out;
        } else {
            out.push(line);
        }
        self.drive(out)
    }

    /// What a closing session still owes the chip: a held reset is applied, because the client
    /// that asserted it expected one.
    pub fn close(&mut self) -> Vec<LineState> {
        if std::mem::take(&mut self.held) {
            self.drive(vec![EN_LOW])
        } else {
            Vec::new()
        }
    }

    /// Drops rows repeating the chip's current state and records where the chip ends.
    fn drive(&mut self, rows: Vec<LineState>) -> Vec<LineState> {
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            if row != self.chip {
                self.chip = row;
                out.push(row);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_control(v: u8) -> [u8; 7] {
        [IAC, SB, OPT_COM_PORT, cmd::SET_CONTROL, v, IAC, SE]
    }

    #[test]
    fn data_passes_through_with_doubled_iac_unescaped() {
        let mut s = Rfc2217::default();
        let fed = s.feed(&[b'a', IAC, IAC, b'b', 0xC0]);
        assert_eq!(fed.data, vec![b'a', 0xFF, b'b', 0xC0]);
        assert!(fed.reply.is_empty() && fed.lines.is_empty());
        let mut out = Vec::new();
        escape_into(&[1, 0xFF, 2], &mut out);
        assert_eq!(out, vec![1, 0xFF, 0xFF, 2]);
    }

    #[test]
    fn supported_options_are_accepted_once_and_others_refused() {
        let mut s = Rfc2217::default();
        let fed = s.feed(&[IAC, WILL, OPT_COM_PORT, IAC, DO, OPT_BINARY, IAC, WILL, 24]);
        assert_eq!(
            fed.reply,
            vec![IAC, DO, OPT_COM_PORT, IAC, WILL, OPT_BINARY, IAC, DONT, 24]
        );
        assert!(s.com_port_active());
        assert!(s.feed(&[IAC, WILL, OPT_COM_PORT]).reply.is_empty());
    }

    #[test]
    fn set_baudrate_is_acknowledged_with_the_value_in_force() {
        let mut s = Rfc2217::default();
        let mut msg = vec![IAC, SB, OPT_COM_PORT, cmd::SET_BAUDRATE];
        msg.extend(460_800u32.to_be_bytes());
        msg.extend([IAC, SE]);
        let fed = s.feed(&msg);
        let mut want = vec![IAC, SB, OPT_COM_PORT, cmd::SET_BAUDRATE + SERVER_OFFSET];
        want.extend(460_800u32.to_be_bytes());
        want.extend([IAC, SE]);
        assert_eq!(fed.reply, want);
        assert_eq!(s.baud(), 460_800);
        assert!(fed.data.is_empty());
    }

    /// esptool `ClassicReset` over RFC 2217: DTR1 RTS0, DTR1 RTS1, DTR0 RTS1, DTR1 RTS1, DTR1 RTS0,
    /// DTR0 RTS0, one line per command.
    #[test]
    fn set_control_reports_each_change_of_the_pair_and_no_repeat() {
        let mut s = Rfc2217::default();
        let mut stream = Vec::new();
        for v in [
            control::DTR_ON,
            control::RTS_OFF, // a repeat of RTS off: no event
            control::RTS_ON,
            control::DTR_OFF,
            control::DTR_ON,
            control::RTS_OFF,
            control::DTR_OFF,
        ] {
            stream.extend(set_control(v));
        }
        let fed = s.feed(&stream);
        let pairs: Vec<(bool, bool)> = fed.lines.iter().map(|l| (l.dtr, l.rts)).collect();
        assert_eq!(
            pairs,
            vec![
                (true, false),
                (true, true),
                (false, true),
                (true, true),
                (true, false),
                (false, false)
            ]
        );
        assert_eq!(fed.reply.len(), 7 * 7);
        assert_eq!(
            &fed.reply[..7],
            &[IAC, SB, OPT_COM_PORT, 105, control::DTR_ON, IAC, SE]
        );
    }

    #[test]
    fn a_state_request_reports_without_changing_the_line() {
        let mut s = Rfc2217::new(LineState {
            dtr: true,
            rts: false,
        });
        let fed = s.feed(&set_control(control::RTS_REQUEST));
        assert!(fed.lines.is_empty());
        assert_eq!(fed.reply[4], control::RTS_OFF);
        let fed = s.feed(&set_control(control::DTR_REQUEST));
        assert_eq!(fed.reply[4], control::DTR_ON);
    }

    #[test]
    fn a_command_split_across_reads_decodes_the_same() {
        let mut s = Rfc2217::default();
        let msg = set_control(control::DTR_ON);
        let mut lines = Vec::new();
        for b in msg {
            lines.extend(s.feed(&[b]).lines);
        }
        assert_eq!(
            lines,
            vec![LineState {
                dtr: true,
                rts: false
            }]
        );
    }

    fn purge_data(value: u8) -> Vec<u8> {
        vec![IAC, SB, OPT_COM_PORT, cmd::PURGE_DATA, value, IAC, SE]
    }

    fn purge_reply(value: u8) -> Vec<u8> {
        vec![
            IAC,
            SB,
            OPT_COM_PORT,
            cmd::PURGE_DATA + SERVER_OFFSET,
            value,
            IAC,
            SE,
        ]
    }

    #[test]
    fn each_purge_value_names_its_own_buffer_and_is_answered() {
        for (value, direction, from_guest, to_guest) in [
            (purge::RECEIVE, PurgeDirection::FromGuest, true, false),
            (purge::TRANSMIT, PurgeDirection::ToGuest, false, true),
            (purge::BOTH, PurgeDirection::Both, true, true),
        ] {
            let mut s = Rfc2217::default();
            let fed = s.feed(&purge_data(value));
            assert_eq!(fed.events, vec![Event::Purge(direction)], "value {value}");
            assert_eq!(
                (direction.clears_from_guest(), direction.clears_to_guest()),
                (from_guest, to_guest),
                "value {value}"
            );
            assert_eq!(fed.reply, purge_reply(value), "value {value}");
        }
    }

    #[test]
    fn an_unknown_purge_value_discards_nothing_and_is_echoed() {
        // 0xFF is left out: a client escapes it as `IAC IAC`, which another test covers.
        for value in [0u8, 4, 0x55, 0xFE] {
            let mut s = Rfc2217::default();
            let fed = s.feed(&purge_data(value));
            assert!(fed.events.is_empty(), "value {value}: {:?}", fed.events);
            assert_eq!(fed.reply, purge_reply(value), "value {value}");
        }
        let mut s = Rfc2217::default();
        let fed = s.feed(&[IAC, SB, OPT_COM_PORT, cmd::PURGE_DATA, IAC, SE]);
        assert!(fed.events.is_empty());
        assert_eq!(
            fed.reply,
            vec![
                IAC,
                SB,
                OPT_COM_PORT,
                cmd::PURGE_DATA + SERVER_OFFSET,
                IAC,
                SE
            ]
        );
    }

    #[test]
    fn the_esptool_flush_pair_keeps_its_order_and_directions() {
        let mut s = Rfc2217::default();
        let mut bytes = purge_data(purge::RECEIVE);
        bytes.extend(purge_data(purge::TRANSMIT));
        let fed = s.feed(&bytes);
        assert_eq!(
            fed.events,
            vec![
                Event::Purge(PurgeDirection::FromGuest),
                Event::Purge(PurgeDirection::ToGuest)
            ]
        );
    }

    #[test]
    fn data_and_line_changes_keep_their_order_within_one_feed() {
        let mut s = Rfc2217::new(LineState::default());
        let mut input = b"ab".to_vec();
        input.extend([
            IAC,
            SB,
            OPT_COM_PORT,
            cmd::SET_CONTROL,
            control::DTR_ON,
            IAC,
            SE,
        ]);
        input.extend(b"cd");
        let fed = s.feed(&input);
        assert_eq!(
            fed.events,
            vec![
                Event::Data(b"ab".to_vec()),
                Event::Line(LineState {
                    dtr: true,
                    rts: false
                }),
                Event::Data(b"cd".to_vec()),
            ]
        );
    }

    fn rows(lines: &[LineState]) -> Vec<(bool, bool)> {
        lines.iter().map(|l| (l.rts, l.dtr)).collect()
    }

    /// The client's lines after each `SET-CONTROL` of an esptool reset, starting from `open`, with
    /// repeats removed. After every `R` esptool writes DTR again (a `usbser.sys` workaround), which
    /// is always a repeat.
    fn client_lines(open: LineState, steps: &[(char, bool)]) -> Vec<LineState> {
        let mut now = open;
        let mut out = Vec::new();
        for &(line, on) in steps {
            let next = match line {
                'D' => LineState { dtr: on, ..now },
                _ => LineState { rts: on, ..now },
            };
            if next != now {
                out.push(next);
                now = next;
            }
        }
        out
    }

    const CLASSIC: &[(char, bool)] = &[
        ('D', false),
        ('R', true),
        ('D', true),
        ('R', false),
        ('D', false),
    ];
    const USB_JTAG: &[(char, bool)] = &[
        ('R', false),
        ('D', false),
        ('D', true),
        ('R', false),
        ('R', true),
        ('D', false),
        ('R', true),
        ('D', false),
        ('R', false),
    ];
    const HARD: &[(char, bool)] = &[('R', true), ('R', false)];

    /// The state after open: esptool on Windows clears both lines first; pyserial elsewhere opens
    /// with both on, DTR first (so the chip already saw (0, 1)).
    const WINDOWS_OPEN: LineState = LineState {
        dtr: false,
        rts: false,
    };
    const MACOS_OPEN: LineState = LineState {
        dtr: true,
        rts: true,
    };

    /// Runs client lines through a bridge and a copy of TRM Table 30.3-2 (as `pemu_board::usb_plug`
    /// implements it); each reset is `true` for download mode, `false` for a flash boot.
    fn chip_resets(
        chip_start: (LineState, bool),
        client: &[LineState],
    ) -> (Vec<bool>, ResetBridge) {
        let (mut line, mut flag) = chip_start;
        let mut bridge = ResetBridge::new(line);
        let mut resets = Vec::new();
        for &c in client {
            for row in bridge.client(c) {
                assert_ne!(row, line, "the bridge never repeats the chip's state");
                match (row.rts, row.dtr) {
                    (false, false) => flag = false,
                    (false, true) => flag = true,
                    (true, false) => resets.push(flag),
                    (true, true) => {}
                }
                line = row;
            }
        }
        (resets, bridge)
    }

    /// `ClassicReset` from a Windows open (0, 0): the server holds EN low, sees GPIO9 pulled low,
    /// and resets once into download mode.
    #[test]
    fn a_classic_reset_from_a_windows_open_resets_once_into_download_mode() {
        let client = client_lines(WINDOWS_OPEN, CLASSIC);
        let (resets, _) = chip_resets((WINDOWS_OPEN, false), &client);
        assert_eq!(resets, vec![true]);
    }

    #[test]
    fn a_classic_reset_from_a_macos_open_resets_once_into_download_mode() {
        let client = client_lines(MACOS_OPEN, CLASSIC);
        let (resets, _) = chip_resets((MACOS_OPEN, true), &client);
        assert_eq!(resets, vec![true]);
    }

    /// `USBJTAGSerialReset` sets the flag before EN goes low, so the held reset released into
    /// (0, 0) reaches download mode through the chip's own rows.
    #[test]
    fn a_usb_jtag_reset_resets_once_into_download_mode_from_either_open() {
        for (open, flag) in [(WINDOWS_OPEN, false), (MACOS_OPEN, true)] {
            let client = client_lines(open, USB_JTAG);
            let (resets, _) = chip_resets((open, flag), &client);
            assert_eq!(resets, vec![true], "open {open:?}");
        }
    }

    #[test]
    fn a_hard_reset_boots_the_app_and_the_tight_sequence_reaches_download_mode() {
        let client = client_lines(WINDOWS_OPEN, HARD);
        let (resets, _) = chip_resets((WINDOWS_OPEN, false), &client);
        assert_eq!(resets, vec![false]);

        // `U1,1|U0,1|W0.1|U1,0|W0.05|U0,0` (`U<dtr>,<rts>`), from a pair-level client.
        let tight = [
            LineState {
                dtr: true,
                rts: true,
            },
            EN_LOW,
            LineState {
                dtr: true,
                rts: false,
            },
            LineState {
                dtr: false,
                rts: false,
            },
        ];
        let (resets, _) = chip_resets((WINDOWS_OPEN, false), &tight);
        assert_eq!(resets, vec![true]);
    }

    #[test]
    fn the_bridge_holds_en_low_and_drives_the_usb_jtag_rows() {
        let mut bridge = ResetBridge::new(WINDOWS_OPEN);
        assert!(bridge.client(EN_LOW).is_empty(), "EN low is held");
        assert_eq!(
            rows(&bridge.client(LineState {
                dtr: true,
                rts: true
            })),
            vec![(false, true), (true, true), (true, false), (true, true)],
        );
        assert_eq!(
            rows(&bridge.client(LineState {
                dtr: true,
                rts: false
            })),
            vec![(false, true)],
            "after the reset every change is forwarded"
        );
        let mut bridge = ResetBridge::new(WINDOWS_OPEN);
        assert!(bridge.client(EN_LOW).is_empty());
        assert_eq!(rows(&bridge.close()), vec![(true, false)]);
        assert!(bridge.close().is_empty());
    }
}
