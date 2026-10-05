use ribolita::{ApplyReport, Query, RedbStore, RunEnvironment};
use ribolita_common::{entry::Entry, rmp_json};
use ribolita_node::{SharedEnv, net};
use slint::{Model, ModelRc, SharedString, VecModel};
use std::{
    cell::RefCell,
    collections::{BTreeMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, MutexGuard, OnceLock},
};

static ANDROID_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

slint::include_modules!();
include!(concat!(env!("OUT_DIR"), "/embedded_wasm.rs"));
include!(concat!(env!("OUT_DIR"), "/embedded_metadata.rs"));

type Env = RunEnvironment<RedbStore>;

#[derive(Debug, Clone, serde::Deserialize)]
struct AppMetadata {
    name: Option<String>,
    kinds: BTreeMap<String, KindMetadata>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
struct KindMetadata {
    description: Option<String>,
    #[serde(default)]
    values: BTreeMap<String, FieldDef>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
struct FieldDef {
    #[serde(rename = "type")]
    field_type: String,
    optional: Option<bool>,
    description: Option<String>,
}

struct AppState {
    /// shared with the sync node, which applies entries it receives
    env: SharedEnv<RedbStore>,
    metadata: Option<AppMetadata>,
    entries: Vec<Entry>,
    entries_model: Rc<VecModel<EntrySummary>>,
    new_entry_model: Rc<VecModel<KeyValue>>,
    edit_entry_model: Rc<VecModel<KeyValue>>,
}

impl AppState {
    fn env(&self) -> MutexGuard<'_, Env> {
        self.env
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn field_types(&self, kind: &str) -> Option<&BTreeMap<String, FieldDef>> {
        self.metadata
            .as_ref()
            .and_then(|m| m.kinds.get(kind))
            .map(|k| &k.values)
    }

