//! Levý panel: strom `AgLibraryRootFolder` → podsložky (PROJECT.md §6.1).
//!
//! Meziuzly stromu (např. "2019" nad "2019/2019-07 HR/") nejsou nutně
//! skutečné řádky `AgLibraryFolder` - vznikají rozpadem `pathFromRoot` na
//! segmenty kvůli stromovému zobrazení. To je čistě prezentační derivace nad
//! už načtenými daty (CLAUDE.md §5: `app/` nesmí dělat SQL/I/O - a tohle ani
//! jedno nedělá), ne business logika CLUSTER fáze.

use std::collections::BTreeMap;

use lrcatfix::fsops::verify::FileStatus;

use crate::app::state::LoadedCatalog;

pub struct FolderNode {
    pub name: String,
    /// Jednoznačný klíč uzlu: `"<root_id>/<segment>/<segment>/..."`.
    /// Používá se pro výběr ve stromu a pro filtrování v `panel_files`.
    pub key: String,
    pub total: usize,
    pub ok: usize,
    pub folder_missing: usize,
    pub children: Vec<FolderNode>,
}

impl FolderNode {
    pub fn missing(&self) -> usize {
        self.total - self.ok
    }

    /// Barva odznaku dle PROJECT.md §6.1: zelená (0 chybí), oranžová (část),
    /// červená (celá složka nenalezena - všechny soubory pod uzlem mají
    /// status `FolderMissing`).
    pub fn badge_color(&self) -> egui::Color32 {
        if self.total == 0 || self.ok == self.total {
            egui::Color32::from_rgb(0x2e, 0xa0, 0x4a)
        } else if self.folder_missing == self.total {
            egui::Color32::from_rgb(0xd6, 0x33, 0x33)
        } else {
            egui::Color32::from_rgb(0xd9, 0x8a, 0x1f)
        }
    }
}

/// Klíč složky obsahující daný soubor - stejná konstrukce jako klíče uzlů
/// stromu, aby `panel_files` mohl filtrovat podle výběru ve stromu.
pub fn folder_key(root_id: i64, rel_path: &str) -> String {
    let mut key = root_id.to_string();
    for segment in rel_path.split('/').filter(|s| !s.is_empty()) {
        key.push('/');
        key.push_str(segment);
    }
    key
}

#[derive(Default)]
struct Builder {
    total: usize,
    ok: usize,
    folder_missing: usize,
    children: BTreeMap<String, Builder>,
}

impl Builder {
    fn insert(&mut self, segments: &[&str], status: FileStatus) {
        self.total += 1;
        match status {
            FileStatus::Ok => self.ok += 1,
            FileStatus::FolderMissing => self.folder_missing += 1,
            // `Missing` ani `FolderUnavailable` se do `folder_missing`
            // nepočítají - ten řídí ČERVENÝ odznak "celá složka nenalezena"
            // (§6.1), což u jen nedostupného svazku není pravda.
            FileStatus::Missing | FileStatus::FolderUnavailable => {}
        }
        if let Some((first, rest)) = segments.split_first() {
            self.children
                .entry((*first).to_string())
                .or_default()
                .insert(rest, status);
        }
    }

    fn into_node(self, name: String, key: String) -> FolderNode {
        let children = self
            .children
            .into_iter()
            .map(|(segment, child)| {
                let child_key = format!("{key}/{segment}");
                child.into_node(segment, child_key)
            })
            .collect();
        FolderNode {
            name,
            key,
            total: self.total,
            ok: self.ok,
            folder_missing: self.folder_missing,
            children,
        }
    }
}

pub fn build_tree(loaded: &LoadedCatalog) -> Vec<FolderNode> {
    loaded
        .roots
        .iter()
        .map(|root| {
            let mut builder = Builder::default();
            for f in loaded.files.iter().filter(|f| f.root_id == root.root_id) {
                let segments: Vec<&str> = f.rel_path.split('/').filter(|s| !s.is_empty()).collect();
                builder.insert(&segments, loaded.status_of(f.file_id));
            }
            builder.into_node(root.name.clone(), root.root_id.to_string())
        })
        .collect()
}

pub fn show(ui: &mut egui::Ui, nodes: &[FolderNode], selected: &mut Option<String>) {
    egui::ScrollArea::vertical()
        .id_salt("tree_scroll")
        .show(ui, |ui| {
            for node in nodes {
                show_node(ui, node, selected);
            }
        });
}

