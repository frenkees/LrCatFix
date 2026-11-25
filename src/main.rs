//! LrCatFix - vstupní bod (PROJECT.md §11).
//!
//! Bez `--headless` se spustí GUI (dvoupanel, strom, tabulka nad výsledky
//! OPEN → EXTRACT → VERIFY → INDEX → MATCH, plus přepínač ZÁPIS POVOLEN pro
//! M3 APPLY). S `--headless` běží CLI: read-only report, nebo
//! `--apply-root-remap <id> [--confirm]` pro zápisovou APPLY session (§8).
//!
//! Zápis (ať CLI nebo GUI) vždy prochází `catalog::apply::apply` - záloha,
//! journal, two-phase commit s kompenzací. Bez `--apply-root-remap`/bez
//! zapnutého přepínače v GUI se do katalogu nikdy nesahá jinak než
//! `catalog::open::open_read_only` (`SQLITE_OPEN_READ_ONLY`).

mod app;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context, Result};
use serde::Serialize;

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

use lrcatfix::catalog::apply::{self, DbPhaseResult, Plan};
use lrcatfix::catalog::extract;
use lrcatfix::catalog::open;
use lrcatfix::catalog::schema;
use lrcatfix::cluster;
use lrcatfix::cluster::root_remap;
use lrcatfix::config;
use lrcatfix::fsops::verify::{self, FileStatus};
use lrcatfix::fsops::volumes;
use lrcatfix::index;
use lrcatfix::matching;

#[derive(Debug)]
struct Args {
    catalog: Option<PathBuf>,
    report: Option<PathBuf>,
    headless: bool,
    /// `--apply-root-remap <root_id>`: navrhované přemapování konkrétního
    /// kořene, které se má provést (vyžaduje `--headless`).
    apply_root_remap: Option<i64>,
    /// `--apply-root-remap` bez tohohle jen VYPÍŠE souhrn a nic nezapíše
    /// (CLAUDE.md §11: žádná nevratná operace bez explicitního potvrzení).
    confirm: bool,
    /// `--resume <session.json>`: GUI restart po UAC elevaci (§5.2) - načte
    /// `catalog_path` z předchozí (neelevované) session, aby uživatel po
    /// restartu neztratil rozdělanou práci. Platí jen bez `--headless`;
    /// pokud je zadaný i `--catalog`, ten má přednost.
    resume: Option<PathBuf>,
}

fn parse_args() -> Result<Args, String> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut catalog = None;
    let mut report = None;
    let mut headless = false;
    let mut apply_root_remap = None;
    let mut confirm = false;
    let mut resume = None;

    let mut iter = args;
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--catalog" => {
                let val = iter.next().ok_or("--catalog vyžaduje cestu ke katalogu")?;
                catalog = Some(PathBuf::from(val));
            }
            "--report" => {
                let val = iter
                    .next()
                    .ok_or("--report vyžaduje cestu k výstupnímu souboru")?;
                report = Some(PathBuf::from(val));
            }
            "--headless" => headless = true,
            "--apply-root-remap" => {
                let val = iter.next().ok_or("--apply-root-remap vyžaduje root_id")?;
                apply_root_remap = Some(
                    val.parse::<i64>()
                        .map_err(|_| format!("--apply-root-remap: '{val}' není celé číslo"))?,
                );
            }
            "--confirm" => confirm = true,
            "--resume" => {
                let val = iter
                    .next()
                    .ok_or("--resume vyžaduje cestu k session.json")?;
                resume = Some(PathBuf::from(val));
            }
            other => return Err(format!("neznámý argument: {other}")),
        }
    }

    if headless && catalog.is_none() {
        return Err("--headless vyžaduje --catalog <cesta>".to_string());
    }
    if !headless && report.is_some() {
        return Err("--report platí jen společně s --headless".to_string());
    }
    if apply_root_remap.is_some() && !headless {
        return Err("--apply-root-remap platí jen společně s --headless".to_string());
    }
    if resume.is_some() && headless {
        return Err("--resume platí jen bez --headless (GUI restart po UAC elevaci)".to_string());
    }

    Ok(Args {
        catalog,
        report,
        headless,
        apply_root_remap,
        confirm,
        resume,
    })
}

#[derive(Serialize)]
struct CandidateReport {
    path: PathBuf,
    score: i32,
}

