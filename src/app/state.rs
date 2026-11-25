//! `AppState` - jediný zdroj pravdy pro UI (CLAUDE.md §5: „`app/` nesmí
//! obsahovat SQL ani přímé I/O. Volá jen funkce z `catalog/`, `fsops/`,
//! `index/`.").
//!
//! Načtení katalogu (OPEN → EXTRACT → VERIFY → sonda root remapu) běží na
//! pozadí ve `std::thread`, aby UI vlákno nikdy nezamrzlo (CLAUDE.md
//! „Souběžnost": „Dlouhé operace běží mimo UI vlákno"). Výsledek se předává
//! přes `mpsc` kanál, ne sdíleným mutexem přes celý stav.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use lrcatfix::catalog::apply::{self, DbOpTemplate, DbPhaseResult, ManualResolveError, Plan};
use lrcatfix::catalog::extract::{self, CatalogFile, RootFolder};
use lrcatfix::catalog::open::{self, LockStatus};
use lrcatfix::catalog::schema;
use lrcatfix::cluster;
use lrcatfix::cluster::album_scattered::AlbumScatteredSuggestion;
use lrcatfix::cluster::folder_moved::{self, FolderMovedSuggestion};
use lrcatfix::cluster::root_remap::{self, RemapSuggestion};
use lrcatfix::cluster::scattered::{self, ScatteredSuggestion};
use lrcatfix::config;
use lrcatfix::fsops::paths::match_key_str;
use lrcatfix::fsops::verify::{self, FileStatus};
use lrcatfix::fsops::volumes;
use lrcatfix::index;
use lrcatfix::matching::{self, score::FileMatches, MatchRun};

use crate::app::dialogs::duplicates::{self, DuplicatePickerState};
use crate::app::panel_tree::folder_key;

/// Výsledek úspěšného načtení katalogu - vše, co panely potřebují k vykreslení.
pub struct LoadedCatalog {
    pub catalog_path: PathBuf,
    pub lock_status: LockStatus,
    pub roots: Vec<RootFolder>,
    pub files: Vec<CatalogFile>,
    pub statuses: HashMap<i64, FileStatus>,
    pub remap_suggestions: Vec<RemapSuggestion>,
    /// Vzor A (§7.1) - vyhodnocený AŽ PO `remap_suggestions` (§7.4 "Pořadí
    /// vyhodnocení: D → A → B → C").
    pub folder_moved_suggestions: Vec<FolderMovedSuggestion>,
    /// Vzor B (§7.2) - detekce + konsolidace (M6).
    pub scattered_suggestions: Vec<ScatteredSuggestion>,
    /// Vzor C (§7.3) - jen detekce; prázdné, když katalog nemá schéma pro
    /// kolekce (`catalog::schema::has_collection_schema`). Fyzické sloučení
    /// vědomě mimo rozsah M6 - viz `cluster::album_scattered` modul.
    pub album_scattered_suggestions: Vec<AlbumScatteredSuggestion>,
    pub match_run: MatchRun,
}

impl LoadedCatalog {
    pub fn status_of(&self, file_id: i64) -> FileStatus {
        self.statuses
            .get(&file_id)
            .copied()
            .unwrap_or(FileStatus::Missing)
    }

    pub fn matches_of(&self, file_id: i64) -> Option<&FileMatches> {
        self.match_run.matches.get(&file_id)
    }
}

type LoadOutcome = Result<LoadedCatalog, String>;

#[derive(Default)]
pub struct AppState {
    pub catalog_path_input: String,
    pub loaded: Option<LoadedCatalog>,
    pub loading: bool,
    pub error: Option<String>,
    /// `true`, pokud proces běží s elevovaným tokenem (§5.2) - backend B
    /// (MFT scan) je dostupný jen tehdy. Zjištěno jednou při startu
    /// (`AppState::new`), token se za běhu neumí změnit.
    pub elevated: bool,
    /// Klíč vybrané složky ve stromu (viz `panel_tree::FolderNode::key`).
    /// `None` = zobrazit soubory ze všech složek.
    pub selected_folder_key: Option<String>,
    pub filter_text: String,
    pub only_unresolved: bool,
    receiver: Option<mpsc::Receiver<LoadOutcome>>,
    /// Sdílené s vláknem na pozadí - tlačítko "Zrušit" ho nastaví na `true`,
    /// INDEX fáze (walk backend) ho kontroluje mezi položkami (CLAUDE.md
    /// „Souběžnost": uživatel musí umět zastavit skenování).
    cancel: Option<Arc<AtomicBool>>,