    /// value rows for a new entry of `kind`: one empty row per declared field
    fn rows_for_kind(&self, kind: &str) -> Vec<KeyValue> {
        let rows: Vec<KeyValue> = self
            .field_types(kind)
            .map(|fields| fields.keys().map(|name| kv(name, "")).collect())
            .unwrap_or_default();
        if rows.is_empty() {
            vec![kv("", "")]
        } else {
            rows
        }
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
fn android_main(app: slint::android::AndroidApp) {
    if let Some(path) = app.inner().internal_data_path() {
        let _ = ANDROID_DATA_DIR.set(path.to_path_buf());
    }
    slint::android::init(app).expect("slint android backend init");
    main().unwrap();
}

pub fn main() -> Result<(), slint::PlatformError> {
    let app = AppWindow::new()?;
    let data_dir = default_data_dir();

    let metadata: Option<AppMetadata> = match serde_json::from_str(METADATA_JSON) {
        Ok(metadata) => Some(metadata),
        Err(err) => {
            eprintln!("ignoring invalid metadata.json: {err}");
            None
        }
    };
    let program_name = metadata
        .as_ref()
        .and_then(|m| m.name.clone())
        .unwrap_or_else(|| "program".to_string());
    app.set_app_title(program_name.as_str().into());

    let secret = net::load_or_create_secret(&data_dir)
        .map_err(|err| slint::PlatformError::Other(format!("failed to load node key: {err}")))?;
    let program_id = slug(&program_name);
    app.set_program_id(program_id.as_str().into());
    let env = open_env(&data_dir, &program_id, net::author_of(&secret))
        .map_err(|err| slint::PlatformError::Other(format!("failed to open program: {err}")))?;
    let env = Arc::new(Mutex::new(env));

    let kind_names: Vec<SharedString> = metadata
        .as_ref()
        .map(|m| m.kinds.keys().map(|k| k.as_str().into()).collect())
        .unwrap_or_default();
    app.set_kind_names(ModelRc::new(VecModel::from(kind_names.clone())));

    let state = Rc::new(RefCell::new(AppState {
        env: env.clone(),
        metadata,
        entries: Vec::new(),
        entries_model: Rc::new(VecModel::default()),
        new_entry_model: Rc::new(VecModel::default()),
        edit_entry_model: Rc::new(VecModel::default()),
    }));
    {
        let state = state.borrow();
        let first_kind = kind_names.first().cloned().unwrap_or_default();
        app.set_new_entry_kind(first_kind.clone());
        state
            .new_entry_model
            .set_vec(state.rows_for_kind(first_kind.as_str()));
        app.set_new_entry_values(ModelRc::from(state.new_entry_model.clone()));
        app.set_entry_values(ModelRc::from(state.edit_entry_model.clone()));
        app.set_entries(ModelRc::from(state.entries_model.clone()));
    }

    // sync with other devices over iroh
    let host = net::SingleProgram {
        program: program_id.clone(),
        env,
    };
    let (node, node_events) = net::spawn(host, secret);
    app.set_node_id(node.node_id.clone().into());
    let connected_peers_model = Rc::new(VecModel::<SharedString>::default());
    app.set_connected_peers(ModelRc::from(connected_peers_model.clone()));
    app.set_data_dir(data_dir.to_string_lossy().to_string().into());

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    app.on_refresh_state(move || {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let json = state_handle
            .borrow()
            .env()
            .state_json(app.get_state_path().as_str());
        match json {
            Ok(json) => {
                let pretty = serde_json::to_string_pretty(&json).unwrap_or_default();
                app.set_state_json(pretty.into());
                app.set_state_error("".into());
            }
            Err(err) => app.set_state_error(format!("{err:#}").into()),
        }
    });

    // new entry form
    let state_handle = state.clone();
    app.on_new_entry_kind_selected(move |kind| {
        let state = state_handle.borrow();
        state.new_entry_model.set_vec(state.rows_for_kind(&kind));
    });
    let state_handle = state.clone();
    app.on_add_new_entry_row(move || state_handle.borrow().new_entry_model.push(kv("", "")));
    let state_handle = state.clone();
    app.on_remove_new_entry_row(move |index| {
        remove_row(&state_handle.borrow().new_entry_model, index)
    });

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    let node_cmd = node.cmd_tx.clone();
    app.on_submit_new_entry(move || {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let result = (|| {
            let state = state_handle.borrow();
            let kind = app.get_new_entry_kind().trim().to_string();
            if kind.is_empty() {
                return Err("Kind is required".to_string());
            }
            let when = parse_when(&app.get_new_entry_when())?;
            let values = collect_values(&state.new_entry_model, state.field_types(&kind))?;
            let (entry, report) = state
                .env()
                .add_with_report(kind, values, when)
                .map_err(|err| format!("{err}"))?;
            node_cmd.send(net::Cmd::Sync { program: None }).ok();
            state
                .new_entry_model
                .set_vec(state.rows_for_kind(&entry.kind));
            Ok(report)
        })();
        match result {
            Ok(report) => {
                // added, but it changed how later entries play out
                let warning = report.failed.first().map(|(id, err)| {
                    format!(
                        "Added, but {} later entries are now rejected, e.g. {id}: {err}",
                        report.failed.len()
                    )
                });
                app.set_new_entry_error(warning.unwrap_or_default().into());
                app.set_new_entry_when("".into());
                refresh_entries(&app, &state_handle);
                app.invoke_refresh_state();
            }
            Err(err) => app.set_new_entry_error(err.into()),
        }
    });

    // entries list and editor
    let app_handle = app.as_weak();
    let state_handle = state.clone();
    app.on_refresh_entries(move || {
        if let Some(app) = app_handle.upgrade() {
            refresh_entries(&app, &state_handle);
        }
    });

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    app.on_select_entry(move |index| {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let state = state_handle.borrow();
        let Some(entry) = usize::try_from(index)
            .ok()
            .and_then(|i| state.entries.get(i))
        else {
            return;
        };
        app.set_selected_entry_index(index);
        app.set_entry_kind(entry.kind.as_str().into());
        app.set_entry_when(format_when(entry.when).into());
        app.set_entry_editor_error("".into());
        state.edit_entry_model.set_vec(
            entry
                .values
                .iter()
                .map(|(k, v)| kv(k, &value_to_text(v)))
                .collect::<Vec<_>>(),
        );
    });
    let state_handle = state.clone();
    app.on_entry_kind_selected(move |kind| {
        let state = state_handle.borrow();
        state.edit_entry_model.set_vec(state.rows_for_kind(&kind));
    });
    let state_handle = state.clone();
    app.on_add_entry_row(move || state_handle.borrow().edit_entry_model.push(kv("", "")));
    let state_handle = state.clone();
    app.on_remove_entry_row(move |index| {
        remove_row(&state_handle.borrow().edit_entry_model, index)
    });

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    let node_cmd = node.cmd_tx.clone();
    app.on_delete_entry(move || {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let result = (|| {
            let state = state_handle.borrow();
            let id = usize::try_from(app.get_selected_entry_index())
                .ok()
                .and_then(|i| state.entries.get(i))
                .map(|entry| entry.id.clone())
                .ok_or("Select an entry to delete")?;
            let report = state.env().delete(&id).map_err(|err| format!("{err}"))?;
            node_cmd.send(net::Cmd::Sync { program: None }).ok();
            Ok::<_, String>(report)
        })();
        after_change(&app, &state_handle, result);
    });

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    let node_cmd = node.cmd_tx.clone();
    app.on_save_entry(move || {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let result = (|| {
            let state = state_handle.borrow();
            let original = usize::try_from(app.get_selected_entry_index())
                .ok()
                .and_then(|i| state.entries.get(i))
                .cloned()
                .ok_or("Select an entry to edit")?;
            let kind = app.get_entry_kind().trim().to_string();
            if kind.is_empty() {
                return Err("Kind is required".to_string());
            }
            let when = parse_when(&app.get_entry_when())?;
            let values = collect_values(&state.edit_entry_model, state.field_types(&kind))?;
            // replaces it with a new entry at the same time (or the one given); entries after
            // it are replayed on top of the replacement
            let (_, report) = state
                .env()
                .edit(&original.id, when, kind, values)
                .map_err(|err| format!("{err}"))?;
            node_cmd.send(net::Cmd::Sync { program: None }).ok();
            Ok(report)
        })();
        after_change(&app, &state_handle, result);
    });

    let app_handle = app.as_weak();
    let node_cmd = node.cmd_tx.clone();
    app.on_connect_peer(move || {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let peer = app.get_peer_connect_input().trim().to_string();
        // servers want the program's api key; other devices don't need one
        let token = Some(app.get_peer_token_input().trim().to_string()).filter(|t| !t.is_empty());
        if !peer.is_empty() {
            let program = program_id.clone();
            if node_cmd.send(net::Cmd::Connect { peer, program, token }).is_err() {
                eprintln!("connect_peer: sync node is gone");
            }
            app.set_peer_connect_input("".into());
        }
    });

    refresh_entries(&app, &state);
    app.invoke_refresh_state();

    // sync node events
    let timer = slint::Timer::default();
    let app_handle = app.as_weak();
    let state_handle = state.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(200),
        move || {
            let Some(app) = app_handle.upgrade() else {
                return;
            };
            while let Ok(event) = node_events.try_recv() {
                match event {
                    net::Event::Synced {
                        peer,
                        received,
                        report,
                        ..
                    } if received > 0 => {
                        eprintln!("synced {received} entries from {peer}");
                        for (id, err) in report.failed {
                            eprintln!("entry {id} from {peer} was rejected: {err}");
                        }
                        refresh_entries(&app, &state_handle);
                        app.invoke_refresh_state();
                    }
                    net::Event::PeerConnected(id) => connected_peers_model.push(id.into()),
                    net::Event::PeerDisconnected(id) => {
                        if let Some(i) = connected_peers_model.iter().position(|p| p == id.as_str())
                        {
                            connected_peers_model.remove(i);
                        }
                    }
                    net::Event::Synced { .. } => {}
                    net::Event::Error(msg) => eprintln!("sync: {msg}"),
                }
            }
        },
    );

    app.run()
}

/// Opens the program's databases. They are keyed by program name, not by module, so a new build
/// of the program keeps its entries; when the module changed, the state is rebuilt from them.
fn open_env(
    data_dir: &std::path::Path,
    program_id: &str,
    author: ribolita_common::entry::Author,
) -> Result<Env, String> {
    let db_dir = data_dir.join("programs").join(program_id);
    std::fs::create_dir_all(&db_dir).map_err(|e| e.to_string())?;
    let mut env = Env::new(&db_dir, WASM, author).map_err(|e| e.to_string())?;

    let mut hasher = DefaultHasher::new();
    WASM.hash(&mut hasher);
    let hash = format!("{:016x}", hasher.finish());
    let hash_path = db_dir.join("program.hash");
    if std::fs::read_to_string(&hash_path).ok().as_deref() != Some(hash.as_str()) {
        let report = env
            .replay()
            .map_err(|e| format!("replaying with new program: {e}"))?;
        eprintln!(
            "program changed, replayed {} entries ({} rejected)",
            report.applied,
            report.failed.len()
        );
        std::fs::write(&hash_path, hash).map_err(|e| e.to_string())?;
    }
    Ok(env)
}

/// Refreshes the views after an edit or delete, or shows why it failed.
fn after_change(
    app: &AppWindow,
    state_handle: &Rc<RefCell<AppState>>,
    result: Result<ApplyReport, String>,
) {
    match result {
        Ok(report) => {
            refresh_entries(app, state_handle);
            app.invoke_refresh_state();
            if let Some((id, err)) = report.failed.first() {
                app.set_entries_error(
                    format!(
                        "{} entries are rejected by the program, e.g. {id}: {err}",
                        report.failed.len()
                    )
                    .into(),
                );
            }
        }
        Err(err) => app.set_entry_editor_error(err.into()),
    }
}

fn refresh_entries(app: &AppWindow, state_handle: &Rc<RefCell<AppState>>) {
    let mut state = state_handle.borrow_mut();
    let result = state.env().entries(Query::ByTime {
        since: None,
        until: None,
        limit: Some(50),
    });
    let entries = match result {
        Ok(entries) => entries,
        Err(err) => {
            app.set_entries_error(format!("Failed to query entries: {err}").into());
            return;
        }
    };
    state.entries_model.set_vec(
        entries
            .iter()
            .map(|entry| EntrySummary {
                author: entry.author.to_hex()[..8].into(),
                when: format_when(entry.when).into(),
                kind: entry.kind.as_str().into(),
            })
            .collect::<Vec<_>>(),
    );
    state.entries = entries;
    app.set_entries_error("".into());
    app.set_selected_entry_index(-1);
}

fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(android_dir) = ANDROID_DATA_DIR.get() {
        return android_dir.join("ribolita");
    }
    let base = dirs::data_local_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ribolita")
}

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: value.into(),
    }
}

