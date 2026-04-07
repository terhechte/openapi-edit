use base64::Engine;
use eframe::egui;
use image::GenericImageView;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

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

const HTTP_METHODS: &[&str] = &[
    "get", "post", "put", "patch", "delete", "head", "options", "trace",
];

struct Operation {
    method: String,
    selected: bool,
}

struct Endpoint {
    path: String,
    operations: Vec<Operation>,
}

impl Endpoint {
    fn all_selected(&self) -> bool {
        self.operations.iter().all(|op| op.selected)
    }

    fn none_selected(&self) -> bool {
        self.operations.iter().all(|op| !op.selected)
    }

    fn selected_count(&self) -> usize {
        self.operations.iter().filter(|op| op.selected).count()
    }

    fn set_all(&mut self, selected: bool) {
        for op in &mut self.operations {
            op.selected = selected;
        }
    }

    fn operation_count(&self) -> usize {
        self.operations.len()
    }
}

struct PathGroup {
    name: String,
    endpoints: Vec<Endpoint>,
}

impl PathGroup {
    fn all_selected(&self) -> bool {
        self.endpoints.iter().all(|e| e.all_selected())
    }

    fn none_selected(&self) -> bool {
        self.endpoints.iter().all(|e| e.none_selected())
    }

    fn selected_count(&self) -> usize {
        self.endpoints.iter().map(|e| e.selected_count()).sum()
    }

    fn operation_count(&self) -> usize {
        self.endpoints.iter().map(|e| e.operation_count()).sum()
    }

    fn set_all(&mut self, selected: bool) {
        for ep in &mut self.endpoints {
            ep.set_all(selected);
        }
    }
}

// ---------------------------------------------------------------------------
// SelectionDb — standalone persistence (JSON file)
// ---------------------------------------------------------------------------

/// Persisted map: source identifier (file path or URL) → list of deselected paths.
#[derive(Default, Serialize, Deserialize)]
struct SelectionDb {
    /// Source key → list of deselected endpoint paths.
    deselected: HashMap<String, Vec<String>>,
    /// Source key → full list of all endpoint paths at last export.
    /// Used to detect new/removed endpoints when the spec changes.
    #[serde(default)]
    all_paths: HashMap<String, Vec<String>>,
    /// Runtime-only: overridden file path for the DB.
    #[serde(skip)]
    custom_path: Option<PathBuf>,
}

impl SelectionDb {
    /// Platform-appropriate config directory for the app.
    fn config_dir() -> PathBuf {
        let base = if cfg!(target_os = "macos") {
            dirs().join("Library/Application Support")
        } else if cfg!(target_os = "windows") {
            std::env::var("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|_| dirs())
        } else {
            // XDG on Linux / others
            std::env::var("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| dirs().join(".config"))
        };
        base.join("openapi-edit")
    }

    fn db_path() -> PathBuf {
        Self::config_dir().join("selections.json")
    }

    fn load(custom_path: Option<PathBuf>) -> Self {
        let path = custom_path.clone().unwrap_or_else(Self::db_path);
        let mut db = match std::fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        };
        db.custom_path = custom_path;
        db
    }

    fn save(&self) {
        let path = self.custom_path.clone().unwrap_or_else(Self::db_path);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, json);
        }
    }

    fn has_selection(&self, key: &str) -> bool {
        self.deselected.contains_key(key)
    }
}

/// Helper: user home directory.
fn dirs() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

// ---------------------------------------------------------------------------
// App state
// ---------------------------------------------------------------------------

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
    /// If set, Export writes directly to this path (no file dialog).
    force_export_path: Option<PathBuf>,
    /// Currently open info popup (endpoint path), if any.
    info_endpoint: Option<String>,
}