    /// Zápisový režim (CLAUDE.md §11: NÁHLED vs. ZÁPIS POVOLEN musí být na
    /// první pohled jasné). `false` = žádné tlačítko nesmí nic zapsat.
    pub write_mode: bool,
    /// Návrh čekající na potvrzení v modálním dialogu - nastaví ho panel
    /// návrhů, vykreslí a potvrdí/zruší `app::mod`.
    pub pending_remap: Option<RemapSuggestion>,
    /// Vzor A (§7.1) čekající na potvrzení - stejný vzor jako `pending_remap`.
    pub pending_folder_repoint: Option<FolderMovedSuggestion>,
    /// Vzor A, druhá možnost (§7.1 bod 2: "Přesun souborů zpět") - čeká na
    /// potvrzení stejně jako `pending_folder_repoint`.
    pub pending_folder_move_back: Option<FolderMovedSuggestion>,
    /// Vzor B (§7.2, "Konsolidace") - čeká na potvrzení stejně jako
    /// `pending_folder_move_back`.
    pub pending_consolidate: Option<ScatteredSuggestion>,
    pub applying: bool,
    pub apply_error: Option<String>,
    pub apply_success: Option<String>,
    apply_receiver: Option<mpsc::Receiver<Result<ApplySummary, String>>>,

    /// Otevřený dialog duplikátů (PROJECT.md §6.2) - `None`, dokud uživatel
    /// neklikne na "Vyřešit" u nějakého řádku.
    pub pending_duplicate_picker: Option<DuplicatePickerState>,
    /// Ruční rozhodnutí z dialogu duplikátů - `file_id -> zvolená cesta
    /// kandidáta`. Čeká na potvrzení přes "Použít vyřešené", teprve pak se
    /// zapíše (CLAUDE.md §11: žádný jednoklik bez souhrnu).
    pub resolved_choices: HashMap<i64, PathBuf>,
    /// Soubory, které uživatel v dialogu vyřešit odmítl ("Přeskočit"/
    /// "Přeskočit vše") - nezobrazují se znovu jako kandidáti na "Vyřešit",
    /// dokud se katalog znovu nenačte.
    pub skipped_files: HashSet<i64>,
    /// "Použít stejnou volbu pro zbytek souborů ze stejné složky" (§6.2) -
    /// `folder_key -> cílová složka` vybraného kandidáta, pro informaci v UI.
    pub sticky_folder_choice: HashMap<String, PathBuf>,
    /// Přepínač modálního potvrzovacího dialogu pro zápis `resolved_choices`
    /// - stejný vzor jako `pending_remap`.
    pub confirm_apply_resolved: bool,
}

pub struct ApplySummary {
    pub message: String,
    pub catalog_path: PathBuf,
}

impl AppState {
    pub fn new(initial_catalog: Option<PathBuf>) -> Self {
        let mut state = Self {
            elevated: index::mft::is_process_elevated(),
            ..Self::default()
        };
        if let Some(path) = initial_catalog {
            state.catalog_path_input = path.display().to_string();
            state.start_load(path);
        }
        state
    }

