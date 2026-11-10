//! Growth against the WHO Child Growth Standards (weight-for-age), using the
//! WHO's LMS method. The table is read from disk at startup and is not part
//! of this repository; see data/README.md for where to get it.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sex {
    Female,
    Male,
}

#[derive(Debug, Clone, Copy)]
struct Lms {
    l: f64,
    m: f64,
    s: f64,
}

/// The weight-for-age table: LMS rows per sex, indexed by age in days.
pub struct Table {
    rows: [Vec<Lms>; 2], // male, female
}

impl Table {
    pub fn load(path: &Path) -> Result<Table, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Table::parse(&raw).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Tab-separated `sex age l m s` with a header line; sex 1 = male, 2 = female.
    pub fn parse(raw: &str) -> Result<Table, String> {
        let mut rows = [vec![], vec![]];
        for (n, line) in raw.lines().enumerate().skip(1) {
            if line.trim().is_empty() {
                continue;
            }
            let c: Vec<&str> = line.split('\t').collect();
            let bad = || format!("line {}: expected `sex age l m s`", n + 1);
            if c.len() != 5 {
                return Err(bad());
            }
            let sex: usize = c[0].parse().map_err(|_| bad())?;
            let age: usize = c[1].parse().map_err(|_| bad())?;
            let num = |i: usize| c[i].parse::<f64>().map_err(|_| bad());
            let v = rows.get_mut(sex.wrapping_sub(1)).ok_or_else(bad)?;
            if v.len() != age {
                return Err(format!("line {}: ages must run 0, 1, 2, … per sex", n + 1));
            }
            v.push(Lms { l: num(2)?, m: num(3)?, s: num(4)? });
        }
        if rows.iter().any(Vec::is_empty) {
            return Err("table needs rows for both sexes".into());
        }
        Ok(Table { rows })
    }

    fn lms(&self, sex: Sex, age_days: i64) -> Option<Lms> {
        let v = &self.rows[match sex {
            Sex::Male => 0,
            Sex::Female => 1,
        }];
        usize::try_from(age_days).ok().and_then(|a| v.get(a).copied())
    }

    /// Weight-for-age z-score, with the WHO's adjustment beyond ±3 SD.
    pub fn zscore(&self, sex: Sex, age_days: i64, kg: f64) -> Option<f64> {
        let p = self.lms(sex, age_days)?;
        if kg <= 0.0 {
            return None;
        }
        let z = if p.l == 0.0 { (kg / p.m).ln() / p.s } else { ((kg / p.m).powf(p.l) - 1.0) / (p.l * p.s) };
        Some(if z > 3.0 {
            let (sd2, sd3) = (at_z(p, 2.0), at_z(p, 3.0));
            3.0 + (kg - sd3) / (sd3 - sd2)
        } else if z < -3.0 {
            let (sd2, sd3) = (at_z(p, -2.0), at_z(p, -3.0));
            -3.0 + (kg - sd3) / (sd2 - sd3)
        } else {
            z
        })
    }

    /// Weight in grams at the given percentile (0–100) and age.
    pub fn grams_at_percentile(&self, sex: Sex, age_days: i64, percentile: f64) -> Option<f64> {
        Some(at_z(self.lms(sex, age_days)?, probit(percentile / 100.0)) * 1000.0)
    }

    /// For tests: the table from data/ if it has been downloaded there.
    #[cfg(test)]
    pub fn for_tests() -> Option<std::sync::Arc<Table>> {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/data/who-weight-for-age.tsv");
        match Table::load(Path::new(p)) {
            Ok(t) => Some(std::sync::Arc::new(t)),
            Err(_) => {
                eprintln!("note: {p} not found; skipping WHO percentile checks (see data/README.md)");
                None
            }
        }
    }
}

/// The weight (kg) at z-score `z` (LMS inverse; valid for |z| <= 3).
fn at_z(p: Lms, z: f64) -> f64 {
    if p.l == 0.0 { p.m * (p.s * z).exp() } else { p.m * (1.0 + p.l * p.s * z).powf(1.0 / p.l) }
}

