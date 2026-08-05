//! The hindsight comparison-JSONL universe, mirrored from `cow_scan.py`.
//!
//! Both stage bins (`stage2`, `stage3`) must run APEX on exactly the analytic scan's headline
//! universe — same canonicalization, quarantine, USD estimation, and routable restriction — or
//! their ceiling comparisons are meaningless. This module is that one shared mirror.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
};

use anyhow::{Context, Result};
use apex_solver::types::{Address as ApexAddress, U256};

pub const WETH: &str = "0x4200000000000000000000000000000000000006";
pub const NATIVE: &str = "0x0000000000000000000000000000000000000000";
pub const PRICE_DEV_FACTOR: f64 = 5.0;
pub const USD_CAP: f64 = 10_000_000.0;
/// Sender-verified self-trading pair (cow_scan WASH_PAIRS); its orders never enter APEX but the
/// pair's volume stays in the intent denominator, mirroring the analytic scan.
pub const WASH_PAIR: (&str, &str) =
    ("0x3c5cd672b204ba0fc48e93b98c0922920a87912d", "0x3d66e6fe9a3cf698db5af3d70830b299c9235151");

/// One netted intent from the headline universe, in raw native units.
#[derive(Clone)]
pub struct Intent {
    pub block: u64,
    pub token_in: ApexAddress,
    pub token_out: ApexAddress,
    pub amount_in: U256,
    pub settled_out: U256,
    pub usd: f64,
    pub id: String,
    pub is_wash: bool,
    /// Fynd's own N−1 quote for this trade (`top.fynd_amount_out`, raw `token_out` units) —
    /// the commercial baseline apex clearings are compared against, present only when fynd
    /// solved the trade.
    pub fynd_out: Option<U256>,
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Operates on bytes, not `str` indices: a `&str` byte length can equal 40 while containing a
/// multi-byte UTF-8 char, and slicing at a non-boundary byte index panics.
pub fn parse_address(token: &str) -> Option<ApexAddress> {
    let hex = token.strip_prefix("0x")?.as_bytes();
    if hex.len() != 40 {
        return None;
    }
    let mut bytes = [0u8; 20];
    for (i, byte) in bytes.iter_mut().enumerate() {
        let high = hex_digit(hex[2 * i])?;
        let low = hex_digit(hex[2 * i + 1])?;
        *byte = (high << 4) | low;
    }
    Some(ApexAddress(bytes))
}

pub fn parse_u256_decimal(digits: &str) -> Option<U256> {
    let mut value = U256::ZERO;
    let ten = U256::from(10u64);
    for c in digits.bytes() {
        if !c.is_ascii_digit() {
            return None;
        }
        value = value
            .checked_mul(ten)?
            .checked_add(U256::from((c - b'0') as u64))?;
    }
    Some(value)
}

pub fn u256_to_f64(value: U256) -> f64 {
    let mut result = 0.0f64;
    for limb in value.as_limbs().iter().rev() {
        result = result * 1.8446744073709552e19 + *limb as f64;
    }
    result
}

/// Mirror of cow_scan's `load_day` + `classify`: canonicalize, quarantine, USD-estimate, and
/// keep the headline (both-tokens-routable) slice. Returns intents plus the day's per-token
/// median price in USD per RAW UNIT — the same decimals-free price the analytic scan uses.
pub fn load_day_headline(path: &Path) -> Result<(Vec<Intent>, HashMap<ApexAddress, f64>)> {
    let wash_a = parse_address(WASH_PAIR.0).expect("static wash address parses");
    let wash_b = parse_address(WASH_PAIR.1).expect("static wash address parses");
    let weth = parse_address(WETH).expect("static WETH address parses");

    struct Raw {
        block: u64,
        token_in: ApexAddress,
        token_out: ApexAddress,
        amount_in: U256,
        settled_out: U256,
        usd: Option<f64>,
        routable_pair: bool,
        id: String,
        fynd_out: Option<U256>,
    }

    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut raws: Vec<Raw> = Vec::new();
    let mut price_samples: HashMap<ApexAddress, Vec<f64>> = HashMap::new();
    let mut routable: HashSet<ApexAddress> = Default::default();

    for line in content.lines() {
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let canon = |t: &str| {
            if t == NATIVE {
                weth
            } else {
                parse_address(t).unwrap_or(ApexAddress([0xFF; 20]))
            }
        };
        let (Some(tin_s), Some(tout_s)) = (rec["token_in"].as_str(), rec["token_out"].as_str())
        else {
            continue;
        };
        let (tin, tout) = (canon(tin_s), canon(tout_s));
        if tin == tout || tin.0 == [0xFF; 20] || tout.0 == [0xFF; 20] {
            continue;
        }
        let (Some(ain), Some(aout)) = (
            rec["amount_in"]
                .as_str()
                .and_then(parse_u256_decimal),
            rec["settled_amount_out"]
                .as_str()
                .and_then(parse_u256_decimal),
        ) else {
            continue;
        };
        if ain.is_zero() || aout.is_zero() {
            continue;
        }
        let verdict = rec["top"]["verdict"]
            .as_str()
            .unwrap_or("");
        let usd = rec["top"]["settled_value_usd"].as_f64();
        if verdict == "win" || verdict == "loss" {
            routable.insert(tin);
            routable.insert(tout);
        }
        if let Some(u) = usd {
            if u > 0.0 {
                price_samples
                    .entry(tout)
                    .or_default()
                    .push(u / u256_to_f64(aout));
                price_samples
                    .entry(tin)
                    .or_default()
                    .push(u / u256_to_f64(ain));
            }
        }
        let (Some(tx), Some(tx_index)) = (rec["settled_tx"].as_str(), rec["tx_index"].as_u64())
        else {
            continue;
        };
        raws.push(Raw {
            block: rec["block"].as_u64().unwrap_or(0),
            token_in: tin,
            token_out: tout,
            amount_in: ain,
            settled_out: aout,
            usd,
            routable_pair: false,
            id: format!("{tx}:{tx_index}"),
            fynd_out: rec["top"]["fynd_amount_out"]
                .as_str()
                .and_then(parse_u256_decimal),
        });
    }

    let mut day_price: HashMap<ApexAddress, f64> = HashMap::new();
    for (token, mut samples) in price_samples {
        samples.sort_by(f64::total_cmp);
        day_price.insert(token, samples[samples.len() / 2]);
    }

    let mut intents = Vec::new();
    for mut raw in raws {
        raw.routable_pair = routable.contains(&raw.token_in) && routable.contains(&raw.token_out);
        let pin = day_price.get(&raw.token_in).copied();
        let pout = day_price.get(&raw.token_out).copied();
        let usd_est = raw
            .usd
            .or_else(|| pin.map(|p| p * u256_to_f64(raw.amount_in)))
            .or_else(|| pout.map(|p| p * u256_to_f64(raw.settled_out)));
        let Some(usd_est) = usd_est else { continue };
        let mut bad = usd_est > USD_CAP;
        if !bad {
            if let Some(pin) = pin {
                if pin > 0.0 {
                    let dev = (usd_est / u256_to_f64(raw.amount_in)) / pin;
                    bad = !(1.0 / PRICE_DEV_FACTOR..=PRICE_DEV_FACTOR).contains(&dev);
                }
            }
        }
        if !bad {
            if let Some(pout) = pout {
                if pout > 0.0 {
                    let dev = (usd_est / u256_to_f64(raw.settled_out)) / pout;
                    bad = !(1.0 / PRICE_DEV_FACTOR..=PRICE_DEV_FACTOR).contains(&dev);
                }
            }
        }
        if bad || !raw.routable_pair {
            continue;
        }
        let pair = if raw.token_in.0 < raw.token_out.0 {
            (raw.token_in, raw.token_out)
        } else {
            (raw.token_out, raw.token_in)
        };
        intents.push(Intent {
            block: raw.block,
            token_in: raw.token_in,
            token_out: raw.token_out,
            amount_in: raw.amount_in,
            settled_out: raw.settled_out,
            usd: usd_est,
            id: raw.id,
            is_wash: pair == (wash_a, wash_b) || pair == (wash_b, wash_a),
            fynd_out: raw.fynd_out,
        });
    }
    Ok((intents, day_price))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    fn token_hex(byte: u8) -> String {
        format!("0x{}", format!("{byte:02x}").repeat(20))
    }

    fn token_addr(byte: u8) -> ApexAddress {
        ApexAddress([byte; 20])
    }

    #[allow(clippy::too_many_arguments)]
    fn record_line(
        block: u64,
        token_in: &str,
        token_out: &str,
        amount_in: &str,
        settled_out: &str,
        tx: &str,
        tx_index: u64,
        verdict: &str,
        usd: Option<f64>,
    ) -> String {
        let mut top = serde_json::json!({
            "verdict": verdict,
            "fynd_amount_out": settled_out,
        });
        if let Some(usd) = usd {
            top["settled_value_usd"] = serde_json::json!(usd);
        }
        serde_json::json!({
            "block": block,
            "token_in": token_in,
            "token_out": token_out,
            "amount_in": amount_in,
            "settled_amount_out": settled_out,
            "settled_tx": tx,
            "tx_index": tx_index,
            "top": top,
        })
        .to_string()
    }

    static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A JSONL fixture in the OS temp dir, removed on drop. No `tempfile` dev-dependency exists
    /// in this crate yet, so the uniqueness (pid + counter + nanos) is hand-rolled.
    struct TempJsonl(std::path::PathBuf);

    impl TempJsonl {
        fn new(name: &str, lines: &[String]) -> Self {
            let n = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "apex_batch_dataset_test_{name}_{}_{n}_{nanos}.jsonl",
                std::process::id()
            ));
            std::fs::write(&path, lines.join("\n")).expect("write temp jsonl fixture");
            Self(path)
        }
    }

    impl std::ops::Deref for TempJsonl {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempJsonl {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[test]
    fn test_load_day_headline_skips_malformed_json_line() {
        let valid1 = record_line(
            100,
            &token_hex(0x01),
            &token_hex(0x02),
            "1000000000000000000",
            "2000000000",
            "0xaaa",
            0,
            "win",
            Some(2000.0),
        );
        let valid2 = record_line(
            101,
            &token_hex(0x03),
            &token_hex(0x04),
            "1000000000000000000",
            "3000000000",
            "0xbbb",
            1,
            "win",
            Some(3000.0),
        );
        let fixture = TempJsonl::new("malformed", &[valid1, "not valid json".to_string(), valid2]);

        let (intents, _) = load_day_headline(&fixture).expect("load succeeds");

        assert_eq!(intents.len(), 2, "malformed line must be skipped, both valids kept");
        let ids: Vec<_> = intents
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        assert!(ids.contains(&"0xaaa:0"));
        assert!(ids.contains(&"0xbbb:1"));
    }

    #[test]
    fn test_load_day_headline_wash_pair_flagged_both_orders() {
        let forward = record_line(
            100,
            WASH_PAIR.0,
            WASH_PAIR.1,
            "1000000000000000000",
            "1000000000000000000",
            "0xw1",
            0,
            "win",
            Some(100.0),
        );
        let reverse = record_line(
            101,
            WASH_PAIR.1,
            WASH_PAIR.0,
            "1000000000000000000",
            "1000000000000000000",
            "0xw2",
            1,
            "win",
            Some(100.0),
        );
        let fixture = TempJsonl::new("wash_pair", &[forward, reverse]);

        let (intents, _) = load_day_headline(&fixture).expect("load succeeds");

        assert_eq!(intents.len(), 2);
        assert!(intents.iter().all(|i| i.is_wash), "both address orders must flag as wash");
    }

    #[test]
    fn test_parse_address_valid() {
        let addr = parse_address("0x1234567890123456789012345678901234567890").unwrap();
        assert_eq!(
            addr.0,
            [
                0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78, 0x90, 0x12, 0x34, 0x56, 0x78,
                0x90, 0x12, 0x34, 0x56, 0x78, 0x90
            ]
        );
    }

    #[test]
    fn test_parse_address_rejects_empty() {
        assert_eq!(parse_address(""), None);
    }

    #[test]
    fn test_parse_address_rejects_non_hex() {
        let non_hex = format!("0x{}", "zz".repeat(20));
        assert_eq!(parse_address(&non_hex), None);
    }

    #[test]
    fn test_parse_address_rejects_wrong_length() {
        assert_eq!(parse_address(&format!("0x{}", "ab".repeat(19))), None);
        assert_eq!(parse_address(&format!("0x{}", "ab".repeat(21))), None);
    }

    #[test]
    fn test_parse_address_rejects_multibyte_utf8_without_panic() {
        // 19 ASCII bytes + 2-byte 'é' + 19 ASCII bytes = 40 bytes, but only 39 chars: a
        // byte-length check alone would let this through, and str-indexing at the wrong byte
        // offset panics on the char boundary. Must return None, not panic.
        let hex_with_multibyte = format!("0x{}é{}", "0".repeat(19), "0".repeat(19));
        assert_eq!(parse_address(&hex_with_multibyte), None);
    }

    #[test]
    fn test_parse_u256_decimal_valid() {
        assert_eq!(parse_u256_decimal("12345"), Some(U256::from(12345u64)));
    }

    #[test]
    fn test_parse_u256_decimal_empty() {
        assert_eq!(parse_u256_decimal(""), Some(U256::ZERO));
    }

    #[test]
    fn test_parse_u256_decimal_rejects_non_digit() {
        assert_eq!(parse_u256_decimal("12a45"), None);
        assert_eq!(parse_u256_decimal("-123"), None);
    }

    #[test]
    fn test_load_day_headline_canonicalizes_native_to_weth() {
        let line = record_line(
            100,
            NATIVE,
            &token_hex(0x05),
            "1000000000000000000",
            "500000000",
            "0xnat",
            0,
            "win",
            Some(1000.0),
        );
        let fixture = TempJsonl::new("native_canon", &[line]);

        let (intents, _) = load_day_headline(&fixture).expect("load succeeds");

        assert_eq!(intents.len(), 1);
        assert_eq!(intents[0].token_in, parse_address(WETH).unwrap());
    }

    #[test]
    fn test_load_day_headline_price_deviation_boundary_kept_at_factor() {
        // Two baseline trades pin token_x's day-median price at exactly 100.0 (two identical
        // samples keep the median stable no matter where the boundary trade's own sample lands).
        // The boundary trade's usd/amount_in ratio is exactly PRICE_DEV_FACTOR × 100.0, which
        // the inclusive `..=` range keeps.
        let baseline1 = record_line(
            100,
            &token_hex(0x07),
            &token_hex(0x08),
            "1",
            "1",
            "0xbase1",
            0,
            "win",
            Some(100.0),
        );
        let baseline2 = record_line(
            101,
            &token_hex(0x07),
            &token_hex(0x09),
            "1",
            "1",
            "0xbase2",
            1,
            "win",
            Some(100.0),
        );
        let boundary = record_line(
            102,
            &token_hex(0x07),
            &token_hex(0x0a),
            "1",
            "1",
            "0xk",
            2,
            "win",
            Some(100.0 * PRICE_DEV_FACTOR),
        );
        let fixture = TempJsonl::new("dev_kept", &[baseline1, baseline2, boundary]);

        let (intents, day_price) = load_day_headline(&fixture).expect("load succeeds");

        assert_eq!(day_price[&token_addr(0x07)], 100.0);
        let ids: Vec<_> = intents
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        assert!(ids.contains(&"0xk:2"), "exactly at the factor must be kept: {ids:?}");
    }

    #[test]
    fn test_load_day_headline_price_deviation_boundary_excluded_beyond_factor() {
        let baseline1 = record_line(
            100,
            &token_hex(0x07),
            &token_hex(0x08),
            "1",
            "1",
            "0xbase1",
            0,
            "win",
            Some(100.0),
        );
        let baseline2 = record_line(
            101,
            &token_hex(0x07),
            &token_hex(0x09),
            "1",
            "1",
            "0xbase2",
            1,
            "win",
            Some(100.0),
        );
        let boundary = record_line(
            102,
            &token_hex(0x07),
            &token_hex(0x0a),
            "1",
            "1",
            "0xe",
            2,
            "win",
            Some(100.0 * PRICE_DEV_FACTOR + 1.0),
        );
        let fixture = TempJsonl::new("dev_excluded", &[baseline1, baseline2, boundary]);

        let (intents, day_price) = load_day_headline(&fixture).expect("load succeeds");

        assert_eq!(day_price[&token_addr(0x07)], 100.0);
        let ids: Vec<_> = intents
            .iter()
            .map(|i| i.id.as_str())
            .collect();
        assert!(!ids.contains(&"0xe:2"), "just beyond the factor must be excluded: {ids:?}");
        assert_eq!(intents.len(), 2, "only the two baseline trades survive");
    }

    #[test]
    fn test_u256_to_f64_large_value_rounds_without_panic() {
        let value = U256::from(u64::MAX);
        let result = u256_to_f64(value);
        assert!(result.is_finite());
        let expected = u64::MAX as f64;
        assert!((result - expected).abs() / expected < 1e-9);
    }
}