fn show_node(ui: &mut egui::Ui, node: &FolderNode, selected: &mut Option<String>) {
    let label = format!("{}  [{} / {} ✗]", node.name, node.missing(), node.total);
    let is_selected = selected.as_deref() == Some(node.key.as_str());

    if node.children.is_empty() {
        let response = ui.selectable_label(
            is_selected,
            egui::RichText::new(label).color(node.badge_color()),
        );
        if response.clicked() {
            *selected = Some(node.key.clone());
        }
        return;
    }

    let header = egui::CollapsingHeader::new(egui::RichText::new(label).color(node.badge_color()))
        .id_salt(&node.key)
        .default_open(false)
        .show(ui, |ui| {
            for child in &node.children {
                show_node(ui, child, selected);
            }
        });
    if header.header_response.clicked() {
        *selected = Some(node.key.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use lrcatfix::catalog::extract::{CatalogFile, RootFolder};
    use lrcatfix::catalog::open::LockStatus;
    use lrcatfix::index::IndexBackend;
    use lrcatfix::matching::{calibrate::Calibration, MatchRun};

    use super::*;

    fn file(id: i64, root_id: i64, rel_path: &str, filename: &str) -> CatalogFile {
        CatalogFile {
            file_id: id,
            folder_id: 1,
            root_id,
            root_path: "C:/Foto/".to_string(),
            rel_path: rel_path.to_string(),
            filename: filename.to_string(),
            base_name: filename.to_string(),
            extension: String::new(),
            expected_path: PathBuf::new(),
            external_mod_time: None,
            md5: None,
            file_size: None,
            sidecar_extensions: Vec::new(),
        }
    }

    fn loaded(files: Vec<CatalogFile>, statuses: &[(i64, FileStatus)]) -> LoadedCatalog {
        LoadedCatalog {
            catalog_path: PathBuf::new(),
            lock_status: LockStatus::default(),
            roots: vec![RootFolder {
                root_id: 1,
                absolute_path: "C:/Foto/".to_string(),
                name: "Foto".to_string(),
            }],
            files,
            statuses: statuses.iter().copied().collect::<HashMap<_, _>>(),
            remap_suggestions: Vec::new(),
            folder_moved_suggestions: Vec::new(),
            scattered_suggestions: Vec::new(),
            album_scattered_suggestions: Vec::new(),
            match_run: MatchRun {
                index_backend: IndexBackend::Walk,
                index_size: 0,
                calibration: Calibration::disabled(),
                matches: HashMap::new(),
            },
        }
    }

    #[test]
    fn folder_key_joins_segments_with_root_id() {
        assert_eq!(folder_key(1, "2019/2019-07 HR/"), "1/2019/2019-07 HR");
        assert_eq!(folder_key(1, ""), "1");
    }

    #[test]
    fn build_tree_aggregates_counts_up_the_chain() {
        let data = loaded(
            vec![
                file(100, 1, "2019/HR/", "a.CR2"),
                file(101, 1, "2019/HR/", "b.CR2"),
                file(102, 1, "2019/Other/", "c.CR2"),
            ],
            &[
                (100, FileStatus::Ok),
                (101, FileStatus::Missing),
                (102, FileStatus::Ok),
            ],
        );

        let tree = build_tree(&data);
        assert_eq!(tree.len(), 1);
        let root = &tree[0];
        assert_eq!(root.total, 3);
        assert_eq!(root.ok, 2);
        assert_eq!(root.missing(), 1);

        let y2019 = &root.children[0];
        assert_eq!(y2019.name, "2019");
        assert_eq!(y2019.total, 3);
        assert_eq!(y2019.ok, 2);

        let hr = y2019.children.iter().find(|c| c.name == "HR").unwrap();
        assert_eq!(hr.total, 2);
        assert_eq!(hr.ok, 1);
        assert_eq!(hr.key, "1/2019/HR");
    }

    #[test]
    fn badge_color_is_red_only_when_entire_node_is_folder_missing() {
        let all_folder_missing = loaded(
            vec![
                file(100, 1, "Gone/", "a.CR2"),
                file(101, 1, "Gone/", "b.CR2"),
            ],
            &[
                (100, FileStatus::FolderMissing),
                (101, FileStatus::FolderMissing),
            ],
        );
        let tree = build_tree(&all_folder_missing);
        let gone = &tree[0].children[0];
        assert_eq!(
            gone.badge_color(),
            egui::Color32::from_rgb(0xd6, 0x33, 0x33)
        );

        let mixed = loaded(
            vec![
                file(100, 1, "Mixed/", "a.CR2"),
                file(101, 1, "Mixed/", "b.CR2"),
            ],
            &[(100, FileStatus::FolderMissing), (101, FileStatus::Ok)],
        );
        let tree = build_tree(&mixed);
        let mixed_node = &tree[0].children[0];
        assert_eq!(
            mixed_node.badge_color(),
            egui::Color32::from_rgb(0xd9, 0x8a, 0x1f)
        );
    }
}
