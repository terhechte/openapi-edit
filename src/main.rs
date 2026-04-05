use eframe::egui;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

// ---------------------------------------------------------------------------
// Data model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum SpecFormat {
    Yaml,
    Json,
}

impl SpecFormat {
    fn label(self) -> &'static str {
        match self {
            Self::Yaml => "YAML",
            Self::Json => "JSON",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Yaml => "yaml",
            Self::Json => "json",
        }
    }
}

struct Endpoint {
    path: String,
    selected: bool,
}

struct PathGroup {
    name: String,
    endpoints: Vec<Endpoint>,
}

impl PathGroup {
    fn all_selected(&self) -> bool {
        self.endpoints.iter().all(|e| e.selected)
    }

    fn none_selected(&self) -> bool {
        self.endpoints.iter().all(|e| !e.selected)
    }

    fn selected_count(&self) -> usize {
        self.endpoints.iter().filter(|e| e.selected).count()
    }

    fn set_all(&mut self, selected: bool) {
        for ep in &mut self.endpoints {
            ep.selected = selected;
        }
    }
}

/// Persisted map: source identifier (file path or URL) → list of deselected paths.
#[derive(Default, Serialize, Deserialize)]
struct SelectionDb {
    deselected: HashMap<String, Vec<String>>,
}

const SELECTION_DB_KEY: &str = "selection_db";

struct App {
    spec: Option<Value>,
    source_format: SpecFormat,
    export_format: SpecFormat,
    groups: Vec<PathGroup>,
    status: String,
    loaded_file_name: Option<String>,
    search_query: String,
    prune_components: bool,
    /// The key identifying the current source (absolute path or URL).
    source_key: Option<String>,
    /// Persisted selection state across sessions.
    selection_db: SelectionDb,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let selection_db: SelectionDb = cc
            .storage
            .and_then(|s| eframe::get_value(s, SELECTION_DB_KEY))
            .unwrap_or_default();

        Self {
            spec: None,
            source_format: SpecFormat::Yaml,
            export_format: SpecFormat::Yaml,
            groups: Vec::new(),
            status: String::from("Open an OpenAPI spec to get started."),
            loaded_file_name: None,
            search_query: String::new(),
            prune_components: true,
            source_key: None,
            selection_db,
        }
    }

    /// Apply saved selection from the db to the current groups.
    fn apply_saved_selection(&mut self) {
        let Some(key) = &self.source_key else { return };
        let Some(deselected) = self.selection_db.deselected.get(key) else { return };
        let deselected_set: HashSet<&str> = deselected.iter().map(|s| s.as_str()).collect();
        for group in &mut self.groups {
            for ep in &mut group.endpoints {
                if deselected_set.contains(ep.path.as_str()) {
                    ep.selected = false;
                }
            }
        }
        let total: usize = self.groups.iter().map(|g| g.endpoints.len()).sum();
        let selected: usize = self.groups.iter().map(|g| g.selected_count()).sum();
        if selected < total {
            self.status = format!(
                "{} — restored previous selection ({selected}/{total})",
                self.status
            );
        }
    }

    /// Save the current selection to the db.
    fn save_selection(&mut self) {
        let Some(key) = &self.source_key else { return };
        let deselected: Vec<String> = self
            .groups
            .iter()
            .flat_map(|g| g.endpoints.iter())
            .filter(|e| !e.selected)
            .map(|e| e.path.clone())
            .collect();
        if deselected.is_empty() {
            self.selection_db.deselected.remove(key);
        } else {
            self.selection_db.deselected.insert(key.clone(), deselected);
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing & export
// ---------------------------------------------------------------------------

fn detect_format(path: &Path) -> SpecFormat {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => SpecFormat::Json,
        _ => SpecFormat::Yaml,
    }
}

fn load_spec(path: &Path) -> Result<(Value, SpecFormat), String> {
    let contents = std::fs::read_to_string(path).map_err(|e| format!("Read error: {e}"))?;
    let format = detect_format(path);
    let value: Value = match format {
        SpecFormat::Json => serde_json::from_str(&contents).map_err(|e| format!("JSON parse error: {e}"))?,
        SpecFormat::Yaml => serde_yaml::from_str(&contents).map_err(|e| format!("YAML parse error: {e}"))?,
    };
    Ok((value, format))
}

fn extract_groups(spec: &Value) -> Result<Vec<PathGroup>, String> {
    let paths_obj = spec
        .get("paths")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "No 'paths' object found in spec.".to_string())?;

    let mut map: BTreeMap<String, Vec<Endpoint>> = BTreeMap::new();

    for key in paths_obj.keys() {
        let group_name = key
            .trim_start_matches('/')
            .split('/')
            .next()
            .unwrap_or("/");
        let group_name = if group_name.is_empty() { "/" } else { group_name };

        map.entry(group_name.to_string())
            .or_default()
            .push(Endpoint {
                path: key.clone(),
                selected: true,
            });
    }

    // Sort endpoints within each group
    for endpoints in map.values_mut() {
        endpoints.sort_by(|a, b| a.path.cmp(&b.path));
    }

    Ok(map
        .into_iter()
        .map(|(name, endpoints)| PathGroup { name, endpoints })
        .collect())
}

