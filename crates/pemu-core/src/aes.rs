//! The AES block cipher of NIST FIPS-197 (128- and 256-bit keys), shared by the SoC's AES
//! accelerator and the BLE controller's `HCI_LE_Encrypt`. Not constant time: it only runs on
//! data the guest already holds.

pub const BLOCK_BYTES: usize = 16;

/// Multiplication in GF(2^8) modulo x^8 + x^4 + x^3 + x + 1 (FIPS-197 section 4.2).
const fn gmul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    let mut i = 0;
    while i < 8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let high = a & 0x80;
        a <<= 1;
        if high != 0 {
            a ^= 0x1B;
        }
        b >>= 1;
        i += 1;
    }
    p
}

/// The S-box computed from its definition (FIPS-197 section 5.1.1): the GF(2^8) inverse, 0 to 0,
/// then the affine transform.
const fn build_sbox() -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut x = 0usize;
    while x < 256 {
        let mut inverse = 0u8;
        if x != 0 {
            let mut y = 1usize;
            while y < 256 {
                if gmul(x as u8, y as u8) == 1 {
                    inverse = y as u8;
                    break;
                }
                y += 1;
            }
        }
        table[x] = inverse
            ^ inverse.rotate_left(1)
            ^ inverse.rotate_left(2)
            ^ inverse.rotate_left(3)
            ^ inverse.rotate_left(4)
            ^ 0x63;
        x += 1;
    }
    table
}

const fn invert(table: [u8; 256]) -> [u8; 256] {
    let mut out = [0u8; 256];
    let mut i = 0usize;
    while i < 256 {
        out[table[i] as usize] = i as u8;
        i += 1;
    }
    out
}

pub const SBOX: [u8; 256] = build_sbox();
pub const INV_SBOX: [u8; 256] = invert(SBOX);
const RCON: [u8; 10] = [0x01, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40, 0x80, 0x1B, 0x36];

/// The expanded key: `4 (Nr + 1)` words of four bytes (FIPS-197 section 5.2).
pub fn expand_key(key: &[u8]) -> Vec<[u8; 4]> {
    let nk = key.len() / 4;
    let rounds = nk + 6;
    let mut w: Vec<[u8; 4]> = key.chunks(4).map(|c| [c[0], c[1], c[2], c[3]]).collect();
    for i in nk..4 * (rounds + 1) {
        let mut temp = w[i - 1];
        if i % nk == 0 {
            temp = [
                SBOX[temp[1] as usize] ^ RCON[i / nk - 1],
                SBOX[temp[2] as usize],
                SBOX[temp[3] as usize],
                SBOX[temp[0] as usize],
            ];
        } else if nk > 6 && i % nk == 4 {
            temp = temp.map(|b| SBOX[b as usize]);
        }
        let prev = w[i - nk];
        w.push([
            prev[0] ^ temp[0],
            prev[1] ^ temp[1],
            prev[2] ^ temp[2],
            prev[3] ^ temp[3],
        ]);
    }
    w
}

fn add_round_key(state: &mut [u8; BLOCK_BYTES], w: &[[u8; 4]], round: usize) {
    for c in 0..4 {
        for r in 0..4 {
            state[4 * c + r] ^= w[round * 4 + c][r];
        }
    }
}

fn shift_rows(state: &mut [u8; BLOCK_BYTES]) {
    let from = *state;
    for c in 0..4 {
        for r in 1..4 {
            state[4 * c + r] = from[4 * ((c + r) % 4) + r];
        }
    }
}

fn inv_shift_rows(state: &mut [u8; BLOCK_BYTES]) {
    let from = *state;
    for c in 0..4 {
        for r in 1..4 {
            state[4 * c + r] = from[4 * ((c + 4 - r) % 4) + r];
        }
    }
}