fn remove_row(model: &VecModel<KeyValue>, index: i32) {
    if let Ok(i) = usize::try_from(index)
        && i < model.row_count()
    {
        model.remove(i);
    }
}

/// The id a program is synced under (on servers too): its name, lowercased, with anything but
/// letters and digits turned into dashes.
fn slug(program_name: &str) -> String {
    program_name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect()
}

/// Parses the editor's "YYYY-M-D H:M" (UTC) or a raw unix timestamp, into seconds. Empty means
/// "now".
fn parse_when(input: &str) -> Result<Option<u64>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if let Ok(ts) = trimmed.parse::<u64>() {
        return Ok(Some(ts));
    }
    let nums: Vec<u32> = trimmed
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty())
        .map(|s| s.parse().map_err(|_| format!("Invalid date: {trimmed}")))
        .collect::<Result<_, _>>()?;
    let [year, month, day, hour, minute] = nums[..] else {
        return Err(format!("Invalid date: {trimmed}"));
    };
    chrono::NaiveDate::from_ymd_opt(year as i32, month, day)
        .and_then(|d| d.and_hms_opt(hour, minute, 0))
        .and_then(|dt| u64::try_from(dt.and_utc().timestamp()).ok())
        .map(Some)
        .ok_or_else(|| format!("Invalid date: {trimmed}"))
}

