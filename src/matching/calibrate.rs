//! Kalibrace `externalModTime` (PROJECT.md §5.4).
//!
//! Formát sloupce je mezi verzemi LrC neověřený (Unix epocha? Apple epocha
//! 2001-01-01? sekundy?). Místo hádání se runtime sám ověří proti
//! skutečnému `mtime` souborů, které fáze VERIFY označila jako existující -
//! přesně postup, který CLAUDE.md §6 žádá pro neověřené detaily schématu:
//! "napiš runtime check, který situaci detekuje". Když se korelace nepovede
//! (< 95 % vzorků), signál se úplně vypne - žádné dohady napůl.

use std::time::{SystemTime, UNIX_EPOCH};

const CORRELATION_THRESHOLD: f64 = 0.95;
/// Kalibrace je hrubší síto než finální skórovací tolerance (ta má vlastní
/// tabulku podle typu FS, viz [`super::score`]) - tady jen rozhodujeme MEZI
/// hypotézami o epoše/jednotce, ne o přesné shodě.
const CALIBRATION_TOLERANCE_SECONDS: f64 = 5.0;
/// Míň vzorků než tohle je málo na spolehlivé rozhodnutí - radši signál
/// vypnout (CLAUDE.md: "raději odmítnout, než tichý fallback").
const MIN_SAMPLES: usize = 5;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TimeEpoch {
    /// `externalModTime` je počet sekund od 1970-01-01 (Unix epocha).
    UnixSeconds,
    /// `externalModTime` je počet sekund od 2001-01-01 (Apple/Mac absolute time).
    AppleAbsoluteSeconds,
}

impl TimeEpoch {
    fn to_unix_seconds(self, raw: f64) -> f64 {
        match self {
            TimeEpoch::UnixSeconds => raw,
            // Rozdíl mezi 1970-01-01 a 2001-01-01 v sekundách.
            TimeEpoch::AppleAbsoluteSeconds => raw + 978_307_200.0,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Calibration {
    /// `None` = časový signál je pro tenhle katalog vypnutý (§5.4 bod 3).
    epoch: Option<TimeEpoch>,
    pub match_ratio: f64,
    pub sample_count: usize,
}

impl Calibration {
    pub fn disabled() -> Self {
        Self {
            epoch: None,
            match_ratio: 0.0,
            sample_count: 0,
        }
    }

    pub fn is_usable(&self) -> bool {
        self.epoch.is_some()
    }

    /// Převede syrovou hodnotu z katalogu na sekundy od Unix epochy podle
    /// zjištěné hypotézy. `None`, pokud kalibrace signál vypnula.
    pub fn to_unix_seconds(&self, raw: f64) -> Option<f64> {
        self.epoch.map(|e| e.to_unix_seconds(raw))
    }
}

/// `samples` = dvojice (syrová `externalModTime` hodnota z katalogu jako
/// `f64`, skutečný `mtime` souboru na disku) - jen ze souborů, které VERIFY
/// označila jako existující.
pub fn calibrate(samples: &[(f64, SystemTime)]) -> Calibration {
    if samples.len() < MIN_SAMPLES {
        return Calibration::disabled();
    }

    let mut best: Option<(TimeEpoch, f64)> = None;
    for epoch in [TimeEpoch::UnixSeconds, TimeEpoch::AppleAbsoluteSeconds] {
        let matched = samples
            .iter()
            .filter(|(raw, actual)| {
                let converted = epoch.to_unix_seconds(*raw);
                let Ok(actual_unix) = actual.duration_since(UNIX_EPOCH) else {
                    return false;
                };
                (converted - actual_unix.as_secs_f64()).abs() <= CALIBRATION_TOLERANCE_SECONDS
            })
            .count();
        let ratio = matched as f64 / samples.len() as f64;
        if best.is_none_or(|(_, best_ratio)| ratio > best_ratio) {
            best = Some((epoch, ratio));
        }
    }

    let Some((epoch, ratio)) = best else {
        return Calibration::disabled();
    };

    if ratio >= CORRELATION_THRESHOLD {
        Calibration {
            epoch: Some(epoch),
            match_ratio: ratio,
            sample_count: samples.len(),
        }
    } else {
        Calibration {
            epoch: None,
            match_ratio: ratio,
            sample_count: samples.len(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn unix_samples(count: usize) -> Vec<(f64, SystemTime)> {
        (0..count)
            .map(|i| {
                let mtime = UNIX_EPOCH + Duration::from_secs(1_600_000_000 + i as u64 * 3600);
                let raw = mtime.duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
                (raw, mtime)
            })
            .collect()
    }

    fn apple_samples(count: usize) -> Vec<(f64, SystemTime)> {
        (0..count)
            .map(|i| {
                let mtime = UNIX_EPOCH + Duration::from_secs(1_600_000_000 + i as u64 * 3600);
                let unix_secs = mtime.duration_since(UNIX_EPOCH).unwrap().as_secs_f64();
                let raw = unix_secs - 978_307_200.0;
                (raw, mtime)
            })
            .collect()
    }

    #[test]
    fn detects_unix_epoch() {
        let cal = calibrate(&unix_samples(20));
        assert!(cal.is_usable());
        assert_eq!(cal.to_unix_seconds(1_600_003_600.0), Some(1_600_003_600.0));
    }

    #[test]
    fn detects_apple_absolute_epoch() {
        let cal = calibrate(&apple_samples(20));
        assert!(cal.is_usable());
        // 0.0 v Apple epoše = 2001-01-01 = 978307200 v Unix epoše.
        assert_eq!(cal.to_unix_seconds(0.0), Some(978_307_200.0));
    }

    #[test]
    fn disables_signal_when_values_dont_correlate_with_anything() {
        let samples: Vec<(f64, SystemTime)> = (0..20)
            .map(|i| {
                (
                    i as f64 * 999.0,
                    UNIX_EPOCH + Duration::from_secs(1_600_000_000),
                )
            })
            .collect();
        let cal = calibrate(&samples);
        assert!(!cal.is_usable());
        assert_eq!(cal.to_unix_seconds(123.0), None);
    }

    #[test]
    fn too_few_samples_disables_signal_even_if_they_would_match() {
        let cal = calibrate(&unix_samples(3));
        assert!(!cal.is_usable());
    }

    #[test]
    fn partial_correlation_below_threshold_disables_signal() {
        let mut samples = unix_samples(10);
        // Zkaz polovinu vzorků - korelace klesne pod 95 %.
        for sample in samples.iter_mut().take(5) {
            sample.0 += 50_000.0;
        }
        let cal = calibrate(&samples);
        assert!(!cal.is_usable());
        assert!(cal.match_ratio < 0.95);
    }
}
