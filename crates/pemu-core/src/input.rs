//! Every input that reaches the machine from outside. Host-to-guest rings are transport only: the
//! run loop drains them into journal entries, which stamp the time, at every slice boundary, so
//! a session and its replay see the same bytes at the same virtual times.

use serde::{Deserialize, Serialize};

use crate::snap::snap_struct;

/// An input to the machine; every input is journaled.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum InputEvent {
    /// The board turns the pressed set into an ADC ladder voltage.
    Button { id: ButtonId, down: bool },
    /// Not on the ladder: it drives the power rail state machine.
    Power { down: bool },
    /// USB cable plugged or unplugged.
    UsbCable { plugged: bool },
    /// A host client opened or closed the CDC-ACM port.
    UsbClient { open: bool },
    /// RFC 2217 line state from a host endpoint.
    UsbLine { dtr: bool, rts: bool },
    /// Bytes towards the guest, drained from `HostIo::usj_rx` or written by an agent command.
    SerialIn { chan: SerialChan, data: Vec<u8> },
    /// A partial battery setting.
    Battery(BatterySet),
    /// One card tap: the reader script of `nfc.tap({ops})`.
    NfcTap { ops: Vec<NfcOp> },
    /// Journaled as consumed; `seq` numbers one stream's chunks from 0, so the journal sees a
    /// live capture drop data (`crate::journal::LiveStream`).
    MicChunk { seq: u64, samples: Vec<i16> },
    /// One inbound Wi-Fi bridge message, numbered like `MicChunk`: a whole WISP v1 packet from the
    /// relay's server side, since the bridge proxies host sockets, not Ethernet frames.
    NetFrame { seq: u64, data: Vec<u8> },
    /// One inbound HCI packet from an external controller, numbered like `MicChunk`; opaque here.
    HciPacket { seq: u64, data: Vec<u8> },
    /// Wall-clock epoch from the host: the only way host time enters the machine.
    RtcEpoch { unix_us: u64 },
    /// A change to the scripted world.
    Env(EnvChange),
}

#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub enum ButtonId {
    #[default]
    Up,
    Down,
    Ok,
}

impl ButtonId {
    pub const ALL: [ButtonId; 3] = [ButtonId::Up, ButtonId::Down, ButtonId::Ok];
}

/// Serial channel of `InputEvent::SerialIn`. Only USB-Serial-JTAG has a host-to-guest ring, so
/// the machine refuses a `SerialIn` on [`SerialChan::UART0`] rather than drop the bytes.
#[derive(
    Copy, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize,
)]
pub struct SerialChan(pub u8);

impl SerialChan {
    pub const USJ: SerialChan = SerialChan(0);
    pub const UART0: SerialChan = SerialChan(1);
}

/// Battery settings of `InputEvent::Battery`; a set changes only the fields it names. Integers,
/// since floats stay out of anything the guest reads back.
/// UNVERIFIED field set and units: a design choice.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct BatterySet {
    /// Whole percent, 0 to 100.
    pub soc: Option<u8>,
    pub mv: Option<u16>,
    /// Tenths of a degree Celsius.
    pub temp_c_deci: Option<i16>,
    /// Whether a battery is connected to the gauge at all.
    pub present: Option<bool>,
}

/// One step of the `nfc.tap({ops})` reader script against the NTAG213 model, as raw command bytes.
/// UNVERIFIED shape: a design choice. Variants may be added; existing ones never change.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum NfcOp {
    /// The card enters the field, where the NFC counter may increment on the first read.
    FieldOn,
    Cmd(Vec<u8>),
    FieldOff,
}

/// Environment change of `InputEvent::Env`; a new kind of scripted world adds its own variant.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum EnvChange {
    /// What the microphone captures from now on.
    MicSource(MicSource),
    /// A script for the scripted BLE central, as `pemu_radio::ble::central::encode_script`
    /// writes it; opaque here, decoded by the bound `ble` radio module.
    BleCentral {
        /// The encoded script.
        script: Vec<u8>,
    },
    /// The access points a scan finds, replacing the ones set before.
    WifiAps(Vec<WifiAp>),
    /// The external HCI bridge attaches or detaches; while attached, the guest's HCI packets go to
    /// the external controller, which answers with [`InputEvent::HciPacket`].
    BleHciBridge {
        /// True to attach, false to detach.
        attached: bool,
    },
    /// The Wi-Fi bridge attaches or detaches; while attached, a guest TCP connection to
    /// `host.emu.internal` on a route's port is proxied to the host loopback port it names. The
    /// routes are journaled, so the journal records every destination a run could reach.
    WifiBridge {
        /// When false, `routes` is empty.
        attached: bool,
        /// At most [`NET_MAX_ROUTES`].
        routes: Vec<NetRoute>,
    },
}