fn format_when(secs: u64) -> String {
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(secs as i64, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| secs.to_string())
}

/// Text shown in the editor for a value: strings are shown bare, everything else as JSON.
fn value_to_text(value: &rmpv::Value) -> String {
    match value.as_str() {
        Some(s) => s.to_string(),
        None => rmp_json::rmp_value_to_json_value(value).to_string(),
    }
}

/// Reads the editor rows as entry values. Values are JSON, except that fields declared as
/// strings (or text that isn't valid JSON) are taken verbatim, so nobody has to type quotes.
fn collect_values(
    model: &VecModel<KeyValue>,
    field_types: Option<&BTreeMap<String, FieldDef>>,
) -> Result<Vec<(String, rmpv::Value)>, String> {
    let mut values = Vec::new();
    for row in model.iter() {
        let key = row.key.trim().to_string();
        let value = row.value.trim();
        if value.is_empty() {
            let optional = field_types
                .and_then(|f| f.get(&key))
                .is_none_or(|f| f.optional.unwrap_or(false));
            if key.is_empty() || optional {
                continue;
            }
            return Err(format!("Value for '{key}' is required"));
        }
        if key.is_empty() {
            return Err("Key cannot be empty".to_string());
        }
        let is_string = field_types
            .and_then(|f| f.get(&key))
            .is_some_and(|f| f.field_type == "string");
        let value = match serde_json::from_str::<serde_json::Value>(value) {
            Ok(serde_json::Value::String(s)) => rmpv::Value::String(s.into()),
            Ok(json) if !is_string => rmp_json::json_value_to_rmp_value(&json),
            _ => rmpv::Value::String(value.into()),
        };
        values.push((key, value));
    }
    Ok(values)
}