    /// "Rychlý sken disku (vyžaduje správce) 🛡" (§5.2): uloží minimální
    /// session (jen cesta ke katalogu, §9 `session.rs`) a restartuje
    /// aplikaci s právy správce (`fsops::shell::relaunch_elevated`, verb
    /// `runas`) - JEDINÉ místo, kde nástroj o elevaci vůbec žádá, a jen na
    /// tohle explicitní kliknutí, nikdy automaticky. Když restart uspěje,
    /// ukončí TENHLE (neelevovaný) proces - o okno se dál stará ten nový.
    pub fn relaunch_elevated(&mut self) {
        let Ok(exe) = std::env::current_exe() else {
            self.error = Some("nejde zjistit cestu k vlastnímu spustitelnému souboru".to_string());
            return;
        };

        let catalog_path = self
            .loaded
            .as_ref()
            .map(|l| l.catalog_path.clone())
            .or_else(|| {
                let trimmed = self.catalog_path_input.trim();
                (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
            });

        let session_path = std::env::temp_dir().join("lrcatfix-resume-session.json");
        let session = lrcatfix::session::ResumeSession { catalog_path };
        if let Err(e) = lrcatfix::session::write(&session_path, &session) {
            self.error = Some(format!("nejde zapsat session pro restart: {e}"));
            return;
        }

        let args = vec!["--resume".to_string(), session_path.display().to_string()];
        match lrcatfix::fsops::shell::relaunch_elevated(&exe, &args) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                self.error = Some(format!("restart s právy správce se nepovedl: {e}"));
            }
        }
    }

    /// Spustí načtení katalogu na pozadí. Nezruší předchozí `loaded` stav,
    /// dokud nepřijde výsledek - uživatel mezitím dál vidí poslední data.
    pub fn start_load(&mut self, path: PathBuf) {
        self.loading = true;
        self.error = None;
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        std::thread::spawn(move || {
            let outcome = load_catalog(&path, &cancel);
            // Přijímací konec může být pryč, pokud uživatel mezitím zavřel
            // okno - poslání do zavřeného kanálu prostě není kam doručit.
            let _ = tx.send(outcome);
        });
    }

    pub fn cancel_loading(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Zavolat jednou za frame. Vrací `true`, pokud se stav právě změnil
    /// (typicky signál pro `ctx.request_repaint()` z volajícího).
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.receiver else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(loaded)) => {
                self.loaded = Some(loaded);
                self.loading = false;
                self.error = None;
                self.receiver = None;
                self.cancel = None;
                self.selected_folder_key = None;
                // Přechodné hlášky o zápisu patří k PŘEDCHOZÍMU stavu dat -
                // jakmile se načtou čerstvá data, ustoupí běžnému souhrnu.
                self.apply_success = None;
                self.apply_error = None;
                // Ruční rozhodnutí z dialogu duplikátů se vážou na konkrétní
                // stav VERIFY/MATCH - po znovunačtení (typicky právě proto,
                // že se něco zapsalo) by dál skrývala řádky podle už
                // neplatného stavu.
                self.pending_duplicate_picker = None;
                self.resolved_choices.clear();
                self.skipped_files.clear();
                self.sticky_folder_choice.clear();
                self.confirm_apply_resolved = false;
                true
            }
            Ok(Err(msg)) => {
                self.error = Some(msg);
                self.loading = false;
                self.receiver = None;
                self.cancel = None;
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                // Vlákno spadlo bez odeslání výsledku (panic) - nenech UI
                // navěky točit "načítám", ale ohlas to jako chybu.
                self.error = Some("načítání katalogu neočekávaně skončilo".to_string());
                self.loading = false;
                self.receiver = None;
                self.cancel = None;
                true
            }
        }
    }

    /// Spustí APPLY pro potvrzený návrh přemapování kořene na pozadí -
    /// zápis (i jen jednořádkový UPDATE) zahrnuje zálohu, takže může chvíli
    /// trvat a UI vlákno nesmí zamrznout (CLAUDE.md „Souběžnost").
    pub fn start_apply_root_remap(&mut self, suggestion: RemapSuggestion) {
        let Some(loaded) = &self.loaded else { return };
        let catalog_path = loaded.catalog_path.clone();
        self.applying = true;
        self.apply_error = None;
        self.apply_success = None;

        let (tx, rx) = mpsc::channel();
        self.apply_receiver = Some(rx);

        std::thread::spawn(move || {
            let outcome = apply_root_remap(&catalog_path, suggestion);
            let _ = tx.send(outcome);
        });
    }

    /// Spustí APPLY pro potvrzený návrh vzoru A (§7.1) na pozadí - stejný
    /// vzor jako [`Self::start_apply_root_remap`].
    pub fn start_apply_folder_repoint(&mut self, suggestion: FolderMovedSuggestion) {
        let Some(loaded) = &self.loaded else { return };
        let catalog_path = loaded.catalog_path.clone();
        let roots = loaded.roots.clone();
        let known_folders = apply::known_folders_by_path(&loaded.files);
        let files_in_folder: Vec<i64> = loaded
            .files
            .iter()
            .filter(|f| f.folder_id == suggestion.folder_id)
            .map(|f| f.file_id)
            .collect();

        self.applying = true;
        self.apply_error = None;
        self.apply_success = None;

        let (tx, rx) = mpsc::channel();
        self.apply_receiver = Some(rx);

        std::thread::spawn(move || {
            let outcome = apply_folder_repoint(
                &catalog_path,
                &suggestion,
                &roots,
                &known_folders,
                &files_in_folder,
            );
            let _ = tx.send(outcome);
        });
    }

    /// Spustí APPLY pro vzor A, druhou možnost (§7.1 bod 2, "Přesun souborů
    /// zpět") na pozadí - stejný vzor jako [`Self::start_apply_folder_repoint`].
    pub fn start_apply_folder_move_back(&mut self, suggestion: FolderMovedSuggestion) {
        let Some(loaded) = &self.loaded else { return };
        let catalog_path = loaded.catalog_path.clone();
        let files = loaded.files.clone();
        let matches = loaded.match_run.matches.clone();

        self.applying = true;
        self.apply_error = None;
        self.apply_success = None;

        let (tx, rx) = mpsc::channel();
        self.apply_receiver = Some(rx);

        std::thread::spawn(move || {
            let outcome = apply_folder_move_back(&catalog_path, &suggestion, &files, &matches);
            let _ = tx.send(outcome);
        });
    }

    /// Spustí APPLY pro vzor B (§7.2, "Konsolidace") na pozadí - stejný vzor
    /// jako [`Self::start_apply_folder_move_back`].
    pub fn start_apply_consolidate(&mut self, suggestion: ScatteredSuggestion) {
        let Some(loaded) = &self.loaded else { return };
        let catalog_path = loaded.catalog_path.clone();
        let files = loaded.files.clone();
        let matches = loaded.match_run.matches.clone();

        self.applying = true;
        self.apply_error = None;
        self.apply_success = None;

        let (tx, rx) = mpsc::channel();
        self.apply_receiver = Some(rx);

        std::thread::spawn(move || {
            let outcome = apply_consolidate(&catalog_path, &suggestion, &files, &matches);
            let _ = tx.send(outcome);
        });
    }

    /// Zavolat jednou za frame vedle [`Self::poll`]. Po úspěšném zápisu
    /// automaticky znovu spustí načtení katalogu (§8 fáze 4.2: „Znovu spusť
    /// VERIFY nad dotčenými soubory a ukaž výsledek").
    pub fn poll_apply(&mut self) -> bool {
        let Some(rx) = &self.apply_receiver else {
            return false;
        };
        match rx.try_recv() {
            Ok(Ok(summary)) => {
                self.applying = false;
                self.apply_receiver = None;
                self.apply_success = Some(summary.message);
                self.start_load(summary.catalog_path);
                true
            }
            Ok(Err(msg)) => {
                self.applying = false;
                self.apply_receiver = None;
                self.apply_error = Some(msg);
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.applying = false;
                self.apply_receiver = None;
                self.apply_error = Some("zápis neočekávaně skončil".to_string());
                true
            }
        }
    }

    /// Otevře dialog duplikátů (§6.2) pro jeden soubor - "Vyřešit" v
    /// `panel_files`. Beze změny, pokud soubor nemá žádného MATCH kandidáta
    /// (nedává smysl otevírat prázdný dialog).
    pub fn open_duplicate_picker(&mut self, file_id: i64) {
        let Some(loaded) = &self.loaded else { return };
        let Some(file) = loaded.files.iter().find(|f| f.file_id == file_id) else {
            return;
        };
        let Some(matches) = loaded.matches_of(file_id) else {
            return;
        };
        if matches.candidates.is_empty() {
            return;
        }
        let key = folder_key(file.root_id, &file.rel_path);
        self.pending_duplicate_picker = Some(duplicates::build(file, &matches.candidates, key));
    }

    /// Zpracuje akci vrácenou z [`duplicates::show`] a podle ní aktualizuje
    /// `resolved_choices`/`skipped_files`. Volající (`app::mod`) předává
    /// `picker`, který právě dialogu vzal (`Option::take`) - `handle_*`
    /// ho zpátky do `state` nevrací, jen podle `action` použije jeho data.
    pub fn handle_duplicate_picker_action(
        &mut self,
        picker: duplicates::DuplicatePickerState,
        action: duplicates::DialogAction,
    ) {
        match action {
            duplicates::DialogAction::UseSelected {
                path,
                apply_to_rest_of_folder,
            } => {
                self.resolved_choices.insert(picker.file_id, path.clone());
                self.skipped_files.remove(&picker.file_id);
                if apply_to_rest_of_folder {
                    self.apply_sticky_choice_to_folder(&picker.folder_key, &path);
                }
            }
            duplicates::DialogAction::Skip {
                apply_to_rest_of_folder,
            } => {
                self.skipped_files.insert(picker.file_id);
                if apply_to_rest_of_folder {
                    self.skip_rest_of_folder(&picker.folder_key);
                }
            }
            duplicates::DialogAction::SkipAll => {
                self.skipped_files.insert(picker.file_id);
                self.skip_rest_of_folder(&picker.folder_key);
            }
            duplicates::DialogAction::Cancel => {}
        }
    }

    /// "Použít stejnou volbu pro zbytek souborů ze stejné složky" (§6.2):
    /// pro každý dosud nevyřešený/nepřeskočený soubor ve stejné složce, který
    /// má mezi svými MATCH kandidáty jeden ve `chosen_path`ově rodičovské
    /// složce, tenhle kandidát rovnou zvolí - bez otevření dialogu (M4 DoD:
    /// „vyřešení 500 souborů bez 500 kliknutí").
    fn apply_sticky_choice_to_folder(&mut self, folder_key_value: &str, chosen_path: &Path) {
        let Some(target_dir) = chosen_path.parent() else {
            return;
        };
        self.sticky_folder_choice
            .insert(folder_key_value.to_string(), target_dir.to_path_buf());

        let Some(loaded) = &self.loaded else { return };
        let target_key = match_key_str(&target_dir.to_string_lossy());

        let mut newly_resolved = Vec::new();
        for f in &loaded.files {
            if !loaded.status_of(f.file_id).is_repairable() {
                continue;
            }
            if self.resolved_choices.contains_key(&f.file_id)
                || self.skipped_files.contains(&f.file_id)
            {
                continue;
            }
            if folder_key(f.root_id, &f.rel_path) != folder_key_value {
                continue;
            }
            let Some(matches) = loaded.matches_of(f.file_id) else {
                continue;
            };
            let candidate = matches.candidates.iter().find(|c| {
                c.path
                    .parent()
                    .map(|p| match_key_str(&p.to_string_lossy()))
                    .as_deref()
                    == Some(target_key.as_str())
            });
            if let Some(c) = candidate {
                newly_resolved.push((f.file_id, c.path.clone()));
            }
        }
        for (id, path) in newly_resolved {
            self.resolved_choices.insert(id, path);
        }
    }

    /// Označí všechny dosud nevyřešené soubory ve stejné složce jako
    /// přeskočené (§6.2 "Přeskočit vše").
    fn skip_rest_of_folder(&mut self, folder_key_value: &str) {
        let Some(loaded) = &self.loaded else { return };
        let ids: Vec<i64> = loaded
            .files
            .iter()
            .filter(|f| loaded.status_of(f.file_id).is_repairable())
            .filter(|f| folder_key(f.root_id, &f.rel_path) == folder_key_value)
            .filter(|f| !self.resolved_choices.contains_key(&f.file_id))
            .map(|f| f.file_id)
            .collect();
        self.skipped_files.extend(ids);
    }

    /// Spustí zápis `resolved_choices` na pozadí (stejný vzor jako
    /// [`Self::start_apply_root_remap`]) - volá se až po potvrzení
    /// `confirm_apply_resolved` dialogu.
    pub fn start_apply_resolved(&mut self) {
        let Some(loaded) = &self.loaded else { return };
        let catalog_path = loaded.catalog_path.clone();
        let roots = loaded.roots.clone();
        let known_folders = apply::known_folders_by_path(&loaded.files);
        let resolved: Vec<(i64, PathBuf)> = self
            .resolved_choices
            .iter()
            .map(|(id, path)| (*id, path.clone()))
            .collect();

        self.applying = true;
        self.apply_error = None;
        self.apply_success = None;

        let (tx, rx) = mpsc::channel();
        self.apply_receiver = Some(rx);

        std::thread::spawn(move || {
            let outcome = apply_resolved(&catalog_path, &roots, &known_folders, resolved);
            let _ = tx.send(outcome);
        });
    }
}