impl App {
    fn new(
        _cc: &eframe::CreationContext<'_>,
        db_path: Option<PathBuf>,
        force_export_path: Option<PathBuf>,
    ) -> Self {
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
            selection_db: SelectionDb::load(db_path),
            force_export_path,
            info_endpoint: None,
        }
    }

    /// Apply saved selection from the db to the current groups.
    fn apply_saved_selection(&mut self) {
        let Some(key) = &self.source_key else {
            return;
        };
        let Some(saved_deselected) = self.selection_db.deselected.get(key) else {
            return;
        };

        let saved_set: HashSet<&str> = saved_deselected.iter().map(|s| s.as_str()).collect();

        // Apply deselection: keys are "METHOD /path"
        for group in &mut self.groups {
            for ep in &mut group.endpoints {
                for op in &mut ep.operations {
                    let key = format!("{} {}", op.method, ep.path);
                    if saved_set.contains(key.as_str()) {
                        op.selected = false;
                    }
                }
            }
        }

        let total: usize = self.groups.iter().map(|g| g.operation_count()).sum();
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
            .flat_map(|e| {
                e.operations
                    .iter()
                    .filter(|op| !op.selected)
                    .map(|op| format!("{} {}", op.method, e.path))
            })
            .collect();
        let all_paths: Vec<String> = self
            .groups
            .iter()
            .flat_map(|g| g.endpoints.iter())
            .flat_map(|e| {
                e.operations
                    .iter()
                    .map(|op| format!("{} {}", op.method, e.path))
            })
            .collect();
        if deselected.is_empty() {
            self.selection_db.deselected.remove(key);
        } else {
            self.selection_db.deselected.insert(key.clone(), deselected);
        }
        self.selection_db.all_paths.insert(key.clone(), all_paths);
        self.selection_db.save();
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
        SpecFormat::Json => {
            serde_json::from_str(&contents).map_err(|e| format!("JSON parse error: {e}"))?
        }
        SpecFormat::Yaml => {
            serde_yaml::from_str(&contents).map_err(|e| format!("YAML parse error: {e}"))?
        }
    };
    Ok((value, format))
}

fn parse_spec_string(contents: &str, format: SpecFormat) -> Result<Value, String> {
    match format {
        SpecFormat::Json => {
            serde_json::from_str(contents).map_err(|e| format!("JSON parse error: {e}"))
        }
        SpecFormat::Yaml => {
            serde_yaml::from_str(contents).map_err(|e| format!("YAML parse error: {e}"))
        }
    }
}

