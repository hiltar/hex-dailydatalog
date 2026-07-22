use chrono::{Duration, Local};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::thread;
use std::time::Duration as StdDuration;
use tiny_keccak::{Hasher, Keccak};

// ─── Constants ───────────────────────────────────────────────────────────────

const API_URL: &str = "http://192.168.68.100:5555/api/live-data";
const RPC_URL: &str = "https://rpc.pulsechain.com";
const HEX_CONTRACT: &str = "0x2b591e99afE9f32eAA6214f7B7629768c40Eeb39";
const DATA_FILE: &str = "daily_data.jsonl";
const GLOBAL_INFO_SELECTOR: &str = "0xf04b5fa0";
const DAILY_DATA_SLOT: u64 = 6;
const HEARTS_PER_HEX: f64 = 1e8;

// ─── API response (only the fields we need) ─────────────────────────────────

#[derive(Deserialize)]
struct ApiResponse {
    price_Pulsechain: f64,
    tshareRateHEX_Pulsechain: f64,
    payoutPerTshare_Pulsechain: f64,
}

// ─── JSON-RPC types ─────────────────────────────────────────────────────────

#[derive(Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'a str,
    method: &'a str,
    params: serde_json::Value,
    id: u8,
}

#[derive(Deserialize)]
struct RpcResponse {
    result: Option<String>,
}

// ─── Stored record (one JSONL line per day) ─────────────────────────────────

#[derive(Serialize)]
struct DayRecord {
    currentDay: u64,
    tshareRateHEX: f64,
    dailyPayoutHEX: f64,
    payoutPerTshareHEX: f64,
    pricePulseX: f64,
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn keccak256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Keccak::v256();
    let mut output = [0u8; 32];
    hasher.update(data);
    hasher.finalize(&mut output);
    output
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn rpc_call(method: &str, params: serde_json::Value) -> Result<String, String> {
    let body = RpcRequest {
        jsonrpc: "2.0",
        method,
        params,
        id: 1,
    };

    let resp: RpcResponse = ureq::post(RPC_URL)
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| format!("RPC HTTP: {e}"))?
        .into_json()
        .map_err(|e| format!("RPC decode: {e}"))?;

    resp.result
        .ok_or_else(|| "RPC returned null result".into())
}

fn get_current_day() -> Result<u64, String> {
    let params = serde_json::json!([{
        "to": HEX_CONTRACT,
        "data": GLOBAL_INFO_SELECTOR
    }, "latest"]);

    let raw = rpc_call("eth_call", params)?;
    let hex_str = raw.trim_start_matches("0x");

    if hex_str.len() < 320 {
        return Err(format!("globalInfo too short ({} chars)", hex_str.len()));
    }
    u64::from_str_radix(&hex_str[256..320], 16).map_err(|e| format!("parse day: {e}"))
}

fn get_daily_payout_hex(day: u64) -> Result<f64, String> {
    let mut preimage = [0u8; 64];
    preimage[24..32].copy_from_slice(&day.to_be_bytes());
    preimage[56..64].copy_from_slice(&DAILY_DATA_SLOT.to_be_bytes());
    let key_hex = format!("0x{}", hex_encode(&keccak256(&preimage)));

    let params = serde_json::json!([HEX_CONTRACT, key_hex, "latest"]);
    let raw = rpc_call("eth_getStorageAt", params)?;
    let hex_str = raw.trim_start_matches("0x");

    if hex_str.len() < 64 {
        return Err(format!("storage too short ({} chars)", hex_str.len()));
    }
    let payout_hearts = u128::from_str_radix(&hex_str[46..], 16)
        .map_err(|e| format!("parse payout: {e}"))?;

    Ok(payout_hearts as f64 / HEARTS_PER_HEX)
}

// ─── Main ────────────────────────────────────────────────────────────────────

fn main() {
    println!("[daily-logger] started");
    println!("[daily-logger] API : {API_URL}");
    println!("[daily-logger] RPC : {RPC_URL}");
    println!("[daily-logger] File: {DATA_FILE}");

    loop {
        let now = Local::now();
        let today_3am = now
            .date_naive()
            .and_hms_opt(3, 0, 0)
            .unwrap()
            .and_local_timezone(Local)
            .unwrap();
        let next_3am = if today_3am <= now {
            today_3am + Duration::days(1)
        } else {
            today_3am
        };

        let wait = (next_3am - now)
            .to_std()
            .unwrap_or(StdDuration::from_secs(60));
        println!(
            "[daily-logger] sleeping until {} ({}s)",
            next_3am.format("%Y-%m-%d %H:%M:%S"),
            wait.as_secs()
        );
        thread::sleep(wait);

        if let Err(e) = collect_and_save() {
            eprintln!("[daily-logger] ERROR: {e}");
        }
    }
}

fn collect_and_save() -> Result<(), String> {
    // 1) Local API → price, tshareRate, payoutPerTshare
    let api: ApiResponse = ureq::get(API_URL)
        .call()
        .map_err(|e| format!("API request: {e}"))?
        .into_json()
        .map_err(|e| format!("API JSON: {e}"))?;

    // 2) PulseChain RPC → currentDay
    let current_day = get_current_day()?;

    // 3) PulseChain RPC → dailyPayoutHEX (previous day's finalized data)
    let daily_payout = if current_day > 0 {
        get_daily_payout_hex(current_day - 1)?
    } else {
        0.0
    };

    // 4) Assemble & append
    let record = DayRecord {
        currentDay: current_day,
        tshareRateHEX: api.tshareRateHEX_Pulsechain,
        dailyPayoutHEX: daily_payout,
        payoutPerTshareHEX: api.payoutPerTshare_Pulsechain,
        pricePulseX: api.price_Pulsechain,
    };

    let line = serde_json::to_string(&record).map_err(|e| format!("serialize: {e}"))?;
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(DATA_FILE)
        .map_err(|e| format!("open {DATA_FILE}: {e}"))?;
    writeln!(file, "{line}").map_err(|e| format!("write: {e}"))?;

    println!(
        "[daily-logger] ✅ day {} | payout {:.8} HEX | tshare {:.1} | price {:.8}",
        record.currentDay, record.dailyPayoutHEX, record.tshareRateHEX, record.pricePulseX
    );
    Ok(())
}