fn apply_root_remap(
    catalog_path: &Path,
    suggestion: RemapSuggestion,
) -> Result<ApplySummary, String> {
    // Pre-flight §8 bod 0.4 uvnitř `plan_root_remap`: sonda (§7.4.1) sice
    // existenci ověřila, ale to bylo při načtení katalogu - disk se mezitím
    // mohl odpojit.
    let db_op = apply::plan_root_remap(suggestion.root_id, &suggestion.proposed_absolute_path)
        .map_err(|e| e.to_string())?;

    let mut journal_path = catalog_path.as_os_str().to_os_string();
    journal_path.push(".journal.jsonl");
    let session = format!("gui-{}", chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f"));

    let plan = Plan {
        fs_ops: vec![],
        db_ops: vec![db_op],
    };

    let outcome = apply::apply(catalog_path, Path::new(&journal_path), &session, plan)
        .map_err(|e| e.to_string())?;

    match outcome.db_phase {
        DbPhaseResult::Committed => Ok(ApplySummary {
            message: format!(
                "Root {} přepsán na „{}“. Záloha: {}",
                suggestion.root_id,
                suggestion.proposed_absolute_path,
                outcome.backup.backup_dir.display()
            ),
            catalog_path: catalog_path.to_path_buf(),
        }),
        DbPhaseResult::RolledBack {
            reason,
            compensation_failures,
        } => {
            if compensation_failures.is_empty() {
                Err(format!(
                    "zápis selhal, katalog je nedotčený (vrácen zpět): {reason}"
                ))
            } else {
                Err(format!(
                    "zápis selhal A kompenzace {} souborů se nepovedla - vyžaduje ruční kontrolu: {reason}",
                    compensation_failures.len()
                ))
            }
        }
    }
}

/// Zapíše potvrzený návrh vzoru A (§7.1). Zkusí levný `RemapFolder` (jeden
/// UPDATE); když `plan_folder_repoint` vrátí `TargetFolderAlreadyExists`
/// (cílová složka už v katalogu existuje jako jiná), spadne na N×
/// `UpdateFileFolder` na `files_in_folder` - přesně fallback, který
/// PROJECT.md §7.1 bod 1 popisuje ("nelze repointovat → místo toho N×
/// UPDATE AgLibraryFile SET folder").
fn apply_folder_repoint(
    catalog_path: &Path,
    suggestion: &FolderMovedSuggestion,
    roots: &[RootFolder],
    known_folders: &HashMap<String, i64>,
    files_in_folder: &[i64],
) -> Result<ApplySummary, String> {
    // Pre-flight §8 bod 0.4 - cílová složka mohla mezitím zmizet / odpojit se.
    let target_dir =
        apply::VerifiedPath::check(&suggestion.target_dir).map_err(|e| e.to_string())?;

    let db_ops =
        match apply::plan_folder_repoint(suggestion.folder_id, &target_dir, roots, known_folders) {
            Ok(op) => vec![op],
            Err(ManualResolveError::TargetFolderAlreadyExists { existing_folder_id }) => {
                files_in_folder
                    .iter()
                    .map(|&file_id| DbOpTemplate::UpdateFileFolder {
                        file_id,
                        new_folder_id: existing_folder_id,
                    })
                    .collect()
            }
            Err(e) => return Err(e.to_string()),
        };
    let is_fallback = db_ops.len() > 1;
    let op_count = db_ops.len();

    let mut journal_path = catalog_path.as_os_str().to_os_string();
    journal_path.push(".journal.jsonl");
    let session = format!(
        "gui-folder-moved-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f")
    );

    let plan = Plan {
        fs_ops: vec![],
        db_ops,
    };
    let outcome = apply::apply(catalog_path, Path::new(&journal_path), &session, plan)
        .map_err(|e| e.to_string())?;

    match outcome.db_phase {
        DbPhaseResult::Committed => {
            let message = if is_fallback {
                format!(
                    "Složka „{}“ přepointována po jednotlivých souborech ({op_count} UPDATE, cílová složka \
                     už v katalogu existovala). Záloha: {}",
                    suggestion.source_rel_path,
                    outcome.backup.backup_dir.display()
                )
            } else {
                format!(
                    "Složka „{}“ přepointována na „{}“ jedním UPDATE. Záloha: {}",
                    suggestion.source_rel_path,
                    suggestion.target_dir.display(),
                    outcome.backup.backup_dir.display()
                )
            };
            Ok(ApplySummary {
                message,
                catalog_path: catalog_path.to_path_buf(),
            })
        }
        DbPhaseResult::RolledBack {
            reason,
            compensation_failures,
        } => {
            if compensation_failures.is_empty() {
                Err(format!(
                    "zápis selhal, katalog je nedotčený (vrácen zpět): {reason}"
                ))
            } else {
                Err(format!(
                    "zápis selhal A kompenzace {} souborů se nepovedla - vyžaduje ruční kontrolu: {reason}",
                    compensation_failures.len()
                ))
            }
        }
    }
}

