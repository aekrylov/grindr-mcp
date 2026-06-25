//! Minimal geohash encoding.
//!
//! Grindr identifies locations by a 12-character geohash (both for the location
//! your profile broadcasts and for browsing the grid). The app derives it from
//! GPS coordinates; this lets a caller pass plain latitude/longitude and get the
//! same 12-char hash without pulling in a geohash crate.

const BASE32: &[u8; 32] = b"0123456789bcdefghjkmnpqrstuvwxyz";

/// Encode `lat`/`lon` (degrees) into a geohash of `len` characters.
pub fn encode(lat: f64, lon: f64, len: usize) -> String {
    let mut lat_r = [-90.0f64, 90.0];
    let mut lon_r = [-180.0f64, 180.0];
    let bits = [16u8, 8, 4, 2, 1];
    let mut out = String::with_capacity(len);
    let mut bit = 0usize;
    let mut ch = 0u8;
    let mut even = true;

    while out.len() < len {
        if even {
            let mid = (lon_r[0] + lon_r[1]) / 2.0;
            if lon > mid {
                ch |= bits[bit];
                lon_r[0] = mid;
            } else {
                lon_r[1] = mid;
            }
        } else {
            let mid = (lat_r[0] + lat_r[1]) / 2.0;
            if lat > mid {
                ch |= bits[bit];
                lat_r[0] = mid;
            } else {
                lat_r[1] = mid;
            }
        }
        even = !even;
        if bit < 4 {
            bit += 1;
        } else {
            out.push(BASE32[ch as usize] as char);
            bit = 0;
            ch = 0;
        }
    }
    out
}

/// Whether `s` is a syntactically valid 12-character geohash.
pub fn is_valid(s: &str) -> bool {
    s.len() == 12 && s.bytes().all(|b| BASE32.contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_known_locations() {
        // Madrid, from the API docs' example region.
        assert!(encode(40.4168, -3.7038, 12).starts_with("ezjm"));
        assert_eq!(encode(40.4168, -3.7038, 12).len(), 12);
    }

    #[test]
    fn validates_length_and_charset() {
        assert!(is_valid("ezjmgyern222"));
        assert!(!is_valid("short"));
        assert!(!is_valid("aaaaaaaaaaaa")); // 'a' is not in the geohash alphabet
        assert!(!is_valid("ezjmgyern2222")); // 13 chars
    }
}
