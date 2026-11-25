//! `config.toml` (PROJECT.md §14).
//!
//! `[cloud]` přibude až s milníkem, který cloud placeholdery skutečně
//! rozezná (§5.1.1, M4+).
//!
//! Načítání `config.toml` ze souboru zatím NENÍ implementováno - vyžadovalo
//! by to crate pro TOML parsing, který není v PROJECT.md §4 na seznamu
//! povolených závislostí. Než se přidá, je potřeba se podle CLAUDE.md §8
//! zeptat uživatele. Do té doby se používají defaulty odpovídající §14.

#[derive(Debug, Clone)]
pub struct ScanConfig {
    pub threads_local: usize,
    pub threads_network: usize,
    /// Glob vzory (PROJECT.md §14) - složky/cesty vyloučené z INDEX fáze
    /// (§5.2). Bez `*` se pattern chová jako jméno segmentu cesty (např.
    /// `$RECYCLE.BIN` vyloučí tuhle složku kdekoli ve stromu), s `*` jako
    /// standardní wildcard nad celou (dopředně-lomítkovou) cestou.
    pub exclude: Vec<String>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            threads_local: 16,
            threads_network: 4,
            exclude: [
                "$RECYCLE.BIN",
                "System Volume Information",
                "*/*.lrdata/*",
                "*/Backups/*",
                "*/LrCatFix-backups/*",
                "*/Lightroom/*Backups*/*",
                "*/.Trash*/*",
                "*/Trash/*",
                "*/AppData/Local/Temp/*",
                "*/Adobe/CameraRaw/*",
                "*/node_modules/*",
                "*/.git/*",
            ]
            .into_iter()
            .map(str::to_string)
            .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MatchConfig {
    pub auto_threshold: u8,
    pub tie_margin: u8,
    pub folder_name_similarity: bool,
}

impl Default for MatchConfig {
    fn default() -> Self {
        Self {
            auto_threshold: 70,
            tie_margin: 15,
            folder_name_similarity: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SafetyConfig {
    /// Bez úspěšné zálohy se nikdy nezapisuje (CLAUDE.md invariant 1). Není
    /// to přepínatelné z UI - pole existuje jen proto, aby to bylo
    /// explicitní na jednom místě, ne zakopané v `apply.rs`.
    pub require_backup: bool,
    /// Aplikace startuje v režimu NÁHLED (PROJECT.md §2).
    pub dry_run_default: bool,
    /// Kolik operací nejvýš v jedné dávce APPLY (§8 „Dávkování").
    pub max_ops_per_batch: usize,
}

impl Default for SafetyConfig {
    fn default() -> Self {
        Self {
            require_backup: true,
            dry_run_default: true,
            max_ops_per_batch: 5000,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub scan: ScanConfig,
    pub r#match: MatchConfig,
    pub safety: SafetyConfig,
}

/// Vrátí konfiguraci s defaulty z PROJECT.md §14. Zatím nečte `config.toml`
/// (viz komentář výše u modulu).
pub fn load() -> Config {
    Config::default()
}