/// Zapíše potvrzený návrh vzoru A, druhou možnost (§7.1 bod 2, "Přesun
/// souborů zpět"): postaví `Plan` přes `folder_moved::plan_move_back`
/// (`db_ops` je vždy prázdný - katalog se nemění) a spustí ho stejnou APPLY
/// sessionou jako [`apply_folder_repoint`]. Částečné selhání (pár souborů z
/// skupiny se nepřesune) je platný výsledek (§8 fáze 1.4) - hlásí se v
/// souhrnu, ne jako tvrdá chyba.
fn apply_folder_move_back(
    catalog_path: &Path,
    suggestion: &FolderMovedSuggestion,
    files: &[CatalogFile],
    matches: &HashMap<i64, FileMatches>,
) -> Result<ApplySummary, String> {
    let plan = folder_moved::plan_move_back(suggestion, files, matches);
    if plan.fs_ops.is_empty() {
        return Err(
            "žádný soubor k přesunu - kandidáti mezitím zmizeli, znovu ověřte katalog".to_string(),
        );
    }

    let mut journal_path = catalog_path.as_os_str().to_os_string();
    journal_path.push(".journal.jsonl");
    let session = format!(
        "gui-move-back-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f")
    );

    let outcome = apply::apply(catalog_path, Path::new(&journal_path), &session, plan)
        .map_err(|e| e.to_string())?;

    let committed = outcome.fs_phase.committed.len();
    let failed = outcome.fs_phase.failed.len();
    let rejected = outcome.fs_phase.rejected.len();

    let mut message = format!(
        "Složka „{}“: {committed} souborů (vč. sidecarů) přesunuto zpět na očekávanou cestu.",
        suggestion.source_rel_path
    );
    if failed > 0 || rejected > 0 {
        message.push_str(&format!(
            " {failed} selhalo, {rejected} odmítnuto při kontrole předem - podrobnosti v logu."
        ));
    }
    message.push_str(&format!(" Záloha: {}", outcome.backup.backup_dir.display()));

    match outcome.db_phase {
        DbPhaseResult::Committed => Ok(ApplySummary {
            message,
            catalog_path: catalog_path.to_path_buf(),
        }),
        DbPhaseResult::RolledBack { reason, .. } => Err(format!(
            "neočekávané selhání DB fáze (tenhle zápis katalog vůbec nemění): {reason}"
        )),
    }
}