/// Standard normal CDF, via erf (Abramowitz & Stegun 7.1.26; error < 1.5e-7).
pub fn percentile(z: f64) -> f64 {
    let x = z.abs() / std::f64::consts::SQRT_2;
    let t = 1.0 / (1.0 + 0.3275911 * x);
    let y = 1.0
        - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592)
            * t
            * (-x * x).exp();
    let cdf = if z >= 0.0 { 0.5 * (1.0 + y) } else { 0.5 * (1.0 - y) };
    cdf * 100.0
}

/// Inverse of the standard normal CDF (Acklam's rational approximation).
fn probit(p: f64) -> f64 {
    const A: [f64; 6] = [-3.969683028665376e1, 2.209460984245205e2, -2.759285104469687e2, 1.38357751867269e2, -3.066479806614716e1, 2.506628277459239];
    const B: [f64; 5] = [-5.447609879822406e1, 1.615858368580409e2, -1.556989798598866e2, 6.680131188771972e1, -1.328068155288572e1];
    const C: [f64; 6] = [-7.784894002430293e-3, -3.223964580411365e-1, -2.400758277161838, -2.549732539343734, 4.374664141464968, 2.938163982698783];
    const D: [f64; 4] = [7.784695709041462e-3, 3.224671290700398e-1, 2.445134137142996, 3.754408661907416];
    let p = p.clamp(1e-9, 1.0 - 1e-9);
    if p < 0.02425 {
        let q = (-2.0 * p.ln()).sqrt();
        (((((C[0] * q + C[1]) * q + C[2]) * q + C[3]) * q + C[4]) * q + C[5]) / ((((D[0] * q + D[1]) * q + D[2]) * q + D[3]) * q + 1.0)
    } else if p > 1.0 - 0.02425 {
        -probit(1.0 - p)
    } else {
        let q = p - 0.5;
        let r = q * q;
        (((((A[0] * r + A[1]) * r + A[2]) * r + A[3]) * r + A[4]) * r + A[5]) * q
            / (((((B[0] * r + B[1]) * r + B[2]) * r + B[3]) * r + B[4]) * r + 1.0)
    }
}

// ------------------------------------------------------------- weight change

/// Delivery mode, for the weight-change reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Delivery {
    Vaginal,
    Cesarean,
}

impl Delivery {
    pub fn describe(self) -> &'static str {
        match self {
            Delivery::Vaginal => "a vaginal birth",
            Delivery::Cesarean => "a C-section",
        }
    }
}

/// Percentiles of the percentage weight change since birth over the first
/// 30 days, per delivery mode (a local table; see data/README.md).
pub struct WeightChange {
    pub percentiles: Vec<f64>,
    rows: [Vec<(f64, Vec<f64>)>; 2], // vaginal, cesarean: (day, value per percentile)
}

impl WeightChange {
    pub fn load(path: &Path) -> Result<WeightChange, String> {
        let raw = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        WeightChange::parse(&raw).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Tab-separated, header `delivery day p5 p10 …`, days ascending.
    pub fn parse(raw: &str) -> Result<WeightChange, String> {
        let mut lines = raw.lines();
        let header: Vec<&str> = lines.next().ok_or("empty table")?.split('\t').collect();
        if header.len() < 4 || header[0] != "delivery" || header[1] != "day" {
            return Err("header must be `delivery day p5 p10 …`".into());
        }
        let percentiles = header[2..]
            .iter()
            .map(|h| h.strip_prefix('p').and_then(|n| n.parse::<f64>().ok()).ok_or(format!("bad column {h:?}")))
            .collect::<Result<Vec<_>, _>>()?;
        let mut rows = [vec![], vec![]];
        for (n, line) in lines.enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let c: Vec<&str> = line.split('\t').collect();
            let bad = || format!("line {}: bad row", n + 2);
            if c.len() != header.len() {
                return Err(bad());
            }
            let i = match c[0] {
                "vaginal" => 0,
                "cesarean" => 1,
                _ => return Err(bad()),
            };
            let day: f64 = c[1].parse().map_err(|_| bad())?;
            let vals = c[2..].iter().map(|v| v.parse::<f64>().map_err(|_| bad())).collect::<Result<Vec<_>, _>>()?;
            if rows[i].last().is_some_and(|(d, _): &(f64, Vec<f64>)| *d >= day) {
                return Err(format!("line {}: days must ascend", n + 2));
            }
            rows[i].push((day, vals));
        }
        if rows.iter().any(Vec::is_empty) {
            return Err("table needs rows for both delivery modes".into());
        }
        Ok(WeightChange { percentiles, rows })
    }