#[derive(Serialize)]
struct MissingFileReport {
    file_id: i64,
    folder_missing: bool,
    expected_path: PathBuf,
    candidates: Vec<CandidateReport>,
}

#[derive(Serialize)]
struct RemapSuggestionReport {
    root_id: i64,
    original_absolute_path: String,
    proposed_absolute_path: String,
    matched_samples: usize,
    total_samples: usize,
}

#[derive(Serialize)]
struct Report {
    catalog: PathBuf,
    generated_at: String,
    total_files: usize,
    ok: usize,
    missing: usize,
    folder_missing: usize,
    root_remap_suggestions: Vec<RemapSuggestionReport>,
    missing_files: Vec<MissingFileReport>,
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("Chyba v argumentech: {msg}");
            eprintln!("Použití: lrcatfix [--catalog <cesta.lrcat>]           (spustí GUI)");
            eprintln!("         lrcatfix --catalog <cesta.lrcat> --headless [--report <out.json>]");
            eprintln!(
                "         lrcatfix --catalog <cesta.lrcat> --headless --apply-root-remap <id> [--confirm]"
            );
            return ExitCode::FAILURE;
        }
    };

    if args.headless {
        let Some(catalog) = args.catalog.as_ref() else {
            // `parse_args` už tohle ověřila (--headless vyžaduje --catalog),
            // takže sem se běh nikdy nedostane - ale `main` není konstanta
            // ani test, takže i tahle vždy-nedosažitelná větev dostane
            // hlášku a čistý FAILURE misto paniky.
            eprintln!("Interní chyba: --headless bez --catalog prošlo validací.");
            return ExitCode::FAILURE;
        };
        return match run_headless(
            catalog,
            args.report.as_deref(),
            args.apply_root_remap,
            args.confirm,
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("Chyba: {err:#}");
                ExitCode::FAILURE
            }
        };
    }

    let initial_catalog = args
        .catalog
        .or_else(|| resolve_resume_catalog(args.resume.as_deref()));
    run_gui(initial_catalog)
}

/// `--resume <session.json>` (§5.2): přečte katalog z předchozí session.
/// Chyba čtení (soubor chybí/poškozený) se jen zaloguje - GUI se stejně
/// spustí, jen prázdné, radši než aby restart po UAC skončil pádem.
fn resolve_resume_catalog(resume_path: Option<&Path>) -> Option<PathBuf> {
    let path = resume_path?;
    match lrcatfix::session::read(path) {
        Ok(session) => session.catalog_path,
        Err(err) => {
            tracing::warn!(%err, path = %path.display(), "nejde načíst --resume session, pokračuji bez ní");
            None
        }
    }
}

