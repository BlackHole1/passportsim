//! The scripted access points a Wi-Fi scan returns.
//!
//! A scripted AP enters through the `env` command as a journaled
//! [`pemu_core::input::EnvChange::WifiAps`], so the value lives in `pemu-core`; this module adds
//! the scan record layout and the scan order. The pre-shared key is a secret input: it never
//! reaches an output and taints the instance. SSID, BSSID, channel and RSSI are caller-supplied
//! and never redacted.

pub use pemu_core::input::{
    WIFI_AUTH_OPEN as AUTH_OPEN, WIFI_MAX_AUTH as MAX_AUTH, WIFI_MAX_CHANNEL as MAX_CHANNEL,
    WIFI_MAX_SSID as MAX_SSID, WifiAp as ScriptedAp, WifiApError as ApError,
};

/// The placeholder BSSID prefix every scripted AP uses (`docs/secrets.md`).
pub const BSSID_PREFIX: [u8; 3] = [0x02, 0x00, 0x00];

/// Byte offsets inside the 92-byte `wifi_ap_record_t`, as `wifi.toml` `[driver]` pins them.
mod rec {
    pub const BSSID: usize = 0;
    pub const SSID: usize = 6;
    pub const PRIMARY: usize = 39;
    pub const RSSI: usize = 44;
    pub const AUTHMODE: usize = 48;
}

/// Writes `ap` as a `wifi_ap_record_t` into `out`, which is one record long. Unscripted fields
/// are left as they were, so a zeroed buffer reads as an open network with no country
/// information. A key is never part of a record: a PSK is not on the air.
pub fn write_record(ap: &ScriptedAp, out: &mut [u8]) {
    let put = |out: &mut [u8], at: usize, bytes: &[u8]| {
        if let Some(slot) = out.get_mut(at..at + bytes.len()) {
            slot.copy_from_slice(bytes);
        }
    };
    put(out, rec::BSSID, &ap.bssid);
    let ssid = ap.ssid.as_bytes();
    let len = ssid.len().min(MAX_SSID);
    put(out, rec::SSID, &ssid[..len]);
    put(out, rec::SSID + len, &[0]);
    put(out, rec::PRIMARY, &[ap.channel]);
    put(out, rec::RSSI, &[ap.rssi as u8]);
    put(out, rec::AUTHMODE, &[ap.authmode]);
}

/// What a sweep heard, strongest RSSI first as `esp_wifi_scan_get_ap_records` sorts; equal RSSIs
/// keep script order. Keys are dropped so sweep results never copy secrets into snapshots; the
/// scripted world keeps them for `RadioModule::secret_values`.
pub fn heard(aps: &[ScriptedAp]) -> Vec<ScriptedAp> {
    let mut out: Vec<ScriptedAp> = aps
        .iter()
        .map(|ap| ScriptedAp {
            psk: Vec::new(),
            ..ap.clone()
        })
        .collect();
    out.sort_by(|a, b| b.rssi.cmp(&a.rssi));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ap(ssid: &str, rssi: i8, channel: u8, authmode: u8) -> ScriptedAp {
        ScriptedAp {
            ssid: ssid.to_string(),
            bssid: [0x02, 0x00, 0x00, 0x47, 0x32, channel],
            rssi,
            channel,
            authmode,
            psk: if authmode == AUTH_OPEN {
                Vec::new()
            } else {
                b"scripted-key".to_vec()
            },
        }
    }

    #[test]
    fn a_record_is_the_92_byte_driver_abi() {
        let mut out = vec![0u8; 92];
        write_record(&ap("G2-Alpha", -42, 1, 3), &mut out);
        assert_eq!(&out[0..6], &[0x02, 0x00, 0x00, 0x47, 0x32, 0x01]);
        assert_eq!(&out[6..14], b"G2-Alpha");
        assert_eq!(out[14], 0, "the ssid is NUL terminated");
        assert_eq!(out[39], 1, "primary channel");
        assert_eq!(out[44] as i8, -42, "rssi");
        assert_eq!(out[48], 3, "authmode");
        assert!(
            out[52..].iter().all(|b| *b == 0),
            "unscripted fields stay 0"
        );
        assert!(
            !out.windows(12).any(|w| w == b"scripted-key"),
            "a key is never part of a scan record"
        );
    }

    #[test]
    fn records_come_back_strongest_first_and_without_a_key() {
        let world = [
            ap("G2-Charlie", -75, 11, 4),
            ap("G2-Alpha", -42, 1, 3),
            ap("G2-Bravo", -60, 6, 0),
        ];
        let found = heard(&world);
        let order: Vec<String> = found.iter().map(|a| a.ssid.clone()).collect();
        assert_eq!(order, ["G2-Alpha", "G2-Bravo", "G2-Charlie"]);
        assert!(
            found.iter().all(|ap| ap.psk.is_empty()),
            "a sweep result carries no key"
        );
        assert!(
            world.iter().any(|ap| !ap.psk.is_empty()),
            "the scripted world still has one"
        );
    }

    #[test]
    fn the_checks_are_the_ones_pemu_core_defines() {
        assert_eq!(ap("G2-Alpha", -42, 1, 3).check(), Ok(()));
        let mut off_channel = ap("G2-Alpha", -42, 1, 3);
        off_channel.channel = MAX_CHANNEL + 1;
        assert_eq!(off_channel.check(), Err(ApError::Channel));
        let mut bad_auth = ap("G2-Alpha", -42, 1, 3);
        bad_auth.authmode = MAX_AUTH + 1;
        assert_eq!(bad_auth.check(), Err(ApError::Auth));
    }
}