/// Zapíše potvrzený návrh vzoru B (§7.2, "Konsolidace") - stejný vzor jako
/// [`apply_folder_move_back`], jen přes `scattered::plan_consolidate`.
fn apply_consolidate(
    catalog_path: &Path,
    suggestion: &ScatteredSuggestion,
    files: &[CatalogFile],
    matches: &HashMap<i64, FileMatches>,
) -> Result<ApplySummary, String> {
    let plan = scattered::plan_consolidate(suggestion, files, matches);
    if plan.fs_ops.is_empty() {
        return Err(
            "žádný soubor k přesunu - kandidáti mezitím zmizeli, znovu ověřte katalog".to_string(),
        );
    }

    let mut journal_path = catalog_path.as_os_str().to_os_string();
    journal_path.push(".journal.jsonl");
    let session = format!(
        "gui-consolidate-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f")
    );

    let outcome = apply::apply(catalog_path, Path::new(&journal_path), &session, plan)
        .map_err(|e| e.to_string())?;

    let committed = outcome.fs_phase.committed.len();
    let failed = outcome.fs_phase.failed.len();
    let rejected = outcome.fs_phase.rejected.len();

    let mut message = format!(
        "Složka „{}“: {committed} souborů (vč. sidecarů) konsolidováno zpět do zdrojové složky.",
        suggestion.source_rel_path
    );
    if failed > 0 || rejected > 0 {
        message.push_str(&format!(
            " {failed} selhalo, {rejected} odmítnuto při kontrole předem (typicky kolize jmen v cíli) - \
             podrobnosti v logu."
        ));
    }
    message.push_str(&format!(" Záloha: {}", outcome.backup.backup_dir.display()));

    match outcome.db_phase {
        DbPhaseResult::Committed => Ok(ApplySummary {
            message,
            catalog_path: catalog_path.to_path_buf(),
        }),
        DbPhaseResult::RolledBack { reason, .. } => Err(format!(
            "neočekávané selhání DB fáze (tenhle zápis katalog vůbec nemění): {reason}"
        )),
    }
}