fn run_gui(initial_catalog: Option<PathBuf>) -> ExitCode {
    // Výchozí velikost `NativeOptions` je na tenhle layout malá - dvoupanel
    // + tabulka o šesti sloupcích + panel návrhů se do ní nevejdou a prvky
    // toolbaru končí za okrajem. `min_inner_size` navíc brání zmenšit okno
    // pod mez, kde by se panely překrývaly.
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1440.0, 900.0])
            .with_min_inner_size([1024.0, 640.0])
            .with_title("LrCatFix"),
        ..Default::default()
    };
    let result = eframe::run_native(
        "LrCatFix",
        native_options,
        Box::new(move |_cc| Ok(Box::new(app::LrCatFixApp::new(initial_catalog)))),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("Chyba GUI: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run_headless(
    catalog: &Path,
    report: Option<&Path>,
    apply_root_remap: Option<i64>,
    confirm: bool,
) -> Result<()> {
    if let Some(root_id) = apply_root_remap {
        apply_requested_remap(catalog, root_id, confirm)?;
        // Pokračuj běžným (read-only) průchodem níž - ten se znovu otevře od
        // začátku, takže VERIFY/MATCH už uvidí stav PO zápisu (PROJECT.md §8
        // fáze 4.2: „Znovu spusť VERIFY nad dotčenými soubory").
    }

    tracing::info!("otevírám katalog (read-only)");
    let handle = open::open_read_only(catalog).context("nejde otevřít katalog")?;

    if handle.lock_status.is_locked() {
        println!("Upozornění: katalog vypadá, že je otevřený v Lightroom Classic (nalezen zámkový soubor).");
        println!("Toto čtení je bezpečné (read-only), ale zápis by byl v tuto chvíli odmítnut.");
    }

    let roots = extract::list_root_folders(&handle.conn)
        .context("nejde načíst kořenové složky z katalogu")?;

    #[cfg(windows)]
    {
        let foreign = extract::foreign_roots(&roots);
        if !foreign.is_empty() {
            println!(
                "Katalog obsahuje {} kořenových složek s cestou, která nevypadá jako windows cesta \
                 (pravděpodobně katalog z macOS). Automatické dohledání souborů by nahlásilo skoro \
                 všechny fotky jako chybějící.",
                foreign.len()
            );
            println!("Ruční mapování kořenových složek zatím nástroj neumí (přijde v pozdějším milníku).");
            println!("Nalezené kořeny:");
            for r in &foreign {
                println!("  - id {}: {}", r.root_id, r.absolute_path);
            }
            bail!("katalog vyžaduje ruční mapování kořenových složek - zastaveno");
        }
    }

    let files = extract::extract(&handle.conn).context("EXTRACT selhalo")?;
    tracing::info!(count = files.len(), "EXTRACT hotovo");

    // Sonda svazků (§7.4.1) běží PŘED indexem; zbytek CLUSTER fáze až po
    // MATCHi, viz `cluster::detect_all` níž. Návrhy se tisknou až tam,
    // pohromadě se vzory A/B/C.
    let remap_suggestions = root_remap::probe_all(&roots, &files);

    let cfg = config::load();
    let io_threads = volumes::pick_io_thread_count(catalog, &cfg.scan);
    let results = verify::verify(&files, io_threads);
    let statuses: HashMap<i64, FileStatus> =
        results.iter().map(|r| (r.file_id, r.status)).collect();

    let ok = results
        .iter()
        .filter(|r| r.status == FileStatus::Ok)
        .count();
    let missing = results
        .iter()
        .filter(|r| r.status == FileStatus::Missing)
        .count();
    let folder_missing = results
        .iter()
        .filter(|r| r.status == FileStatus::FolderMissing)
        .count();
    let unavailable = results
        .iter()
        .filter(|r| r.status == FileStatus::FolderUnavailable)
        .count();

    tracing::info!(ok, missing, folder_missing, unavailable, "VERIFY hotovo");
    println!("Celkem souborů v katalogu: {}", files.len());
    println!("  V pořádku:            {ok}");
    println!("  Chybí (složka OK):     {missing}");
    println!("  Chybí (i celá složka): {folder_missing}");
    if unavailable > 0 {
        println!("  Na nedostupném disku:  {unavailable}");
        println!(
            "    ⚠ Tyto soubory NEJDE ověřit (odpojený disk / síť / oprávnění). Nepovažují se za\n\
             \x20     chybějící a nevstupují do návrhů oprav. Připojte disk a spusťte ověření znovu."
        );
    }

    let walk_roots = index::default_walk_roots(
        &roots
            .iter()
            .map(|r| r.absolute_path.clone())
            .collect::<Vec<_>>(),
    );
    let cancel = AtomicBool::new(false);
    let match_run = matching::run(
        &files,
        &statuses,
        &walk_roots,
        &cfg.r#match,
        &cfg.scan.exclude,
        &cancel,
        catalog,
    );

    let with_candidates = match_run
        .matches
        .values()
        .filter(|m| !m.candidates.is_empty())
        .count();
    let confident = match_run
        .matches
        .values()
        .filter(|m| {
            matching::score::is_confident(
                &m.candidates,
                cfg.r#match.auto_threshold,
                cfg.r#match.tie_margin,
            )
        })
        .count();
    println!(
        "MATCH: index {:?} ({} položek), {}/{} chybějících souborů má kandidáta, {} důvěryhodných shod",
        match_run.index_backend,
        match_run.index_size,
        with_candidates,
        missing + folder_missing,
        confident
    );
    if match_run.calibration.is_usable() {
        println!(
            "  Kalibrace času: OK ({:.0} % shody)",
            match_run.calibration.match_ratio * 100.0
        );
    } else {
        println!("  Kalibrace času: signál vypnutý (nedostatek/nekonzistence vzorků)");
    }

    print_score_diagnostics(&files, &statuses, &match_run, &cfg);

    // Fáze CLUSTER (§7) - sdílená s GUI přes `cluster::detect_all`.
    let members = if schema::has_collection_schema(&handle.conn) {
        extract::collection_membership(&handle.conn).unwrap_or_default()
    } else {
        Vec::new()
    };
    let clusters = cluster::detect_all(
        &roots,
        &files,
        &statuses,
        &match_run.matches,
        remap_suggestions,
        &members,
    );
    print_clusters(&clusters);

    if let Some(report_path) = report {
        let missing_files: Vec<MissingFileReport> = files
            .iter()
            .filter(|f| {
                statuses
                    .get(&f.file_id)
                    .copied()
                    .unwrap_or(FileStatus::Missing)
                    != FileStatus::Ok
            })
            .map(|f| MissingFileReport {
                file_id: f.file_id,
                folder_missing: statuses.get(&f.file_id) == Some(&FileStatus::FolderMissing),
                expected_path: f.expected_path.clone(),
                candidates: match_run
                    .matches
                    .get(&f.file_id)
                    .map(|m| {
                        m.candidates
                            .iter()
                            .map(|c| CandidateReport {
                                path: c.path.clone(),
                                score: c.score,
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            })
            .collect();

        let report_data = Report {
            catalog: catalog.to_path_buf(),
            generated_at: chrono::Utc::now().to_rfc3339(),
            total_files: files.len(),
            ok,
            missing,
            folder_missing,
            root_remap_suggestions: clusters
                .remap
                .iter()
                .map(|s| RemapSuggestionReport {
                    root_id: s.root_id,
                    original_absolute_path: s.original_absolute_path.clone(),
                    proposed_absolute_path: s.proposed_absolute_path.clone(),
                    matched_samples: s.matched_samples,
                    total_samples: s.total_samples,
                })
                .collect(),
            missing_files,
        };
        let json = serde_json::to_string_pretty(&report_data)
            .context("nejde serializovat report do JSON")?;
        std::fs::write(report_path, json)
            .with_context(|| format!("nejde zapsat report do {}", report_path.display()))?;
        println!("Report zapsán do {}", report_path.display());
    }

    Ok(())
}

/// Vypíše návrhy všech čtyř vzorů CLUSTER (§7). Dřív `--headless` tisklo
/// jen vzor D, takže to nejcennější (A/B/C) bylo vidět výhradně v GUI.
fn print_clusters(c: &cluster::ClusterResult) {
    if c.is_empty() {
        println!("\nCLUSTER: žádné vzory nedetekovány.");
        return;
    }
    println!("\nCLUSTER - návrhy:");

    for s in &c.remap {
        println!(
            "  [změna kořene] root {} '{}' -> '{}' (shoda {}/{} vzorků, {:.0} %)",
            s.root_id,
            s.original_absolute_path,
            s.proposed_absolute_path,
            s.matched_samples,
            s.total_samples,
            s.ratio() * 100.0
        );
    }
    for s in &c.folder_moved {
        println!(
            "  [přesunutá složka] '{}' -> '{}' ({}/{} souborů{})",
            cluster::display_source_path(&s.source_rel_path),
            s.target_dir.display(),
            s.matched_in_target,
            s.with_candidate,
            if s.target_name_differs {
                ", ⚠ jiný název cílové složky"
            } else {
                ""
            }
        );
    }
    for s in &c.scattered {
        println!(
            "  [rozsypaná složka] '{}': {} souborů v {} různých složkách (medián {:.1})",
            cluster::display_source_path(&s.source_rel_path),
            s.with_candidate,
            s.distinct_targets,
            s.median_per_target
        );
    }
    for s in &c.album_scattered {
        println!(
            "  [rozházené album] '{}': {}/{} chybí, ve {} složkách (jen detekce)",
            s.collection_name, s.missing_files, s.total_files, s.distinct_folders
        );
    }
}

/// Rozložení skóre a diagnostika, proč (ne)vznikly důvěryhodné shody.
///
/// Samotné „0 důvěryhodných shod" uživateli nic neřekne. Když je `md5`
/// v katalogu prázdné, je dosažitelné maximum 63 bodů a práh 70 se nedá
/// překonat - to je potřeba říct, ne nechat hádat (ověřeno na LrC 13.3, kde
/// `md5` bylo NULL u všech řádků).
fn print_score_diagnostics(
    files: &[extract::CatalogFile],
    statuses: &HashMap<i64, FileStatus>,
    match_run: &matching::MatchRun,
    cfg: &config::Config,
) {
    let mut buckets: std::collections::BTreeMap<i32, usize> = Default::default();
    for m in match_run.matches.values() {
        if let Some(best) = m.candidates.first() {
            *buckets.entry(best.score).or_default() += 1;
        }
    }
    if buckets.is_empty() {
        return;
    }

    println!(
        "  Rozložení nejlepšího skóre (práh pro důvěryhodnou shodu: {}):",
        cfg.r#match.auto_threshold
    );
    for (score, count) in buckets.iter().rev() {
        let mark = if *score >= cfg.r#match.auto_threshold as i32 {
            "≥ práh"
        } else {
            ""
        };
        println!("    {score:>4} bodů: {count:>5} souborů  {mark}");
    }

    let repairable = files
        .iter()
        .filter(|f| {
            statuses
                .get(&f.file_id)
                .copied()
                .unwrap_or(FileStatus::Missing)
                .is_repairable()
        })
        .count();
    let repairable_files = || {
        files
            .iter()
            .filter(|f| statuses.get(&f.file_id).is_some_and(|s| s.is_repairable()))
    };
    let with_md5 = repairable_files().filter(|f| f.md5.is_some()).count();
    let with_size = repairable_files().filter(|f| f.file_size.is_some()).count();

    if repairable > 0 && with_md5 == 0 {
        if with_size > 0 {
            println!(
                "  ℹ Sloupec md5 je u chybějících souborů prázdný, takže signál „MD5 shoda\"\n\
                 \x20   (+100) nikdy nenastane. Velikost souboru je ale k dispozici u {with_size}/{repairable}\n\
                 \x20   z nich (z importHash), takže signál „velikost odpovídá\" (+30) zůstává v hře\n\
                 \x20   a práh {} je dosažitelný i bez md5.",
                cfg.r#match.auto_threshold
            );
        } else {
            println!(
                "  ⚠ Katalog nemá u chybějících souborů ani md5, ani velikost (importHash).\n\
                 \x20   Dosažitelné maximum je pak 63 bodů (25 sourozenci, 25 čas, 8 podobnost\n\
                 \x20   jména, 5 známá složka) - tedy POD prahem {}. Auto-vyhodnocení se proto\n\
                 \x20   nespustí a všechno jde na ruční rozhodnutí. Není to chyba, ale je dobré\n\
                 \x20   o tom vědět.",
                cfg.r#match.auto_threshold
            );
        }
    }
}

/// `--apply-root-remap <root_id> [--confirm]`: najde odpovídající návrh
/// (§7.4), vypíše souhrn a BEZ `--confirm` skončí bez zápisu - CLAUDE.md
/// §11 vyžaduje souhrn před jakoukoli nevratnou operací i v GUI, totéž musí
/// platit pro CLI.
fn apply_requested_remap(catalog: &Path, root_id: i64, confirm: bool) -> Result<()> {
    let handle = open::open_read_only(catalog).context("nejde otevřít katalog")?;
    let roots = extract::list_root_folders(&handle.conn)
        .context("nejde načíst kořenové složky z katalogu")?;

    #[cfg(windows)]
    {
        let foreign = extract::foreign_roots(&roots);
        if !foreign.is_empty() {
            bail!(
                "katalog obsahuje kořenové složky, které nevypadají jako windows cesta - zastaveno"
            );
        }
    }

    let files = extract::extract(&handle.conn).context("EXTRACT selhalo")?;
    let suggestions = root_remap::probe_all(&roots, &files);
    // Uvolni read-only spojení PŘED zápisem - `apply::apply` si otevře
    // vlastní zápisové spojení a re-zkontroluje zámky (invariant 2).
    drop(handle);

    let suggestion = suggestions.into_iter().find(|s| s.root_id == root_id).ok_or_else(|| {
        anyhow::anyhow!("pro root {root_id} není žádný návrh na přemapování (buď neexistuje, nebo cesta funguje)")
    })?;

    println!("Návrh k potvrzení:");
    println!(
        "  Root {}: '{}' -> '{}' (shoda {}/{} vzorků, {:.0} %)",
        suggestion.root_id,
        suggestion.original_absolute_path,
        suggestion.proposed_absolute_path,
        suggestion.matched_samples,
        suggestion.total_samples,
        suggestion.ratio() * 100.0
    );
    println!(
        "  Co se stane: 1 řádek v databázi (AgLibraryRootFolder), 0 přesunů souborů na disku."
    );
    println!("  Před zápisem se automaticky vytvoří ověřená záloha katalogu.");

    if !confirm {
        println!("Zápis se NEPROVEDL. Pro provedení spusťte znovu se stejnými argumenty a navíc --confirm.");
        return Ok(());
    }

    // Pre-flight §8 bod 0.4 uvnitř `plan_root_remap` - mezi sondou a
    // potvrzením se disk mohl odpojit.
    let db_op = apply::plan_root_remap(suggestion.root_id, &suggestion.proposed_absolute_path)
        .context("pre-flight kontrola cílové cesty selhala")?;

    let mut journal_path = catalog.as_os_str().to_os_string();
    journal_path.push(".journal.jsonl");
    let session = format!("cli-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f"));
    let plan = Plan {
        fs_ops: vec![],
        db_ops: vec![db_op],
    };

    let outcome =
        apply::apply(catalog, Path::new(&journal_path), &session, plan).context("APPLY selhalo")?;
    match outcome.db_phase {
        DbPhaseResult::Committed => {
            println!("Zapsáno. Záloha: {}", outcome.backup.backup_dir.display());
            Ok(())
        }
        DbPhaseResult::RolledBack {
            reason,
            compensation_failures,
        } => {
            if compensation_failures.is_empty() {
                bail!("zápis selhal, katalog je nedotčený (vrácen zpět): {reason}");
            }
            bail!(
                "zápis selhal A kompenzace {} souborů se nepovedla - vyžaduje ruční kontrolu: {reason}",
                compensation_failures.len()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(strs: &[&str]) -> Result<Args, String> {
        parse_args_from(strs.iter().map(|s| s.to_string()))
    }

    #[test]
    fn apply_root_remap_requires_headless() {
        let err = args(&["--catalog", "x.lrcat", "--apply-root-remap", "1"]).unwrap_err();
        assert!(err.contains("--headless"));
    }

    #[test]
    fn apply_root_remap_parses_valid_id() {
        let parsed = args(&[
            "--catalog",
            "x.lrcat",
            "--headless",
            "--apply-root-remap",
            "42",
            "--confirm",
        ])
        .unwrap();
        assert_eq!(parsed.apply_root_remap, Some(42));
        assert!(parsed.confirm);
    }

    #[test]
    fn apply_root_remap_rejects_non_integer() {
        let err = args(&[
            "--catalog",
            "x.lrcat",
            "--headless",
            "--apply-root-remap",
            "nope",
        ])
        .unwrap_err();
        assert!(err.contains("není celé číslo"));
    }

    #[test]
    fn confirm_defaults_to_false() {
        let parsed = args(&["--catalog", "x.lrcat", "--headless"]).unwrap();
        assert!(!parsed.confirm);
        assert!(parsed.apply_root_remap.is_none());
    }

    #[test]
    fn headless_still_requires_catalog() {
        let err = args(&["--headless"]).unwrap_err();
        assert!(err.contains("--catalog"));
    }

    #[test]
    fn resume_parses_path() {
        let parsed = args(&["--resume", "session.json"]).unwrap();
        assert_eq!(parsed.resume, Some(PathBuf::from("session.json")));
    }

    #[test]
    fn resume_rejects_headless() {
        let err = args(&[
            "--catalog",
            "x.lrcat",
            "--headless",
            "--resume",
            "session.json",
        ])
        .unwrap_err();
        assert!(err.contains("--headless"));
    }

    #[test]
    fn resolve_resume_catalog_returns_none_for_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("neexistuje.json");
        assert!(resolve_resume_catalog(Some(&missing)).is_none());
    }

    #[test]
    fn resolve_resume_catalog_reads_saved_path() {
        let tmp = tempfile::tempdir().unwrap();
        let session_path = tmp.path().join("session.json");
        lrcatfix::session::write(
            &session_path,
            &lrcatfix::session::ResumeSession {
                catalog_path: Some(PathBuf::from(r"C:\Foto\Katalog.lrcat")),
            },
        )
        .unwrap();

        let resolved = resolve_resume_catalog(Some(&session_path));
        assert_eq!(resolved, Some(PathBuf::from(r"C:\Foto\Katalog.lrcat")));
    }
}