    fn rows(&self, d: Delivery) -> &[(f64, Vec<f64>)] {
        &self.rows[match d {
            Delivery::Vaginal => 0,
            Delivery::Cesarean => 1,
        }]
    }

    pub fn max_day(&self, d: Delivery) -> f64 {
        self.rows(d).last().map(|r| r.0).unwrap_or(0.0)
    }

    /// The percentile curves' values (in % change) at `day`, interpolated.
    pub fn at(&self, d: Delivery, day: f64) -> Option<Vec<f64>> {
        let rows = self.rows(d);
        if day < rows[0].0 || day > rows[rows.len() - 1].0 {
            return None;
        }
        let i = rows.partition_point(|r| r.0 <= day).saturating_sub(1).min(rows.len().saturating_sub(2));
        let (d0, v0) = &rows[i];
        let (d1, v1) = rows.get(i + 1).unwrap_or(&rows[i]);
        let f = if d1 > d0 { (day - d0) / (d1 - d0) } else { 0.0 };
        Some(v0.iter().zip(v1).map(|(a, b)| a + f * (b - a)).collect())
    }

    /// Percentile of a percentage weight change at `day`: interpolated between
    /// the curves on the normal (z) scale; clamped to the outermost curves.
    pub fn percentile(&self, d: Delivery, day: f64, change: f64) -> Option<f64> {
        let v = self.at(d, day)?;
        let z: Vec<f64> = self.percentiles.iter().map(|p| probit(p / 100.0)).collect();
        if change <= v[0] {
            return Some(self.percentiles[0]);
        }
        if change >= v[v.len() - 1] {
            return Some(self.percentiles[v.len() - 1]);
        }
        for k in 0..v.len() - 1 {
            if change >= v[k] && change <= v[k + 1] {
                let f = if v[k + 1] > v[k] { (change - v[k]) / (v[k + 1] - v[k]) } else { 0.5 };
                return Some(percentile(z[k] + f * (z[k + 1] - z[k])));
            }
        }
        None
    }

    /// Label for a weight-change percentile: the outermost curves are bounds.
    pub fn label(&self, p: f64) -> String {
        let (lo, hi) = (self.percentiles[0], self.percentiles[self.percentiles.len() - 1]);
        if p <= lo {
            format!("≤P{lo:.0}")
        } else if p >= hi {
            format!("≥P{hi:.0}")
        } else {
            format!("P{p:.0}")
        }
    }

    #[cfg(test)]
    pub fn for_tests() -> Option<std::sync::Arc<WeightChange>> {
        let p = concat!(env!("CARGO_MANIFEST_DIR"), "/data/newborn-weight-change.tsv");
        WeightChange::load(Path::new(p)).ok().map(std::sync::Arc::new)
    }
}

/// Age in completed days at Unix time `t`, for a birth at `birth`.
pub fn age_days(birth: i64, t: i64) -> i64 {
    (t - birth).div_euclid(86400)
}