/// Sestaví `Plan` z ručně vyřešených souborů (§6.2) přes
/// `apply::plan_manual_resolve` a zapíše ho stejnou APPLY sessionou jako
/// [`apply_root_remap`]. Soubory, pro které se nenajde odpovídající root
/// (`ManualResolveError::NoMatchingRoot`), se do zápisu vůbec nezahrnou -
/// hlásí se zvlášť, ne tiše (CLAUDE.md „raději hlasitá chyba").
fn apply_resolved(
    catalog_path: &Path,
    roots: &[RootFolder],
    known_folders: &HashMap<String, i64>,
    resolved: Vec<(i64, PathBuf)>,
) -> Result<ApplySummary, String> {
    let mut db_ops = Vec::new();
    let mut unplannable = Vec::new();
    for (file_id, path) in &resolved {
        // Pre-flight §8 bod 0.4: od chvíle, kdy uživatel kandidáta vybral v
        // dialogu, mohla uběhnout dlouhá doba (odklikává jich stovky).
        // Ověřujeme SAMOTNÝ SOUBOR, ne jen složku - tvrdíme o něm, že tam je;
        // složka může zůstat, i když soubor mezitím zmizel.
        let verified_file = match apply::VerifiedPath::check(path) {
            Ok(v) => v,
            Err(e) => {
                unplannable.push(format!("soubor id {file_id}: {e}"));
                continue;
            }
        };
        let Some(target_dir) = verified_file.parent() else {
            unplannable.push(format!(
                "soubor id {file_id}: kandidát nemá rodičovskou složku"
            ));
            continue;
        };
        match apply::plan_manual_resolve(*file_id, &target_dir, roots, known_folders) {
            Ok(op) => db_ops.push(op),
            Err(e) => unplannable.push(format!("soubor id {file_id}: {e}")),
        }
    }

    if db_ops.is_empty() {
        return Err(if unplannable.is_empty() {
            "žádné vyřešené soubory k zápisu".to_string()
        } else {
            format!(
                "žádný z {} vyřešených souborů nejde zapsat:\n{}",
                unplannable.len(),
                unplannable.join("\n")
            )
        });
    }

    let mut journal_path = catalog_path.as_os_str().to_os_string();
    journal_path.push(".journal.jsonl");
    let session = format!(
        "gui-resolve-{}",
        chrono::Utc::now().format("%Y%m%d-%H%M%S%.3f")
    );

    let planned_count = db_ops.len();
    let plan = Plan {
        fs_ops: vec![],
        db_ops,
    };

    let outcome = apply::apply(catalog_path, Path::new(&journal_path), &session, plan)
        .map_err(|e| e.to_string())?;

    match outcome.db_phase {
        DbPhaseResult::Committed => {
            let mut message = format!(
                "{planned_count} souborů přepsáno v katalogu. Záloha: {}",
                outcome.backup.backup_dir.display()
            );
            if !unplannable.is_empty() {
                message.push_str(&format!(
                    "\n{} souborů se nezapsalo (žádný odpovídající root, zůstávají nevyřešené): {}",
                    unplannable.len(),
                    unplannable.join("; ")
                ));
            }
            Ok(ApplySummary {
                message,
                catalog_path: catalog_path.to_path_buf(),
            })
        }
        DbPhaseResult::RolledBack {
            reason,
            compensation_failures,
        } => {
            if compensation_failures.is_empty() {
                Err(format!(
                    "zápis selhal, katalog je nedotčený (vrácen zpět): {reason}"
                ))
            } else {
                Err(format!(
                    "zápis selhal A kompenzace {} souborů se nepovedla - vyžaduje ruční kontrolu: {reason}",
                    compensation_failures.len()
                ))
            }
        }
    }
}

