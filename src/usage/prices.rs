//! API list prices, cached so `crew usage` still works offline.
//!
//! The source is LiteLLM's published model table. A model that is not in it
//! stays unpriced; short names and `-build` suffixes are not guessed.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use serde::de::Deserializer;
use serde_json::Value;

use crate::job::store::base_dir;

const LITELLM_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
const MAX_AGE_SECS: u64 = 24 * 60 * 60;

#[derive(Clone, Debug)]
pub(crate) struct Rate {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Book {
    exact: BTreeMap<String, Rate>,
    bare: BTreeMap<String, Rate>,
}

impl Book {
    pub(crate) fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.bare.is_empty()
    }

    pub(crate) fn rate_for(&self, model: &str) -> Option<&Rate> {
        if let Some(rate) = self.exact.get(model) {
            return Some(rate);
        }
        let bare = model.rsplit('/').next().unwrap_or(model);
        if let Some(rate) = self.exact.get(bare) {
            return Some(rate);
        }
        self.bare.get(bare)
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct StoredRate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
}

impl From<&Rate> for StoredRate {
    fn from(rate: &Rate) -> Self {
        Self {
            input: rate.input,
            output: rate.output,
            cache_read: rate.cache_read,
            cache_write: rate.cache_write,
        }
    }
}

impl From<StoredRate> for Rate {
    fn from(rate: StoredRate) -> Self {
        Self {
            input: rate.input,
            output: rate.output,
            cache_read: rate.cache_read,
            cache_write: rate.cache_write,
        }
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PriceCache {
    fetched_at: u64,
    exact: BTreeMap<String, StoredRate>,
    bare: BTreeMap<String, StoredRate>,
}

#[derive(Deserialize)]
struct LitellmEntry {
    #[serde(default, deserialize_with = "loose_opt_string")]
    litellm_provider: Option<String>,
    #[serde(default, deserialize_with = "loose_opt_f64")]
    input_cost_per_token: Option<f64>,
    #[serde(default, deserialize_with = "loose_opt_f64")]
    output_cost_per_token: Option<f64>,
    #[serde(default, deserialize_with = "loose_opt_f64")]
    cache_read_input_token_cost: Option<f64>,
    #[serde(default, deserialize_with = "loose_opt_f64")]
    cache_creation_input_token_cost: Option<f64>,
}

fn loose_opt_f64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<f64>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(match value {
        Value::Null => None,
        Value::Number(number) => number.as_f64().filter(|v| v.is_finite() && *v >= 0.0),
        _ => None,
    })
}

fn loose_opt_string<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(match value {
        Value::String(text) => Some(text),
        _ => None,
    })
}

pub(crate) fn book() -> Book {
    let path = base_dir().join("model-prices.json");
    let cached = load_cache(&path);
    if let Some(ref cached) = cached {
        let age = now_secs().saturating_sub(cached.fetched_at);
        if age < MAX_AGE_SECS {
            return book_from_cache(cached);
        }
    }
    if let Some(fresh) = fetch_book() {
        let _ = save_cache(&path, &fresh);
        return fresh;
    }
    cached
        .map(|cached| book_from_cache(&cached))
        .unwrap_or_default()
}

fn book_from_cache(cache: &PriceCache) -> Book {
    Book {
        exact: cache
            .exact
            .iter()
            .map(|(key, rate)| (key.clone(), rate.clone().into()))
            .collect(),
        bare: cache
            .bare
            .iter()
            .map(|(key, rate)| (key.clone(), rate.clone().into()))
            .collect(),
    }
}

fn fetch_book() -> Option<Book> {
    let output = Command::new("curl")
        .arg("-fsS")
        .arg("-L")
        .arg("-m")
        .arg("30")
        .arg(LITELLM_URL)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    book_from_litellm(&output.stdout)
}

fn book_from_litellm(bytes: &[u8]) -> Option<Book> {
    let root: serde_json::Map<String, Value> = serde_json::from_slice(bytes).ok()?;
    let mut exact = BTreeMap::new();
    let mut bare = BTreeMap::new();
    let mut ranks: BTreeMap<String, u8> = BTreeMap::new();
    let mut bare_is_exact: BTreeMap<String, bool> = BTreeMap::new();
    for (key, value) in root {
        if key == "sample_spec" {
            continue;
        }
        let Value::Object(_) = value else {
            continue;
        };
        let Ok(entry) = serde_json::from_value::<LitellmEntry>(value) else {
            continue;
        };
        let Some(rate) = rate_from_entry(&entry) else {
            continue;
        };
        index_rate(
            &mut exact,
            &mut bare,
            &mut ranks,
            &mut bare_is_exact,
            &key,
            entry.litellm_provider.as_deref().unwrap_or(""),
            rate,
        );
    }
    if exact.is_empty() && bare.is_empty() {
        None
    } else {
        Some(Book { exact, bare })
    }
}

fn rate_from_entry(entry: &LitellmEntry) -> Option<Rate> {
    let input = entry.input_cost_per_token;
    let output = entry.output_cost_per_token;
    match (input, output) {
        (None, None) => return None,
        (Some(input), Some(output)) if input == 0.0 && output == 0.0 => return None,
        _ => {}
    }
    Some(Rate {
        input,
        output,
        cache_read: entry.cache_read_input_token_cost,
        cache_write: entry.cache_creation_input_token_cost,
    })
}

fn provider_rank(provider: &str) -> u8 {
    match provider {
        "anthropic" | "openai" | "xai" | "gemini" => 0,
        "vertex_ai" => 1,
        _ => 2,
    }
}

fn index_rate(
    exact: &mut BTreeMap<String, Rate>,
    bare: &mut BTreeMap<String, Rate>,
    ranks: &mut BTreeMap<String, u8>,
    bare_is_exact: &mut BTreeMap<String, bool>,
    key: &str,
    provider: &str,
    rate: Rate,
) {
    let rank = provider_rank(provider);
    let bare_name = key.rsplit('/').next().unwrap_or(key).to_string();
    let key_is_bare = bare_name == key;
    exact.insert(key.to_string(), rate.clone());
    let replace = match ranks.get(&bare_name) {
        None => true,
        Some(old) => {
            rank < *old
                || (rank == *old
                    && key_is_bare
                    && !bare_is_exact.get(&bare_name).copied().unwrap_or(false))
        }
    };
    if replace {
        ranks.insert(bare_name.clone(), rank);
        bare_is_exact.insert(bare_name.clone(), key_is_bare);
        bare.insert(bare_name, rate);
    }
}

fn cache_path_parent(path: &std::path::Path) -> Option<&std::path::Path> {
    path.parent()
}

fn save_cache(path: &std::path::Path, book: &Book) -> Result<(), ()> {
    let cache = PriceCache {
        fetched_at: now_secs(),
        exact: book
            .exact
            .iter()
            .map(|(key, rate)| (key.clone(), StoredRate::from(rate)))
            .collect(),
        bare: book
            .bare
            .iter()
            .map(|(key, rate)| (key.clone(), StoredRate::from(rate)))
            .collect(),
    };
    let bytes = serde_json::to_vec(&cache).map_err(|_| ())?;
    let parent = cache_path_parent(path).ok_or(())?;
    fs::create_dir_all(parent).map_err(|_| ())?;
    let tmp = path.with_extension("json.tmp");
    let mut file = fs::File::create(&tmp).map_err(|_| ())?;
    file.write_all(&bytes).map_err(|_| ())?;
    file.sync_all().map_err(|_| ())?;
    fs::rename(&tmp, path).map_err(|_| ())?;
    Ok(())
}

fn load_cache(path: &std::path::Path) -> Option<PriceCache> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Price one response. A missing rate leaves those tokens out of the dollar sum.
pub(crate) fn price_parts(
    rate: Option<&Rate>,
    fresh: u64,
    cache_read: u64,
    cache_write: u64,
    output: u64,
) -> (f64, u64, u64) {
    let Some(rate) = rate else {
        return (0.0, fresh + cache_read + cache_write + output, 0);
    };
    let mut usd = 0.0;
    let mut unpriced = 0u64;
    let mut priced = 0u64;
    apply(&mut usd, &mut unpriced, &mut priced, fresh, rate.input);
    apply(
        &mut usd,
        &mut unpriced,
        &mut priced,
        cache_read,
        rate.cache_read,
    );
    apply(
        &mut usd,
        &mut unpriced,
        &mut priced,
        cache_write,
        rate.cache_write,
    );
    apply(&mut usd, &mut unpriced, &mut priced, output, rate.output);
    (usd, unpriced, priced)
}

fn apply(usd: &mut f64, unpriced: &mut u64, priced: &mut u64, tokens: u64, rate: Option<f64>) {
    if tokens == 0 {
        return;
    }
    match rate {
        Some(rate) => {
            *usd += tokens as f64 * rate;
            *priced += tokens;
        }
        None => *unpriced += tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_book() -> Book {
        let mut exact = BTreeMap::new();
        let mut bare = BTreeMap::new();
        let mut ranks = BTreeMap::new();
        let mut bare_is_exact = BTreeMap::new();
        let rows = [
            ("azure/gpt-6-astra", "azure", 1.2e-5, 6e-5),
            ("gpt-6-astra", "openai", 1e-5, 5e-5),
            ("xai/grok-4.7", "xai", 2e-6, 6e-6),
        ];
        for (key, provider, input, output) in rows {
            index_rate(
                &mut exact,
                &mut bare,
                &mut ranks,
                &mut bare_is_exact,
                key,
                provider,
                Rate {
                    input: Some(input),
                    output: Some(output),
                    cache_read: None,
                    cache_write: None,
                },
            );
        }
        Book { exact, bare }
    }

    #[test]
    fn api_price_uses_table_rate_and_skips_unknown_model() {
        let haiku = Rate {
            input: Some(1e-7),
            output: Some(5e-7),
            cache_read: Some(1e-8),
            cache_write: None,
        };
        let (usd, unpriced, priced) = price_parts(Some(&haiku), 1_000_000, 1_000_000, 0, 1_000_000);
        assert!((usd - 0.61).abs() < 1e-9);
        assert_eq!(unpriced, 0);
        assert_eq!(priced, 3_000_000);

        let (usd, unpriced, priced) = price_parts(None, 10, 0, 0, 5);
        assert_eq!(usd, 0.0);
        assert_eq!(unpriced, 15);
        assert_eq!(priced, 0);

        let partial = Rate {
            input: Some(1e-6),
            output: Some(2e-6),
            cache_read: None,
            cache_write: None,
        };
        let (usd, unpriced, _) = price_parts(Some(&partial), 100, 40, 0, 0);
        assert!((usd - 0.0001).abs() < 1e-12);
        assert_eq!(unpriced, 40);

        let book = sample_book();
        assert_eq!(book.rate_for("gpt-6-astra").unwrap().input, Some(1e-5));
        assert_eq!(book.rate_for("grok-4.7").unwrap().input, Some(2e-6));
        assert!(book.rate_for("grok-4.7-build").is_none());
    }
}