fn build_filtered_spec(spec: &Value, groups: &[PathGroup], prune: bool) -> Value {
    let mut out = spec.clone();

    // Collect deselected paths
    let deselected: HashSet<&str> = groups
        .iter()
        .flat_map(|g| g.endpoints.iter())
        .filter(|e| !e.selected)
        .map(|e| e.path.as_str())
        .collect();

    if let Some(paths) = out.get_mut("paths").and_then(|v| v.as_object_mut()) {
        paths.retain(|key, _| !deselected.contains(key.as_str()));
    }

    if prune {
        prune_unused_components(&mut out);
    }

    out
}

// ---------------------------------------------------------------------------
// Component pruning — remove unreferenced entries from components/*
// ---------------------------------------------------------------------------

/// Recursively walk a JSON value and collect all `$ref` strings.
fn collect_refs(value: &Value, refs: &mut HashSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(r)) = map.get("$ref") {
                refs.insert(r.clone());
            }
            for v in map.values() {
                collect_refs(v, refs);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                collect_refs(v, refs);
            }
        }
        _ => {}
    }
}

/// Compute the full set of reachable component refs starting from the
/// non-component parts of the spec (paths, info, security, etc.), then
/// transitively following refs within reached components.
fn reachable_component_refs(spec: &Value) -> HashSet<String> {
    let mut reachable = HashSet::new();

    // Seed: collect refs from everything *except* the components object itself
    if let Some(obj) = spec.as_object() {
        for (key, value) in obj {
            if key != "components" {
                collect_refs(value, &mut reachable);
            }
        }
    }

    // Transitively resolve: any newly-reachable component may itself contain refs
    let components = spec.get("components");
    loop {
        let mut newly_found = HashSet::new();
        for r in &reachable {
            // Parse refs like "#/components/schemas/Foo"
            if let Some(component_value) = resolve_local_ref(spec, r) {
                collect_refs(component_value, &mut newly_found);
            }
        }
        // Keep only refs we haven't seen yet
        let before = reachable.len();
        reachable.extend(newly_found);
        if reachable.len() == before {
            break; // Fixed point reached
        }
    }

    let _ = components; // suppress unused warning
    reachable
}

/// Resolve a local JSON pointer ref like `#/components/schemas/Foo` to
/// the corresponding `Value` in the spec.
fn resolve_local_ref<'a>(spec: &'a Value, reference: &str) -> Option<&'a Value> {
    let pointer = reference.strip_prefix('#')?;
    // JSON pointer uses '/' separators; serde_json::Value::pointer expects this format
    spec.pointer(pointer)
}

/// Remove unreferenced entries from every sub-section of `components`.
fn prune_unused_components(spec: &mut Value) {
    let reachable = reachable_component_refs(spec);

    let Some(components) = spec.get_mut("components").and_then(|v| v.as_object_mut()) else {
        return;
    };

    // For each sub-section (schemas, responses, parameters, requestBodies, …)
    for (section_name, section_value) in components.iter_mut() {
        let Some(section_map) = section_value.as_object_mut() else {
            continue;
        };
        let prefix = format!("#/components/{section_name}/");
        section_map.retain(|entry_name, _| {
            let full_ref = format!("{prefix}{entry_name}");
            reachable.contains(&full_ref)
        });
    }

    // Remove empty sub-sections
    components.retain(|_, v| {
        v.as_object().map_or(true, |m| !m.is_empty())
    });
}