/// One allowlisted port of [`EnvChange::WifiBridge`]: the only way the guest reaches loopback.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct NetRoute {
    /// The TCP port the guest connects to on `host.emu.internal`.
    pub port: u16,
    pub host_port: u16,
}

pub const NET_MAX_ROUTES: usize = 16;

impl NetRoute {
    /// At most [`NET_MAX_ROUTES`], no port 0 on either side, and no guest port twice.
    pub fn check_all(routes: &[NetRoute]) -> bool {
        routes.len() <= NET_MAX_ROUTES
            && routes.iter().all(|r| r.port != 0 && r.host_port != 0)
            && routes
                .iter()
                .enumerate()
                .all(|(i, r)| routes[..i].iter().all(|o| o.port != r.port))
    }
}

snap_struct!(NetRoute { port, host_port });

/// One scripted Wi-Fi access point of [`EnvChange::WifiAps`]. `psk` is secret: [`WifiAp::check`]
/// refuses a key the `SecretSet` could not mask, and a journal export drops the whole entry,
/// because a `Vec<u8>` renders as a number array no string masker sees.
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct WifiAp {
    pub ssid: String,
    /// A scripted one starts with the `02:00:00` placeholder prefix (`docs/secrets.md` section 1).
    pub bssid: [u8; 6],
    /// In dBm, as a scan reports it.
    pub rssi: i8,
    pub channel: u8,
    /// `wifi_auth_mode_t`.
    pub authmode: u8,
    /// Empty for an open network.
    pub psk: Vec<u8>,
}

/// Largest SSID, `wifi_ap_record_t.ssid` less its NUL (`esp_wifi_types_generic.h`).
pub const WIFI_MAX_SSID: usize = 32;
/// Highest 2.4 GHz channel the default country `"01"` allows.
pub const WIFI_MAX_CHANNEL: u8 = 11;
/// Highest `wifi_auth_mode_t`, `WIFI_AUTH_WPA3_ENT_192` (`esp_wifi_types_generic.h`).
pub const WIFI_MAX_AUTH: u8 = 10;
pub const WIFI_AUTH_OPEN: u8 = 0;
/// Shortest key the `SecretSet` keeps as a credential (`pemu_api::secret_set::MIN_CREDENTIAL_LEN`);
/// a shorter one could not be masked in an output.
pub const WIFI_MIN_PSK: usize = 6;
/// A 63-byte passphrase or a 64-byte hex PSK (`wifi_sta_config_t.password` is 64 plus NUL).
pub const WIFI_MAX_PSK: usize = 64;

/// Why a scripted access point was refused ([`WifiAp::check`]).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum WifiApError {
    Ssid,
    Channel,
    Auth,
    /// A key for an open network, or none for a secured one.
    Psk,
    /// A key the `SecretSet` could not hold ([`WifiAp::key_is_trackable`]).
    PskStrength,
}

impl WifiApError {
    /// The message a refusing command prints.
    pub fn detail(self) -> &'static str {
        match self {
            WifiApError::Ssid => "an ssid is 1 to 32 bytes",
            WifiApError::Channel => "a channel is 1 to 11 with the default country",
            WifiApError::Auth => "an auth mode is 0 (open) to 10 (wpa3 enterprise 192)",
            WifiApError::Psk => "an open network takes no key, and a secured one needs one",
            WifiApError::PskStrength => {
                "a key must be at least 6 bytes, at most 64, and not one repeated byte"
            }
        }
    }
}

impl WifiAp {
    /// What the driver would refuse before the AP reaches the world.
    pub fn check(&self) -> Result<(), WifiApError> {
        if self.ssid.is_empty() || self.ssid.len() > WIFI_MAX_SSID {
            return Err(WifiApError::Ssid);
        }
        if self.channel == 0 || self.channel > WIFI_MAX_CHANNEL {
            return Err(WifiApError::Channel);
        }
        if self.authmode > WIFI_MAX_AUTH {
            return Err(WifiApError::Auth);
        }
        if (self.authmode == WIFI_AUTH_OPEN) != self.psk.is_empty() {
            return Err(WifiApError::Psk);
        }
        if !self.psk.is_empty() && !Self::key_is_trackable(&self.psk) {
            return Err(WifiApError::PskStrength);
        }
        Ok(())
    }