fn mix_columns(state: &mut [u8; BLOCK_BYTES], coefficients: [u8; 4]) {
    for c in 0..4 {
        let a = [
            state[4 * c],
            state[4 * c + 1],
            state[4 * c + 2],
            state[4 * c + 3],
        ];
        for r in 0..4 {
            state[4 * c + r] = (0..4)
                .map(|i| gmul(a[i], coefficients[(4 + i - r) % 4]))
                .fold(0, |acc, byte| acc ^ byte);
        }
    }
}

const MIX: [u8; 4] = [0x02, 0x03, 0x01, 0x01];
const INV_MIX: [u8; 4] = [0x0E, 0x0B, 0x0D, 0x09];

/// One block through the cipher (FIPS-197 section 5.1).
pub fn encrypt_block(state: &mut [u8; BLOCK_BYTES], w: &[[u8; 4]]) {
    let rounds = w.len() / 4 - 1;
    add_round_key(state, w, 0);
    for round in 1..=rounds {
        for byte in state.iter_mut() {
            *byte = SBOX[*byte as usize];
        }
        shift_rows(state);
        if round != rounds {
            mix_columns(state, MIX);
        }
        add_round_key(state, w, round);
    }
}

/// One block through the inverse cipher (FIPS-197 section 5.3).
pub fn decrypt_block(state: &mut [u8; BLOCK_BYTES], w: &[[u8; 4]]) {
    let rounds = w.len() / 4 - 1;
    add_round_key(state, w, rounds);
    for round in (0..rounds).rev() {
        inv_shift_rows(state);
        for byte in state.iter_mut() {
            *byte = INV_SBOX[*byte as usize];
        }
        add_round_key(state, w, round);
        if round != 0 {
            mix_columns(state, INV_MIX);
        }
    }
}

/// AES-128 of one block, key and block in FIPS-197 byte order.
pub fn encrypt_128(key: &[u8; 16], input: &[u8; BLOCK_BYTES]) -> [u8; BLOCK_BYTES] {
    let mut state = *input;
    encrypt_block(&mut state, &expand_key(key));
    state
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anchors from the published S-box table (FIPS-197 section 5.1.1) check the generator.
    #[test]
    fn the_generated_sbox_matches_the_published_table_at_its_anchors() {
        assert_eq!(SBOX[0x00], 0x63);
        assert_eq!(SBOX[0x01], 0x7C);
        assert_eq!(SBOX[0x53], 0xED);
        assert_eq!(SBOX[0x7F], 0xD2);
        assert_eq!(SBOX[0xFF], 0x16);
        let mut seen = [false; 256];
        for value in SBOX {
            assert!(!seen[value as usize], "{value:#04X} appears twice");
            seen[value as usize] = true;
        }
        for (i, value) in SBOX.iter().enumerate() {
            assert_eq!(INV_SBOX[*value as usize] as usize, i);
        }
    }

    /// FIPS-197 Appendix C.1 (AES-128) and C.3 (AES-256), both directions.
    #[test]
    fn the_fips_197_appendix_c_vectors() {
        let plain: [u8; 16] = core::array::from_fn(|i| (i as u8) * 0x11);
        let key128: [u8; 16] = core::array::from_fn(|i| i as u8);
        let want128 = [
            0x69, 0xC4, 0xE0, 0xD8, 0x6A, 0x7B, 0x04, 0x30, 0xD8, 0xCD, 0xB7, 0x80, 0x70, 0xB4,
            0xC5, 0x5A,
        ];
        assert_eq!(encrypt_128(&key128, &plain), want128);
        let mut back = want128;
        decrypt_block(&mut back, &expand_key(&key128));
        assert_eq!(back, plain);

        let key256: [u8; 32] = core::array::from_fn(|i| i as u8);
        let want256 = [
            0x8E, 0xA2, 0xB7, 0xCA, 0x51, 0x67, 0x45, 0xBF, 0xEA, 0xFC, 0x49, 0x90, 0x4B, 0x49,
            0x60, 0x89,
        ];
        let w = expand_key(&key256);
        let mut block = plain;
        encrypt_block(&mut block, &w);
        assert_eq!(block, want256);
        decrypt_block(&mut block, &w);
        assert_eq!(block, plain);
    }
}