/// "P31" style label; extremes shown as <P1 / >P99.
pub fn label(p: f64) -> String {
    if p < 1.0 {
        "<P1".into()
    } else if p > 99.0 {
        ">P99".into()
    } else {
        format!("P{:.0}", p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_errors() {
        assert!(Table::parse("sex\tage\tl\tm\ts\n1\t0\t0.3\t3.3\t0.14\n").is_err(), "one sex only");
        assert!(Table::parse("h\n1\t1\t0.3\t3.3\t0.14\n2\t0\t0.3\t3.2\t0.14\n").is_err(), "gap in ages");
        let t = Table::parse("h\n1\t0\t1\t3.0\t0.1\n2\t0\t1\t3.0\t0.1\n").unwrap();
        // L = 1: z = (w/M - 1)/S → 3.3 kg is z = 1.
        assert!((t.zscore(Sex::Female, 0, 3.3).unwrap() - 1.0).abs() < 1e-9);
        assert!(t.zscore(Sex::Female, 1, 3.3).is_none(), "beyond the table");
    }

    #[test]
    fn normal_distribution() {
        assert!((percentile(0.0) - 50.0).abs() < 1e-6);
        assert!((percentile(1.959964) - 97.5).abs() < 1e-3);
        assert!((percentile(-1.880794) - 3.0).abs() < 1e-3);
        for p in [3.0, 15.0, 50.0, 85.0, 97.0] {
            assert!((percentile(probit(p / 100.0)) - p).abs() < 1e-4, "{p}");
        }
    }

    #[test]
    fn weight_change_table() {
        let t = WeightChange::parse("delivery\tday\tp5\tp50\tp95\nvaginal\t0\t0\t0\t0\nvaginal\t2\t-10\t-6\t-2\ncesarean\t0\t0\t0\t0\ncesarean\t2\t-12\t-8\t-4\n").unwrap();
        assert_eq!(t.at(Delivery::Cesarean, 1.0).unwrap(), vec![-6.0, -4.0, -2.0]);
        assert!((t.percentile(Delivery::Cesarean, 2.0, -8.0).unwrap() - 50.0).abs() < 1e-6);
        assert_eq!(t.percentile(Delivery::Cesarean, 2.0, -20.0), Some(5.0));
        assert_eq!(t.label(5.0), "≤P5");
        assert!(t.at(Delivery::Vaginal, 3.0).is_none(), "beyond the table");
        assert!(WeightChange::parse("delivery\tday\tp50\nvaginal\t1\t0\nvaginal\t0\t0\ncesarean\t0\t0\n").is_err(), "days must ascend");
        // The digitised table, when present, matches the paper's text.
        let Some(t) = WeightChange::for_tests() else { return };
        let v = t.at(Delivery::Cesarean, 68.0 / 24.0).unwrap();
        assert!((v[3] - -7.1).abs() < 0.3, "C-section median nadir −7.1% at 68 h: {}", v[3]);
        assert!(t.at(Delivery::Cesarean, 10.0).unwrap()[3].abs() < 0.5, "half back at birth weight by day 10");
        let p = t.percentile(Delivery::Cesarean, 14.0, 0.0).unwrap();
        assert!((p - 24.0).abs() < 3.0, "24% not back by day 14: {p}");
        let p = t.percentile(Delivery::Cesarean, 21.0, 0.0).unwrap();
        assert!((p - 8.0).abs() < 2.0, "8% not back by day 21: {p}");
        let p = t.percentile(Delivery::Vaginal, 14.0, 0.0).unwrap();
        assert!((p - 14.0).abs() < 3.0, "14% (vaginal) not back by day 14: {p}");
    }

    #[test]
    fn who_values() {
        let Some(t) = Table::for_tests() else { return };
        // The median has z = 0.
        assert!(t.zscore(Sex::Female, 0, 3.2322).unwrap().abs() < 1e-9);
        assert!((t.grams_at_percentile(Sex::Female, 30, 50.0).unwrap() - 4171.6).abs() < 0.5);
        // A 4.0 kg girl at birth: z ≈ 1.567, about the 94th percentile.
        let z = t.zscore(Sex::Female, 0, 4.0).unwrap();
        assert!((z - 1.567).abs() < 0.005, "{z}");
        assert_eq!(label(percentile(z)), "P94");
        for p in [3.0, 15.0, 50.0, 85.0, 97.0] {
            let g = t.grams_at_percentile(Sex::Male, 100, p).unwrap();
            assert!((percentile(t.zscore(Sex::Male, 100, g / 1000.0).unwrap()) - p).abs() < 0.01);
        }
        // Beyond +3 SD the WHO uses the distance between +2 and +3 SD.
        let l = t.lms(Sex::Female, 0).unwrap();
        let (sd2, sd3) = (at_z(l, 2.0), at_z(l, 3.0));
        assert!((t.zscore(Sex::Female, 0, sd3 + (sd3 - sd2)).unwrap() - 4.0).abs() < 1e-9);
    }
}