fn load_catalog(path: &Path, cancel: &AtomicBool) -> LoadOutcome {
    let handle = open::open_read_only(path).map_err(|e| e.to_string())?;
    let roots = extract::list_root_folders(&handle.conn).map_err(|e| e.to_string())?;

    #[cfg(windows)]
    {
        let foreign = extract::foreign_roots(&roots);
        if !foreign.is_empty() {
            let list = foreign
                .iter()
                .map(|r| format!("  - {} (id {})", r.absolute_path, r.root_id))
                .collect::<Vec<_>>()
                .join("\n");
            return Err(format!(
                "Katalog obsahuje {} kořenových složek, jejichž cesta nevypadá jako windows cesta \
                 (pravděpodobně katalog z macOS). Ruční mapování kořenů zatím GUI neumí (přijde \
                 v pozdějším milníku):\n{list}",
                foreign.len()
            ));
        }
    }

    load_with_roots(&handle, path, roots, cancel)
}

/// Členství v kolekcích pro vzor C (§7.3). Schéma kolekcí není zaručené,
/// takže se detekce jen tiše vynechá, když nesedí - místo aby hádala
/// (CLAUDE.md §6). Ověřeno na LrC 13.3: schéma sedí a tabulky jsou naplněné.
fn collection_members(conn: &rusqlite::Connection) -> Vec<extract::CollectionMember> {
    if !schema::has_collection_schema(conn) {
        return Vec::new();
    }
    match extract::collection_membership(conn) {
        Ok(members) => members,
        Err(e) => {
            tracing::warn!(%e, "vzor C: dotaz na kolekce selhal, detekce se přeskakuje");
            Vec::new()
        }
    }
}

fn load_with_roots(
    handle: &open::CatalogHandle,
    path: &Path,
    roots: Vec<RootFolder>,
    cancel: &AtomicBool,
) -> LoadOutcome {
    let files = extract::extract(&handle.conn).map_err(|e| e.to_string())?;
    // Vzor D, sonda (§7.4.1) - běží před INDEXem/MATCHem záměrně (skenovat
    // kvůli pouhé změně písmene disku celý disk by bylo absurdní).
    let remap_suggestions = root_remap::probe_all(&roots, &files);

    let cfg = config::load();
    let io_threads = volumes::pick_io_thread_count(path, &cfg.scan);
    let statuses: HashMap<i64, FileStatus> = verify::verify(&files, io_threads)
        .into_iter()
        .map(|r| (r.file_id, r.status))
        .collect();

    let walk_roots = index::default_walk_roots(
        &roots
            .iter()
            .map(|r| r.absolute_path.clone())
            .collect::<Vec<_>>(),
    );
    let match_run = matching::run(
        &files,
        &statuses,
        &walk_roots,
        &cfg.r#match,
        &cfg.scan.exclude,
        cancel,
        path,
    );

    // Celá fáze CLUSTER (§7) - sdílená s CLI přes `cluster::detect_all`,
    // aby se obě cesty nemohly rozejít.
    let members = collection_members(&handle.conn);
    let clusters = cluster::detect_all(
        &roots,
        &files,
        &statuses,
        &match_run.matches,
        remap_suggestions,
        &members,
    );
    let cluster::ClusterResult {
        remap: remap_suggestions,
        folder_moved: folder_moved_suggestions,
        scattered: scattered_suggestions,
        album_scattered: album_scattered_suggestions,
    } = clusters;

    Ok(LoadedCatalog {
        catalog_path: path.to_path_buf(),
        lock_status: handle.lock_status.clone(),
        roots,
        files,
        statuses,
        remap_suggestions,
        folder_moved_suggestions,
        scattered_suggestions,
        album_scattered_suggestions,
        match_run,
    })
}