fn extract_groups(spec: &Value) -> Result<Vec<PathGroup>, String> {
    let paths_obj = spec
        .get("paths")
        .and_then(|v| v.as_object())
        .ok_or_else(|| "No 'paths' object found in spec.".to_string())?;

    let mut map: BTreeMap<String, Vec<Endpoint>> = BTreeMap::new();

    for (key, path_item) in paths_obj {
        let group_name = key.trim_start_matches('/').split('/').next().unwrap_or("/");
        let group_name = if group_name.is_empty() {
            "/"
        } else {
            group_name
        };

        let operations: Vec<Operation> = HTTP_METHODS
            .iter()
            .filter(|m| path_item.get(**m).is_some())
            .map(|m| Operation {
                method: m.to_string(),
                selected: true,
            })
            .collect();

        if !operations.is_empty() {
            map.entry(group_name.to_string())
                .or_default()
                .push(Endpoint {
                    path: key.clone(),
                    operations,
                });
        }
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

    // Build a map: path → set of deselected methods
    let mut deselected_ops: HashMap<&str, HashSet<&str>> = HashMap::new();
    let mut fully_deselected: HashSet<&str> = HashSet::new();

    for group in groups {
        for ep in &group.endpoints {
            if ep.none_selected() {
                fully_deselected.insert(ep.path.as_str());
            } else if !ep.all_selected() {
                let methods: HashSet<&str> = ep
                    .operations
                    .iter()
                    .filter(|op| !op.selected)
                    .map(|op| op.method.as_str())
                    .collect();
                deselected_ops.insert(ep.path.as_str(), methods);
            }
        }
    }

    if let Some(paths) = out.get_mut("paths").and_then(|v| v.as_object_mut()) {
        // Remove fully deselected paths
        paths.retain(|key, _| !fully_deselected.contains(key.as_str()));

        // Remove individual deselected methods from partially selected paths
        for (path_key, methods) in &deselected_ops {
            if let Some(path_item) = paths.get_mut(*path_key).and_then(|v| v.as_object_mut()) {
                path_item.retain(|method, _| !methods.contains(method.as_str()));
            }
        }
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
    components.retain(|_, v| v.as_object().map_or(true, |m| !m.is_empty()));
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
    fn save(&mut self, _storage: &mut dyn eframe::Storage) {
        // SelectionDb is saved to its own file (not eframe storage) so the
        // CLI can access it independently.
        self.selection_db.save();
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
                    let total: usize =
                        self.groups.iter().map(|g| g.operation_count()).sum();
                    ui.label(format!("Selected: {selected} / {total} operations"));
                    ui.separator();
                }
                ui.label(&self.status);
            });
            ui.add_space(2.0);
        });

        // -- Right detail panel (shown when an endpoint is selected for info) --
        if let Some(ref ep_path) = self.info_endpoint.clone() {
            egui::SidePanel::right("detail_panel")
                .default_width(350.0)
                .min_width(250.0)
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.heading(ep_path);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button("✕").clicked() {
                                self.info_endpoint = None;
                            }
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        if let Some(spec) = &self.spec {
                            Self::render_endpoint_info(ui, spec, ep_path);
                        }
                    });
                });
        }

        // -- Central panel: tree --
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.groups.is_empty() {
                ui.centered_and_justified(|ui| {
                    ui.label("No spec loaded. Click \"Open File…\" to begin.");
                });
                return;
            }

            let query = self.search_query.to_lowercase();
            let mut info_endpoint = None::<String>;

            egui::ScrollArea::vertical().show(ui, |ui| {
                for group in &mut self.groups {
                    // Filter endpoints by search query
                    let visible_indices: Vec<usize> = group
                        .endpoints
                        .iter()
                        .enumerate()
                        .filter(|(_, ep)| {
                            query.is_empty() || ep.path.to_lowercase().contains(&query)
                        })
                        .map(|(i, _)| i)
                        .collect();

                    if visible_indices.is_empty() && !query.is_empty() {
                        continue; // Skip group entirely if no matches
                    }

                    let op_count = if query.is_empty() {
                        group.operation_count()
                    } else {
                        visible_indices
                            .iter()
                            .map(|&i| group.endpoints[i].operation_count())
                            .sum()
                    };

                    // Group row: checkbox + collapsing header
                    let id = ui.make_persistent_id(&group.name);

                    ui.horizontal(|ui| {
                        // Tri-state checkbox for group
                        let all = group.all_selected();
                        let none = group.none_selected();
                        let mut state = !none;

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
                            "/{} ({} operation{})",
                            group.name,
                            op_count,
                            if op_count == 1 { "" } else { "s" }
                        );

                        egui::CollapsingHeader::new(egui::RichText::new(header_text).strong())
                            .id_salt(id)
                            .default_open(false)
                            .show(ui, |ui| {
                                let render_ep =
                                    |ui: &mut egui::Ui,
                                     ep: &mut Endpoint,
                                     info_ep: &mut Option<String>| {
                                        ui.horizontal(|ui| {
                                            // Endpoint-level tri-state checkbox
                                            let all = ep.all_selected();
                                            let none = ep.none_selected();
                                            let mut state = !none;

                                            let response = ui.checkbox(&mut state, "");

                                            if !all && !none {
                                                let rect = response.rect;
                                                let center = rect.center();
                                                let half = rect.width() * 0.2;
                                                ui.painter().line_segment(
                                                    [
                                                        egui::pos2(center.x - half, center.y),
                                                        egui::pos2(center.x + half, center.y),
                                                    ],
                                                    egui::Stroke::new(
                                                        2.0,
                                                        ui.visuals().text_color(),
                                                    ),
                                                );
                                            }

                                            if response.changed() {
                                                ep.set_all(state);
                                            }

                                            ui.label(&ep.path);

                                            if ui
                                                .small_button("i")
                                                .on_hover_text("Show endpoint details")
                                                .clicked()
                                            {
                                                *info_ep = Some(ep.path.clone());
                                            }
                                        });

                                        // Individual operation checkboxes
                                        ui.indent(ui.make_persistent_id(&ep.path), |ui| {
                                            for op in &mut ep.operations {
                                                ui.horizontal(|ui| {
                                                    ui.checkbox(&mut op.selected, "");
                                                    ui.label(
                                                        egui::RichText::new(
                                                            op.method.to_uppercase(),
                                                        )
                                                        .color(method_color(&op.method))
                                                        .strong(),
                                                    );
                                                });
                                            }
                                        });
                                    };
                                if query.is_empty() {
                                    for ep in &mut group.endpoints {
                                        render_ep(ui, ep, &mut info_endpoint);
                                    }
                                } else {
                                    for &idx in &visible_indices {
                                        let ep = &mut group.endpoints[idx];
                                        render_ep(ui, ep, &mut info_endpoint);
                                    }
                                }
                            });
                    });
                }
            });

            if let Some(path) = info_endpoint {
                self.info_endpoint = Some(path);
            }
        });
    }
}