fn serialize_spec(spec: &Value, format: SpecFormat) -> Result<String, String> {
    match format {
        SpecFormat::Json => {
            serde_json::to_string_pretty(spec).map_err(|e| format!("JSON write error: {e}"))
        }
        SpecFormat::Yaml => {
            serde_yaml::to_string(spec).map_err(|e| format!("YAML write error: {e}"))
        }
    }
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------

impl eframe::App for App {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SELECTION_DB_KEY, &self.selection_db);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // -- Top panel --
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.button("Open File…").clicked() {
                    self.open_file();
                }

                ui.separator();

                let has_spec = self.spec.is_some();

                if ui
                    .add_enabled(has_spec, egui::Button::new("Select All"))
                    .clicked()
                {
                    for g in &mut self.groups {
                        g.set_all(true);
                    }
                }

                if ui
                    .add_enabled(has_spec, egui::Button::new("Deselect All"))
                    .clicked()
                {
                    for g in &mut self.groups {
                        g.set_all(false);
                    }
                }

                ui.separator();

                ui.label("Export as:");
                ui.radio_value(&mut self.export_format, SpecFormat::Yaml, "YAML");
                ui.radio_value(&mut self.export_format, SpecFormat::Json, "JSON");

                ui.separator();

                ui.checkbox(&mut self.prune_components, "Remove unused components");

                ui.separator();

                if ui
                    .add_enabled(has_spec, egui::Button::new("Export…"))
                    .clicked()
                {
                    self.export_file();
                }
            });
            ui.add_space(2.0);

            // Search bar
            if self.spec.is_some() {
                ui.horizontal(|ui| {
                    ui.label("Filter:");
                    ui.text_edit_singleline(&mut self.search_query);
                    if ui.button("✕").clicked() {
                        self.search_query.clear();
                    }
                });
                ui.add_space(2.0);
            }
        });

        // -- Bottom panel --
        egui::TopBottomPanel::bottom("status_bar").show(ctx, |ui| {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                if !self.groups.is_empty() {
                    let selected: usize = self.groups.iter().map(|g| g.selected_count()).sum();
                    let total: usize = self.groups.iter().map(|g| g.endpoints.len()).sum();
                    ui.label(format!("Selected: {selected} / {total} endpoints"));
                    ui.separator();
                }
                ui.label(&self.status);
            });
            ui.add_space(2.0);
        });

        // -- Central panel: tree --
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.groups.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No spec loaded. Click \"Open File…\" to begin.");
                });
                return;
            }

            let query = self.search_query.to_lowercase();

            egui::ScrollArea::vertical().show(ui, |ui| {
                for group in &mut self.groups {
                    // Filter endpoints by search query
                    let visible_indices: Vec<usize> = group
                        .endpoints
                        .iter()
                        .enumerate()
                        .filter(|(_, ep)| query.is_empty() || ep.path.to_lowercase().contains(&query))
                        .map(|(i, _)| i)
                        .collect();

                    if visible_indices.is_empty() && !query.is_empty() {
                        continue; // Skip group entirely if no matches
                    }

                    let ep_count = if query.is_empty() {
                        group.endpoints.len()
                    } else {
                        visible_indices.len()
                    };

                    // Group row: checkbox + collapsing header
                    let id = ui.make_persistent_id(&group.name);

                    ui.horizontal(|ui| {
                        // Tri-state checkbox for group
                        let all = group.all_selected();
                        let none = group.none_selected();
                        let mut state = !none; // checked if any are selected

                        let response = ui.checkbox(&mut state, "");

                        // Draw indeterminate indicator (dash) when partially selected
                        if !all && !none {
                            let rect = response.rect;
                            let center = rect.center();
                            let half = rect.width() * 0.2;
                            ui.painter().line_segment(
                                [
                                    egui::pos2(center.x - half, center.y),
                                    egui::pos2(center.x + half, center.y),
                                ],
                                egui::Stroke::new(2.0, ui.visuals().text_color()),
                            );
                        }

                        if response.changed() {
                            group.set_all(state);
                        }

                        let header_text = format!(
                            "/{} ({} endpoint{})",
                            group.name,
                            ep_count,
                            if ep_count == 1 { "" } else { "s" }
                        );

                        egui::CollapsingHeader::new(
                            egui::RichText::new(header_text).strong(),
                        )
                        .id_salt(id)
                        .default_open(false)
                        .show(ui, |ui| {
                            if query.is_empty() {
                                for ep in &mut group.endpoints {
                                    ui.checkbox(&mut ep.selected, &ep.path);
                                }
                            } else {
                                for &idx in &visible_indices {
                                    let ep = &mut group.endpoints[idx];
                                    ui.checkbox(&mut ep.selected, &ep.path);
                                }
                            }
                        });
                    });
                }
            });
        });
    }
}

impl App {
    fn load_from_path(&mut self, path: &Path) {
        match load_spec(path) {
            Ok((value, format)) => match extract_groups(&value) {
                Ok(groups) => {
                    let total: usize = groups.iter().map(|g| g.endpoints.len()).sum();
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    self.status = format!("Loaded {name} — {total} endpoints");
                    self.spec = Some(value);
                    self.source_format = format;
                    self.export_format = format;
                    self.groups = groups;
                    self.loaded_file_name = Some(name);
                    self.search_query.clear();
                    // Use canonical absolute path as source key
                    self.source_key = Some(
                        std::fs::canonicalize(path)
                            .unwrap_or_else(|_| path.to_path_buf())
                            .to_string_lossy()
                            .to_string(),
                    );
                    self.apply_saved_selection();
                }
                Err(e) => {
                    self.status = e;
                }
            },
            Err(e) => {
                self.status = e;
            }
        }
    }