    /// Whether the `SecretSet` keeps this key, so it masks in outputs and taints the instance.
    /// Mirrors `pemu_api::secret_set::SecretSetBuilder::nvs_credential`.
    pub fn key_is_trackable(psk: &[u8]) -> bool {
        if psk.len() > WIFI_MAX_PSK {
            return false;
        }
        let key = psk.strip_suffix(&[0]).unwrap_or(psk);
        key.len() >= WIFI_MIN_PSK && !key.windows(2).all(|w| w[0] == w[1])
    }
}

impl core::fmt::Debug for WifiAp {
    /// Prints the key's length only, so a `Debug` of a journal entry cannot bypass redaction.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WifiAp")
            .field("ssid", &self.ssid)
            .field("bssid", &self.bssid)
            .field("rssi", &self.rssi)
            .field("channel", &self.channel)
            .field("authmode", &self.authmode)
            .field("psk", &format_args!("<{} bytes>", self.psk.len()))
            .finish()
    }
}

snap_struct!(WifiAp {
    ssid,
    bssid,
    rssi,
    channel,
    authmode,
    psk
});

/// Microphone source of [`EnvChange::MicSource`]; `Live` is browser-only.
/// UNVERIFIED field set: a design choice.
#[derive(Clone, Default, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum MicSource {
    #[default]
    Silence,
    /// A steady tone from integers only, keeping the platform libm out of every state path.
    Tone {
        hz: u32,
        /// Peak, as a signed 16-bit sample.
        amplitude: i16,
    },
    /// A host-side audio asset; the samples arrive as `InputEvent::MicChunk`.
    File { name: String },
    /// Browser `getUserMedia`; a gap in the chunks' `seq` makes the run live, not replayable.
    Live,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The snapshot codec round-trip is what lets `journal_pending` hold future-stamped inputs.
    #[test]
    fn every_event_round_trips_through_postcard() {
        let events = vec![
            InputEvent::Button {
                id: ButtonId::Ok,
                down: true,
            },
            InputEvent::Power { down: false },
            InputEvent::UsbCable { plugged: true },
            InputEvent::UsbClient { open: true },
            InputEvent::UsbLine {
                dtr: true,
                rts: false,
            },
            InputEvent::SerialIn {
                chan: SerialChan::USJ,
                data: b"help\r\n".to_vec(),
            },
            InputEvent::Battery(BatterySet {
                soc: Some(42),
                mv: Some(3_700),
                temp_c_deci: Some(-105),
                present: Some(true),
            }),
            InputEvent::NfcTap {
                ops: vec![
                    NfcOp::FieldOn,
                    NfcOp::Cmd(vec![0x30, 0x04]),
                    NfcOp::FieldOff,
                ],
            },
            InputEvent::MicChunk {
                seq: 7,
                samples: vec![-32_768, 0, 32_767],
            },
            InputEvent::NetFrame {
                seq: 1,
                data: vec![0xff; 6],
            },
            InputEvent::HciPacket {
                seq: 2,
                data: vec![0x04, 0x0e],
            },
            InputEvent::RtcEpoch {
                unix_us: 1_700_000_000_000_000,
            },
            InputEvent::Env(EnvChange::MicSource(MicSource::Tone {
                hz: 1_000,
                amplitude: 8_192,
            })),
            InputEvent::Env(EnvChange::MicSource(MicSource::File {
                name: "beep.wav".to_string(),
            })),
            InputEvent::Env(EnvChange::MicSource(MicSource::Live)),
            InputEvent::Env(EnvChange::BleCentral {
                script: vec![1, 1, 3],
            }),
            InputEvent::Env(EnvChange::WifiAps(vec![WifiAp {
                ssid: "G2-Alpha".to_string(),
                bssid: [0x02, 0x00, 0x00, 0x47, 0x32, 0x01],
                rssi: -42,
                channel: 1,
                authmode: 3,
                psk: b"scripted-key".to_vec(),
            }])),
        ];
        for ev in events {
            let bytes = postcard::to_allocvec(&ev).expect("event serializes");
            let back: InputEvent = postcard::from_bytes(&bytes).expect("event deserializes");
            assert_eq!(back, ev);
        }
    }

    #[test]
    fn a_battery_set_is_partial() {
        let only_soc = BatterySet {
            soc: Some(80),
            ..BatterySet::default()
        };
        assert_eq!(only_soc.soc, Some(80));
        assert_eq!(only_soc.mv, None);
        assert_eq!(only_soc.temp_c_deci, None);
        assert_eq!(only_soc.present, None);
        assert_eq!(BatterySet::default(), BatterySet::default());
    }

    #[test]
    fn a_scripted_access_point_is_checked_and_its_key_is_never_printed() {
        let good = WifiAp {
            ssid: "G2-Bravo".to_string(),
            bssid: [0x02, 0x00, 0x00, 0x47, 0x32, 0x02],
            rssi: -60,
            channel: 6,
            authmode: WIFI_AUTH_OPEN,
            psk: Vec::new(),
        };
        assert_eq!(good.check(), Ok(()));
        let mut long = good.clone();
        long.ssid = "s".repeat(WIFI_MAX_SSID + 1);
        assert_eq!(long.check(), Err(WifiApError::Ssid));
        let mut off_channel = good.clone();
        off_channel.channel = WIFI_MAX_CHANNEL + 1;
        assert_eq!(off_channel.check(), Err(WifiApError::Channel));
        let mut bad_auth = good.clone();
        bad_auth.authmode = WIFI_MAX_AUTH + 1;
        bad_auth.psk = b"secret-key".to_vec();
        assert_eq!(bad_auth.check(), Err(WifiApError::Auth));
        let mut open_with_key = good.clone();
        open_with_key.psk = b"secret-key".to_vec();
        assert_eq!(open_with_key.check(), Err(WifiApError::Psk));
        let mut secured = good.clone();
        secured.authmode = 3;
        assert_eq!(secured.check(), Err(WifiApError::Psk));
        secured.psk = b"secret-key".to_vec();
        assert_eq!(secured.check(), Ok(()));
        let printed = format!("{secured:?}");
        assert!(!printed.contains("secret-key"), "the key is never printed");
        assert!(printed.contains("<10 bytes>"));
    }

    #[test]
    fn a_key_the_secret_set_could_not_hold_is_refused_by_the_check_itself() {
        let secured = |psk: &[u8]| WifiAp {
            ssid: "G2-Secure".to_string(),
            bssid: [0x02, 0x00, 0x00, 0x47, 0x32, 0x06],
            rssi: -55,
            channel: 6,
            authmode: 3,
            psk: psk.to_vec(),
        };
        for weak in [b"abc".as_slice(), b"12345", b"aaaaaaaa", b"\0\0\0\0\0\0\0"] {
            assert_eq!(
                secured(weak).check(),
                Err(WifiApError::PskStrength),
                "{weak:?} is not a value the set keeps"
            );
        }
        // A hex PSK is exactly 64 bytes; one byte more is refused.
        let hex_psk: Vec<u8> = (0..WIFI_MAX_PSK)
            .map(|i| b"0123456789abcdef"[i % 16])
            .collect();
        assert_eq!(secured(&hex_psk).check(), Ok(()));
        let mut too_long = hex_psk.clone();
        too_long.push(b'f');
        assert_eq!(
            secured(&too_long).check(),
            Err(WifiApError::PskStrength),
            "over the 64-byte bound"
        );
        assert_eq!(secured(b"scripted-key").check(), Ok(()));
        // The trailing NUL the set ignores is ignored here too.
        assert_eq!(secured(b"abcdef\0").check(), Ok(()));
        assert_eq!(
            secured(b"abcde\0").check(),
            Err(WifiApError::PskStrength),
            "five bytes and a terminator is still five bytes"
        );
        const { assert!(WIFI_MIN_PSK == 6 && WIFI_MAX_PSK == 64) };
    }

    #[test]
    fn buttons_enumerate_in_a_fixed_order() {
        assert_eq!(ButtonId::ALL, [ButtonId::Up, ButtonId::Down, ButtonId::Ok]);
        assert_eq!(ButtonId::default(), ButtonId::Up);
        assert_eq!(SerialChan::default(), SerialChan::USJ);
        assert_eq!(MicSource::default(), MicSource::Silence);
    }
}