impl App {
    fn render_endpoint_info(ui: &mut egui::Ui, spec: &Value, ep_path: &str) {
        let Some(path_item) = spec.get("paths").and_then(|p| p.get(ep_path)) else {
            ui.label("Endpoint not found in spec.");
            return;
        };

        let mut first = true;
        for method in HTTP_METHODS {
            let Some(op) = path_item.get(*method) else {
                continue;
            };

            if !first {
                ui.separator();
            }
            first = false;

            ui.heading(
                egui::RichText::new(method.to_uppercase())
                    .strong()
                    .color(method_color(*method)),
            );

            if let Some(summary) = op.get("summary").and_then(|v| v.as_str()) {
                ui.label(egui::RichText::new(summary).strong());
            }
            if let Some(desc) = op.get("description").and_then(|v| v.as_str()) {
                ui.label(desc);
            }
            if let Some(op_id) = op.get("operationId").and_then(|v| v.as_str()) {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("operationId:").weak());
                    ui.code(op_id);
                });
            }
            if let Some(tags) = op.get("tags").and_then(|v| v.as_array()) {
                let tag_strs: Vec<&str> = tags.iter().filter_map(|t| t.as_str()).collect();
                if !tag_strs.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("tags:").weak());
                        ui.label(tag_strs.join(", "));
                    });
                }
            }

            // Parameters
            let params: Vec<&Value> = path_item
                .get("parameters")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
                .chain(
                    op.get("parameters")
                        .and_then(|v| v.as_array())
                        .into_iter()
                        .flatten(),
                )
                .collect();

            if !params.is_empty() {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Parameters").strong());
                egui::Grid::new(format!("{ep_path}_{method}_params"))
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Name").weak());
                        ui.label(egui::RichText::new("In").weak());
                        ui.label(egui::RichText::new("Type").weak());
                        ui.label(egui::RichText::new("Required").weak());
                        ui.end_row();

                        for param in &params {
                            let name = param.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            let loc = param.get("in").and_then(|v| v.as_str()).unwrap_or("?");
                            let typ = param
                                .get("schema")
                                .and_then(|s| s.get("type"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("-");
                            let required = param
                                .get("required")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false);
                            ui.label(name);
                            ui.label(loc);
                            ui.label(typ);
                            ui.label(if required { "yes" } else { "no" });
                            ui.end_row();
                        }
                    });
            }

            // Request body
            if let Some(body) = op.get("requestBody") {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Request Body").strong());
                if let Some(desc) = body.get("description").and_then(|v| v.as_str()) {
                    ui.label(desc);
                }
                if let Some(content) = body.get("content").and_then(|v| v.as_object()) {
                    let types: Vec<&str> = content.keys().map(|k| k.as_str()).collect();
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("Content types:").weak());
                        ui.label(types.join(", "));
                    });
                }
            }

            // Responses
            if let Some(responses) = op.get("responses").and_then(|v| v.as_object()) {
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Responses").strong());
                egui::Grid::new(format!("{ep_path}_{method}_responses"))
                    .striped(true)
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new("Status").weak());
                        ui.label(egui::RichText::new("Description").weak());
                        ui.end_row();

                        for (status, resp) in responses {
                            let desc =
                                resp.get("description").and_then(|v| v.as_str()).unwrap_or("-");
                            ui.label(egui::RichText::new(status.as_str()).code());
                            ui.label(desc);
                            ui.end_row();
                        }
                    });
            }
        }

        if first {
            ui.label("No HTTP methods found for this endpoint.");
        }
    }

    fn load_from_path(&mut self, path: &Path) {
        match load_spec(path) {
            Ok((value, format)) => match extract_groups(&value) {
                Ok(groups) => {
                    let total: usize = groups.iter().map(|g| g.operation_count()).sum();
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    self.status = format!("Loaded {name} — {total} operations");
                    self.spec = Some(value);
                    self.source_format = format;
                    self.export_format = format;
                    self.groups = groups;
                    self.loaded_file_name = Some(name);
                    self.search_query.clear();
                    self.source_key = Some(canonical_source_key(path));
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

    fn load_from_string(
        &mut self,
        contents: &str,
        name: &str,
        source_key: &str,
        format: SpecFormat,
    ) {
        match parse_spec_string(contents, format) {
            Ok(value) => match extract_groups(&value) {
                Ok(groups) => {
                    let total: usize = groups.iter().map(|g| g.operation_count()).sum();
                    self.status = format!("Loaded {name} — {total} operations");
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
                let path = if let Some(forced) = &self.force_export_path {
                    Some(forced.clone())
                } else {
                    let default_name = format!("filtered.{}", self.export_format.extension());
                    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

                    rfd::FileDialog::new()
                        .set_directory(&cwd)
                        .set_file_name(&default_name)
                        .add_filter(
                            self.export_format.label(),
                            &[self.export_format.extension()],
                        )
                        .save_file()
                };

                if let Some(path) = path {
                    match std::fs::write(&path, &output) {
                        Ok(()) => {
                            self.save_selection();
                            let selected: usize =
                                self.groups.iter().map(|g| g.selected_count()).sum();
                            self.status =
                                format!("Exported {selected} operations to {}", path.display());
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
// Shared helpers
// ---------------------------------------------------------------------------

fn method_color(method: &str) -> egui::Color32 {
    match method {
        "get" => egui::Color32::from_rgb(97, 175, 254),
        "post" => egui::Color32::from_rgb(73, 204, 144),
        "put" => egui::Color32::from_rgb(252, 161, 48),
        "patch" => egui::Color32::from_rgb(80, 227, 194),
        "delete" => egui::Color32::from_rgb(249, 62, 62),
        _ => egui::Color32::from_rgb(183, 183, 183),
    }
}

/// Canonical source key for a local file path.
fn canonical_source_key(path: &Path) -> String {
    std::fs::canonicalize(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// Guess format from a URL path or fall back to YAML.
fn detect_format_from_url(url: &str) -> SpecFormat {
    if url.ends_with(".json") {
        SpecFormat::Json
    } else {
        SpecFormat::Yaml
    }
}

/// Download a URL synchronously, returning (contents, display_name, format).
fn download_spec(
    url: &str,
    basic_auth: Option<(&str, &str)>,
) -> Result<(String, String, SpecFormat), String> {
    let mut req = ureq::get(url);

    if let Some((user, password)) = basic_auth {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        req = req.header("Authorization", &format!("Basic {encoded}"));
    }

    let body: String = req
        .call()
        .map_err(|e| format!("HTTP request failed: {e}"))?
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("Failed to read response body: {e}"))?;

    let format = detect_format_from_url(url);
    let name = url.rsplit('/').next().unwrap_or(url).to_string();
    Ok((body, name, format))
}

// ---------------------------------------------------------------------------
// CLI export mode
// ---------------------------------------------------------------------------

/// Apply a saved selection to groups, detecting spec drift.
/// Prints warnings for removed operations and info for new ones.
/// Returns `true` if a saved selection was found and applied.
fn apply_saved_selection_cli(groups: &mut [PathGroup], db: &SelectionDb, source_key: &str) -> bool {
    let Some(saved_deselected) = db.deselected.get(source_key) else {
        return false;
    };

    let deselected_set: HashSet<&str> = saved_deselected.iter().map(|s| s.as_str()).collect();

    // Current operation keys: "METHOD /path"
    let current_ops: HashSet<String> = groups
        .iter()
        .flat_map(|g| g.endpoints.iter())
        .flat_map(|e| {
            e.operations
                .iter()
                .map(|op| format!("{} {}", op.method, e.path))
        })
        .collect();

    // Detect removed operations (were deselected but no longer in spec)
    let removed: Vec<&str> = saved_deselected
        .iter()
        .map(|s| s.as_str())
        .filter(|p| !current_ops.contains(*p))
        .collect();

    if !removed.is_empty() {
        eprintln!(
            "Warning: {} previously deselected operation(s) no longer exist in the spec:",
            removed.len()
        );
        for r in &removed {
            eprintln!("  - {r}");
        }
    }

    // Detect removed operations that were previously *selected*
    if let Some(saved_all) = db.all_paths.get(source_key) {
        let prev_set: HashSet<&str> = saved_all.iter().map(|s| s.as_str()).collect();
        let removed_selected: Vec<&str> = prev_set
            .iter()
            .filter(|p| !current_ops.contains(**p) && !deselected_set.contains(**p))
            .copied()
            .collect();
        if !removed_selected.is_empty() {
            eprintln!(
                "Warning: {} previously selected operation(s) no longer exist in the spec:",
                removed_selected.len()
            );
            for r in &removed_selected {
                eprintln!("  - {r}");
            }
        }

        // Detect new operations (in current spec but not in previous known set)
        let mut new_ops: Vec<&str> = current_ops
            .iter()
            .map(|s| s.as_str())
            .filter(|p| !prev_set.contains(p))
            .collect();
        new_ops.sort();
        if !new_ops.is_empty() {
            eprintln!(
                "Info: {} new operation(s) found in the spec (auto-selected):",
                new_ops.len()
            );
            for n in &new_ops {
                eprintln!("  + {n}");
            }
        }
    }

    // Apply deselection
    for group in groups.iter_mut() {
        for ep in &mut group.endpoints {
            for op in &mut ep.operations {
                let key = format!("{} {}", op.method, ep.path);
                if deselected_set.contains(key.as_str()) {
                    op.selected = false;
                }
            }
        }
    }

    true
}

/// Run a headless CLI export. Returns Ok(true) if export was performed,
/// Ok(false) if no saved selection exists (caller should launch GUI).
fn run_cli_export(
    source_key: &str,
    spec: Value,
    export_path: &Path,
    db_path: Option<PathBuf>,
) -> Result<bool, String> {
    let mut db = SelectionDb::load(db_path);

    if !db.has_selection(source_key) && !db.all_paths.contains_key(source_key) {
        return Ok(false);
    }

    let mut groups = extract_groups(&spec)?;
    let had_selection = apply_saved_selection_cli(&mut groups, &db, source_key);

    if !had_selection {
        return Ok(false);
    }

    let export_format = detect_format(export_path);
    let filtered = build_filtered_spec(&spec, &groups, true);
    let output = serialize_spec(&filtered, export_format)?;

    std::fs::write(export_path, &output).map_err(|e| format!("Write error: {e}"))?;

    let selected: usize = groups.iter().map(|g| g.selected_count()).sum();
    let total: usize = groups.iter().map(|g| g.operation_count()).sum();
    eprintln!(
        "Exported {selected}/{total} operations to {}",
        export_path.display()
    );

    // Update the db with current state (so all_paths stays fresh)
    let deselected: Vec<String> = groups
        .iter()
        .flat_map(|g| g.endpoints.iter())
        .flat_map(|e| {
            e.operations
                .iter()
                .filter(|op| !op.selected)
                .map(|op| format!("{} {}", op.method, e.path))
        })
        .collect();
    let all_paths: Vec<String> = groups
        .iter()
        .flat_map(|g| g.endpoints.iter())
        .flat_map(|e| {
            e.operations
                .iter()
                .map(|op| format!("{} {}", op.method, e.path))
        })
        .collect();
    if deselected.is_empty() {
        db.deselected.remove(source_key);
    } else {
        db.deselected.insert(source_key.to_string(), deselected);
    }
    db.all_paths.insert(source_key.to_string(), all_paths);
    db.save();

    Ok(true)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

struct CliArgs {
    input: Option<String>,
    export_path: Option<PathBuf>,
    db_path: Option<PathBuf>,
    user: Option<String>,
    password: Option<String>,
    force_export_path: Option<PathBuf>,
}

fn parse_args() -> CliArgs {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut input = None;
    let mut export_path = None;
    let mut db_path = None;
    let mut user = None;
    let mut password = None;
    let mut force_export_path = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--export" => {
                i += 1;
                if i < args.len() {
                    export_path = Some(PathBuf::from(&args[i]));
                } else {
                    eprintln!("Error: --export requires an output file path");
                    std::process::exit(1);
                }
            }
            "--db" => {
                i += 1;
                if i < args.len() {
                    db_path = Some(PathBuf::from(&args[i]));
                } else {
                    eprintln!("Error: --db requires a file path");
                    std::process::exit(1);
                }
            }
            "--user" => {
                i += 1;
                if i < args.len() {
                    user = Some(args[i].clone());
                } else {
                    eprintln!("Error: --user requires a username");
                    std::process::exit(1);
                }
            }
            "--password" => {
                i += 1;
                if i < args.len() {
                    password = Some(args[i].clone());
                } else {
                    eprintln!("Error: --password requires a password");
                    std::process::exit(1);
                }
            }
            "--force-export-path" => {
                i += 1;
                if i < args.len() {
                    force_export_path = Some(PathBuf::from(&args[i]));
                } else {
                    eprintln!("Error: --force-export-path requires a file path");
                    std::process::exit(1);
                }
            }
            "--help" | "-h" => {
                eprintln!("Usage: openapi-edit [INPUT] [OPTIONS]");
                eprintln!();
                eprintln!("  INPUT   Path or URL to an OpenAPI spec (YAML/JSON)");
                eprintln!("  --export OUTPUT");
                eprintln!("          Export filtered spec to OUTPUT without opening the GUI.");
                eprintln!("          Requires a previous GUI export to establish the selection.");
                eprintln!("          If no saved selection exists, the GUI opens instead.");
                eprintln!("  --force-export-path PATH");
                eprintln!("          In GUI mode, the Export button writes directly to PATH");
                eprintln!("          instead of opening a file dialog.");
                eprintln!("  --db PATH");
                eprintln!("          Use a custom path for the selections database file.");
                eprintln!("          Defaults to the platform config directory.");
                eprintln!("  --user USER");
                eprintln!("          Username for HTTP Basic Authentication (URL sources only).");
                eprintln!("  --password PASSWORD");
                eprintln!("          Password for HTTP Basic Authentication (URL sources only).");
                std::process::exit(0);
            }
            other => {
                input = Some(other.to_string());
            }
        }
        i += 1;
    }
    CliArgs {
        input,
        export_path,
        db_path,
        user,
        password,
        force_export_path,
    }
}

fn main() -> eframe::Result {
    let cli = parse_args();

    // Resolve input source
    let initial_source: Option<InitialSource> = match cli.input.as_deref() {
        Some(s) if s.starts_with("http://") || s.starts_with("https://") => {
            let basic_auth = cli
                .user
                .as_deref()
                .zip(cli.password.as_deref());
            match download_spec(s, basic_auth) {
                Ok((contents, name, format)) => Some(InitialSource::Downloaded {
                    contents,
                    name,
                    source_url: s.to_string(),
                    format,
                }),
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Some(path) => Some(InitialSource::File(PathBuf::from(path))),
        None => None,
    };

    // --export: attempt headless CLI export
    if let Some(export_path) = &cli.export_path {
        let Some(ref source) = initial_source else {
            eprintln!("Error: --export requires an input file or URL");
            std::process::exit(1);
        };

        let (spec, source_key) = match source {
            InitialSource::File(path) => {
                let (spec, _fmt) = load_spec(path).unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                });
                let key = canonical_source_key(path);
                (spec, key)
            }
            InitialSource::Downloaded {
                contents,
                source_url,
                format,
                ..
            } => {
                let spec = parse_spec_string(contents, *format).unwrap_or_else(|e| {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                });
                (spec, source_url.clone())
            }
        };

        match run_cli_export(&source_key, spec, export_path, cli.db_path.clone()) {
            Ok(true) => std::process::exit(0),
            Ok(false) => {
                eprintln!(
                    "No saved selection found for this source. Opening GUI for initial setup…"
                );
                // Fall through to GUI below
            }
            Err(e) => {
                eprintln!("Error: {e}");
                std::process::exit(1);
            }
        }
    }

    // GUI mode
    let icon_png = include_bytes!("../media/logo.png");
    let icon_image = image::load_from_memory(icon_png).expect("Failed to decode embedded icon");
    let icon_rgba = icon_image.to_rgba8();
    let (icon_w, icon_h) = icon_image.dimensions();
    let icon = egui::IconData {
        rgba: icon_rgba.into_raw(),
        width: icon_w,
        height: icon_h,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([745.0, 700.0])
            .with_min_inner_size([400.0, 300.0])
            .with_icon(icon),
        ..Default::default()
    };

    eframe::run_native(
        "OpenAPI Editor",
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc, cli.db_path, cli.force_export_path);
            match initial_source {
                Some(InitialSource::File(path)) => app.load_from_path(&path),
                Some(InitialSource::Downloaded {
                    contents,
                    name,
                    source_url,
                    format,
                }) => {
                    app.load_from_string(&contents, &name, &source_url, format);
                }
                None => {}
            }
            Ok(Box::new(app))
        }),
    )
}

enum InitialSource {
    File(PathBuf),
    Downloaded {
        contents: String,
        name: String,
        source_url: String,
        format: SpecFormat,
    },
}