    fn load_from_string(&mut self, contents: &str, name: &str, source_key: &str, format: SpecFormat) {
        let parse_result: Result<Value, String> = match format {
            SpecFormat::Json => serde_json::from_str(contents).map_err(|e| format!("JSON parse error: {e}")),
            SpecFormat::Yaml => serde_yaml::from_str(contents).map_err(|e| format!("YAML parse error: {e}")),
        };
        match parse_result {
            Ok(value) => match extract_groups(&value) {
                Ok(groups) => {
                    let total: usize = groups.iter().map(|g| g.endpoints.len()).sum();
                    self.status = format!("Loaded {name} — {total} endpoints");
                    self.spec = Some(value);
                    self.source_format = format;
                    self.export_format = format;
                    self.groups = groups;
                    self.loaded_file_name = Some(name.to_string());
                    self.search_query.clear();
                    self.source_key = Some(source_key.to_string());
                    self.apply_saved_selection();
                }
                Err(e) => self.status = e,
            },
            Err(e) => self.status = e,
        }
    }

    fn open_file(&mut self) {
        let file = rfd::FileDialog::new()
            .add_filter("OpenAPI Spec", &["yaml", "yml", "json"])
            .pick_file();

        if let Some(path) = file {
            self.load_from_path(&path);
        }
    }

    fn export_file(&mut self) {
        let Some(spec) = &self.spec else { return };

        let filtered = build_filtered_spec(spec, &self.groups, self.prune_components);

        match serialize_spec(&filtered, self.export_format) {
            Ok(output) => {
                let default_name = format!(
                    "filtered.{}",
                    self.export_format.extension()
                );

                let file = rfd::FileDialog::new()
                    .set_file_name(&default_name)
                    .add_filter(
                        self.export_format.label(),
                        &[self.export_format.extension()],
                    )
                    .save_file();

                if let Some(path) = file {
                    match std::fs::write(&path, &output) {
                        Ok(()) => {
                            self.save_selection();
                            let selected: usize =
                                self.groups.iter().map(|g| g.selected_count()).sum();
                            self.status = format!(
                                "Exported {selected} endpoints to {}",
                                path.display()
                            );
                        }
                        Err(e) => {
                            self.status = format!("Write error: {e}");
                        }
                    }
                }
            }
            Err(e) => {
                self.status = e;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Guess format from a URL path or fall back to YAML.
fn detect_format_from_url(url: &str) -> SpecFormat {
    if url.ends_with(".json") {
        SpecFormat::Json
    } else {
        SpecFormat::Yaml
    }
}

/// Download a URL synchronously, returning (contents, display_name, format).
fn download_spec(url: &str) -> Result<(String, String, SpecFormat), String> {
    let body: String = ureq::get(url)
        .call()
        .map_err(|e| format!("HTTP request failed: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    let format = detect_format_from_url(url);
    let name = url.rsplit('/').next().unwrap_or(url).to_string();
    Ok((body, name, format))
}

fn main() -> eframe::Result {
    let arg = std::env::args().nth(1);

    // If argument looks like a URL, download it before starting the GUI.
    let initial_source: Option<InitialSource> = match arg.as_deref() {
        Some(s) if s.starts_with("http://") || s.starts_with("https://") => {
            match download_spec(s) {
                Ok((contents, name, format)) => Some(InitialSource::Downloaded { contents, name, source_url: s.to_string(), format }),
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(path) => Some(InitialSource::File(std::path::PathBuf::from(path))),
        None => None,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([600.0, 700.0])
            .with_min_inner_size([400.0, 300.0]),
        ..Default::default()
    };

    eframe::run_native(
        "OpenAPI Editor",
        options,
        Box::new(|cc| {
            let mut app = App::new(cc);
            match initial_source {
                Some(InitialSource::File(path)) => app.load_from_path(&path),
                Some(InitialSource::Downloaded { contents, name, source_url, format }) => {
                    app.load_from_string(&contents, &name, &source_url, format);
                }
                None => {}
            }
            Ok(Box::new(app))
        }),
    )
}

enum InitialSource {
    File(std::path::PathBuf),
    Downloaded { contents: String, name: String, source_url: String, format: SpecFormat },
}
