//! Style keys. A style is shared as its canonical JSON under a key every peer computes alike:
//! FNV-1a 64 of that JSON, as 16 lowercase hex digits. If `styles` already holds the key with a
//! different style, the writer uses a minted key `m<id>` instead.

use gridcraft_model::Style;
use serde_json::Value as Json;

use crate::ids::Id;

/// The hash a style key is made of: bytes to 64 bits.
pub type StyleHash = fn(&[u8]) -> u64;

/// FNV-1a, 64 bit.
#[must_use]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3))
}

/// `v` printed with object keys in sorted order and no spaces: equal styles give equal text.
#[must_use]
pub fn canonical(v: &Json) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Json, out: &mut String) {
    match v {
        Json::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Json::String(k.clone()).to_string());
                out.push(':');
                if let Some(x) = m.get(k) {
                    write_canonical(x, out);
                }
            }
            out.push('}');
        }
        Json::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(x, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

/// The style as shared JSON.
#[must_use]
pub fn to_json(s: &Style) -> Json {
    serde_json::to_value(s).unwrap_or(Json::Null)
}

/// The style a shared JSON stands for, when it reads.
#[must_use]
pub fn from_json(v: &Json) -> Option<Style> {
    serde_json::from_value(v.clone()).ok()
}

/// The key `canonical_json` gets under `hash`.
#[must_use]
pub fn key_of(canonical_json: &str, hash: StyleHash) -> String {
    format!("{:016x}", hash(canonical_json.as_bytes()))
}

/// A minted key for a style whose hash key is taken by another style.
#[must_use]
pub fn minted_key(id: Id) -> String {
    format!("m{}", id.text())
}
