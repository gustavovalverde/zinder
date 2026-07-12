//! Minimal parser for the Prometheus text exposition the harness scrapes.
//!
//! Zinder emits simple label values (bare identifiers), so this parser handles
//! the subset actually rendered by the in-process recorder rather than the full
//! escaping grammar.

use std::collections::HashMap;

/// One parsed metric sample: a series name, its labels, and its float reading.
#[derive(Clone, Debug)]
pub struct MetricSample {
    /// Series name, including any `_sum`/`_count` suffix.
    pub name: String,
    /// Label key/value pairs attached to the series.
    pub labels: HashMap<String, String>,
    /// The numeric reading.
    pub reading: f64,
}

impl MetricSample {
    /// Returns the value of a label, when present.
    #[must_use]
    pub fn label(&self, key: &str) -> Option<&str> {
        self.labels.get(key).map(String::as_str)
    }
}

/// Parses a Prometheus text exposition into flat samples.
#[must_use]
pub fn parse_prometheus_samples(exposition: &str) -> Vec<MetricSample> {
    exposition.lines().filter_map(parse_line).collect()
}

fn parse_line(line: &str) -> Option<MetricSample> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return None;
    }
    let (series, reading_field) = trimmed.rsplit_once(char::is_whitespace)?;
    let reading = reading_field.trim().parse::<f64>().ok()?;
    let (name, labels) = split_series(series.trim());
    Some(MetricSample {
        name,
        labels,
        reading,
    })
}

fn split_series(series: &str) -> (String, HashMap<String, String>) {
    match series.split_once('{') {
        None => (series.to_owned(), HashMap::new()),
        Some((name, remainder)) => {
            let inner = remainder.strip_suffix('}').unwrap_or(remainder);
            (name.to_owned(), parse_labels(inner))
        }
    }
}

fn parse_labels(inner: &str) -> HashMap<String, String> {
    let mut labels = HashMap::new();
    for pair in inner.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        if let Some((key, raw)) = pair.split_once('=') {
            let unquoted = raw.trim().trim_matches('"');
            labels.insert(key.trim().to_owned(), unquoted.to_owned());
        }
    }
    labels
}

/// Sums the readings of every sample whose series name equals `name`.
#[must_use]
pub fn sum_by_name(samples: &[MetricSample], name: &str) -> f64 {
    samples
        .iter()
        .filter(|sample| sample.name == name)
        .map(|sample| sample.reading)
        .sum()
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{parse_prometheus_samples, sum_by_name};

    #[test]
    fn parses_labeled_counter_and_bare_gauge() -> Result<(), Box<dyn Error>> {
        let exposition = "\
# HELP something
zinder_store_multi_get_keys_total{table=\"transparent_output\",caller=\"block_prefetch\"} 42
bare_gauge 7
";
        let samples = parse_prometheus_samples(exposition);
        assert_eq!(samples.len(), 2);
        let labeled = samples
            .iter()
            .find(|sample| sample.name == "zinder_store_multi_get_keys_total")
            .ok_or("labeled sample present")?;
        assert_eq!(labeled.label("caller"), Some("block_prefetch"));
        assert!((labeled.reading - 42.0).abs() < f64::EPSILON);
        assert!((sum_by_name(&samples, "bare_gauge") - 7.0).abs() < f64::EPSILON);
        Ok(())
    }
}
