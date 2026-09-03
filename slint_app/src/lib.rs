mod iroh_net;

use ribolita::{
    EntriesDatabase, Query, ReadTransaction, RunEnvironment, StateDatabase, StateRedbDatabase,
};
use ribolita_common::{entry::Entry, key, rmp_json, state::State, value};
use slint::{Model, ModelRc, SharedString, VecModel};
use std::{
    cell::RefCell,
    collections::hash_map::DefaultHasher,
    hash::{Hash, Hasher},
    path::PathBuf,
    rc::Rc,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

static ANDROID_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

slint::include_modules!();
include!(concat!(env!("OUT_DIR"), "/embedded_wasm.rs"));
include!(concat!(env!("OUT_DIR"), "/embedded_metadata.rs"));

#[derive(Debug, Clone, serde::Deserialize)]
struct AppMetadata {
    name: Option<String>,
    kinds: std::collections::BTreeMap<String, KindMetadata>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
struct KindMetadata {
    description: Option<String>,
    values: Vec<FieldDef>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[allow(dead_code)]
struct FieldDef {
    name: String,
    #[serde(rename = "type")]
    field_type: String,
    optional: Option<bool>,
}

struct AppState {
    program: LoadedProgram,
    entries: Vec<Entry>,
    entries_db: Rc<EntriesDatabase>,
    entries_model: Rc<VecModel<EntrySummary>>,
    new_entry_model: Rc<VecModel<KeyValueRow>>,
    edit_entry_model: Rc<VecModel<KeyValueRow>>,
    metadata: Option<AppMetadata>,
    kind_order: Vec<String>,
}

#[derive(Clone)]
struct LoadedProgram {
    wasm: Arc<Vec<u8>>,
    db_dir: PathBuf,
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

    let wasm_bytes = Arc::new(WASM.to_vec());
    let hasher = DefaultHasher::new();
    let hash = (&wasm_bytes as &[u8]).iter().fold(hasher, |mut h, b| {
        b.hash(&mut h);
        h
    })
    .finish();
    let db_dir = data_dir
        .join("programs")
        .join(format!("embedded-{hash:016x}"));
    if let Err(err) = std::fs::create_dir_all(&db_dir) {
        eprintln!("Failed to create data dir: {err}");
    }
    let program = LoadedProgram {
        wasm: wasm_bytes,
        db_dir,
    };

    let entries_db = Rc::new(init_entries_db(&program.db_dir));

    let entries_model = Rc::new(VecModel::from(Vec::<EntrySummary>::new()));
    let new_entry_model = Rc::new(VecModel::from(vec![KeyValueRow {
        key: SharedString::from(""),
        value: SharedString::from(""),
    }]));
    let edit_entry_model = Rc::new(VecModel::from(Vec::<KeyValueRow>::new()));

    let metadata: Option<AppMetadata> = if !METADATA_JSON.is_empty() {
        serde_json::from_str(METADATA_JSON).ok()
    } else {
        None
    };
    if let Some(name) = metadata.as_ref().and_then(|m| m.name.as_deref()) {
        app.set_app_title(name.into());
    }
    let kind_order: Vec<String> = metadata
        .as_ref()
        .map(|m| m.kinds.keys().cloned().collect())
        .unwrap_or_default();
    let kind_names: Vec<SharedString> =
        kind_order.iter().map(|k| SharedString::from(k.as_str())).collect();
    let kind_model = Rc::new(VecModel::from(kind_names));
    app.set_kind_names(ModelRc::from(kind_model.clone()));

    if !kind_order.is_empty() {
        app.set_new_kind_index(0);
        app.set_new_entry_kind(kind_order[0].as_str().into());
    }

    let has_kinds = !kind_order.is_empty();

    let state = Rc::new(RefCell::new(AppState {
        program,
        entries: Vec::new(),
        entries_db: entries_db.clone(),
        entries_model: entries_model.clone(),
        new_entry_model: new_entry_model.clone(),
        edit_entry_model: edit_entry_model.clone(),
        metadata,
        kind_order,
    }));

    // populate fields for the first kind
    if has_kinds {
        let mut state_mut = state.borrow_mut();
        populate_fields_for_kind(&mut state_mut, &app, 0);
    }

    // iroh networking
    let (iroh_handle, iroh_events) = iroh_net::spawn(data_dir.clone());
    app.set_node_id(iroh_handle.node_id.clone().into());
    let iroh_cmd = iroh_handle.cmd_tx.clone();
    let iroh_cmd2 = iroh_handle.cmd_tx.clone();

    let connected_peers_model = Rc::new(VecModel::<SharedString>::from(Vec::new()));
    app.set_connected_peers(ModelRc::from(connected_peers_model.clone()));

    app.set_data_dir(data_dir.to_string_lossy().to_string().into());
    app.set_entries(ModelRc::from(entries_model));
    app.set_new_entry_values(ModelRc::from(new_entry_model));
    app.set_entry_values(ModelRc::from(edit_entry_model));
    app.set_selected_entry_index(-1);

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    app.on_refresh_state(move || {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let state_path = app.get_state_path();
        let program = state_handle.borrow().program.clone();

        let state_dir = program.db_dir.join("state");
        if let Err(err) = std::fs::create_dir_all(&state_dir) {
            app.set_state_error(format!("Failed to open state dir: {err}").into());
            return;
        }
        let state_db = match StateRedbDatabase::init(&state_dir) {
            Ok(db) => db,
            Err(err) => {
                app.set_state_error(format!("Failed to open state db: {err}").into());
                return;
            }
        };
        let txn = match state_db.start_read_txn() {
            Ok(txn) => txn,
            Err(err) => {
                app.set_state_error(format!("State read failed: {err}").into());
                return;
            }
        };
        let path_parts = if state_path.is_empty() {
            Vec::new()
        } else {
            key::parse(state_path.as_str())
        };
        let prefix = key::to_bytes(&path_parts);
        let (num_rows, data) = match txn.get(&prefix) {
            Ok(result) => result,
            Err(err) => {
                app.set_state_error(format!("State query failed: {err}").into());
                return;
            }
        };
        let state_value: State = match value::from_bytes(num_rows, &data) {
            Ok(val) => val,
            Err(err) => {
                app.set_state_error(format!("Failed to decode state: {err}").into());
                return;
            }
        };
        let json_value = rmp_json::rmp_value_to_json_value(&state_value.0);
        let pretty =
            serde_json::to_string_pretty(&json_value).unwrap_or_else(|_| "{}".to_string());
        app.set_state_json(pretty.into());
        app.set_state_error("".into());
    });

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    app.on_add_new_entry_row(move || {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let state = state_handle.borrow_mut();
        state.new_entry_model.push(KeyValueRow {
            key: SharedString::from(""),
            value: SharedString::from(""),
        });
        app.set_new_entry_values(ModelRc::from(state.new_entry_model.clone()));
    });

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    app.on_update_new_entry_row(move |index, key, value| {
        let Some(_app) = app_handle.upgrade() else {
            return;
        };
        let state = state_handle.borrow_mut();
        if index < 0 {
            return;
        }
        let idx = index as usize;
        if idx >= state.new_entry_model.row_count() {
            return;
        }
        state
            .new_entry_model
            .set_row_data(idx, KeyValueRow { key, value });
    });

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    app.on_remove_new_entry_row(move |index| {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let state = state_handle.borrow_mut();
        if index < 0 {
            return;
        }
        let idx = index as usize;
        if idx >= state.new_entry_model.row_count() {
            return;
        }
        state.new_entry_model.remove(idx);
        app.set_new_entry_values(ModelRc::from(state.new_entry_model.clone()));
    });

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    app.on_submit_new_entry(move || {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let program = state_handle.borrow().program.clone();
        let kind = app.get_new_entry_kind();
        if kind.trim().is_empty() {
            app.set_new_entry_error("Kind is required".into());
            return;
        }
        let when = match parse_when(app.get_new_entry_when().as_str()) {
            Ok(Some(ts)) => ts,
            Ok(None) => now_timestamp(),
            Err(err) => {
                app.set_new_entry_error(err.into());
                return;
            }
        };
        let values = {
            let state = state_handle.borrow();
            match collect_values(&state.new_entry_model) {
                Ok(vals) => vals,
                Err(err) => {
                    app.set_new_entry_error(err.into());
                    return;
                }
            }
        };
        let entry = Entry {
            id: 0,
            when,
            kind: kind.to_string(),
            values,
        };
        let net_entry = entry.clone();
        let mut env: RunEnvironment<StateRedbDatabase> =
            match RunEnvironment::new(&program.db_dir, program.wasm.as_slice()) {
                Ok(env) => env,
                Err(err) => {
                    app.set_new_entry_error(format!("Failed to load wasm: {err}").into());
                    return;
                }
            };
        if let Err(err) = env.run(entry) {
            app.set_new_entry_error(format!("Entry run failed: {err}").into());
            return;
        }
        iroh_cmd.send(iroh_net::Cmd::PublishEntry(net_entry)).ok();
        app.set_new_entry_error("".into());
        app.set_new_entry_when("".into());
        let mut state = state_handle.borrow_mut();
        let current_index = app.get_new_kind_index();
        if current_index >= 0 && state.metadata.is_some() {
            populate_fields_for_kind(&mut state, &app, current_index as usize);
        } else {
            state.new_entry_model = Rc::new(VecModel::from(vec![KeyValueRow {
                key: SharedString::from(""),
                value: SharedString::from(""),
            }]));
            app.set_new_entry_values(ModelRc::from(state.new_entry_model.clone()));
        }
        drop(state);
        refresh_entries(&app_handle, &state_handle);
    });

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    app.on_refresh_entries(move || {
        refresh_entries(&app_handle, &state_handle);
    });

    let app_handle = app.as_weak();
    app.on_connect_peer(move || {
        eprintln!("connect_peer callback triggered");
        let Some(app) = app_handle.upgrade() else {
            eprintln!("connect_peer: app gone");
            return;
        };
        let peer_id = app.get_peer_connect_input().to_string();
        eprintln!("connect_peer: input='{peer_id}'");
        if !peer_id.is_empty() {
            eprintln!("connect_peer: sending Cmd::ConnectPeer({peer_id})");
            if iroh_cmd2.send(iroh_net::Cmd::ConnectPeer(peer_id)).is_err() {
                eprintln!("connect_peer: cmd_tx send failed (iroh thread dead?)");
            }
            app.set_peer_connect_input("".into());
        }
    });

    let app_handle = app.as_weak();
    let state_handle = state.clone();
    app.on_save_entry(move || {
        let Some(app) = app_handle.upgrade() else {
            return;
        };
        let selected = app.get_selected_entry_index();
        if selected < 0 {
            app.set_entry_editor_error("Select an entry to edit".into());
            return;
        }
        let idx = selected as usize;
        let mut state = state_handle.borrow_mut();
        if idx >= state.entries.len() {
            app.set_entry_editor_error("Invalid entry selection".into());
            return;
        }
        let original = state.entries[idx].clone();
        let kind = app.get_entry_kind();
        if kind.trim().is_empty() {
            app.set_entry_editor_error("Kind is required".into());
            return;
        }
        let when = match parse_when(app.get_entry_when().as_str()) {
            Ok(Some(ts)) => ts,
            Ok(None) => original.when,
            Err(err) => {
                app.set_entry_editor_error(err.into());
                return;
            }
        };
        let values = match collect_values(&state.edit_entry_model) {
            Ok(vals) => vals,
            Err(err) => {
                app.set_entry_editor_error(err.into());
                return;
            }
        };
        let updated = Entry {
            id: original.id,
            when,
            kind: kind.to_string(),
            values,
        };

        let entries_db = state.entries_db.clone();
        let txn = match entries_db.start_write_txn() {
            Ok(txn) => txn,
            Err(err) => {
                app.set_entry_editor_error(format!("Entries write failed: {err}").into());
                return;
            }
        };
        if let Err(err) = entries_db.save(&txn, updated.id, &updated) {
            app.set_entry_editor_error(format!("Failed to save entry: {err}").into());
            return;
        }
        if let Err(err) = txn.commit() {
            app.set_entry_editor_error(format!("Failed to commit entry: {err}").into());
            return;
        }

        state.entries[idx] = updated.clone();
        state.entries_model.set_row_data(
            idx,
            EntrySummary {
                id: updated.id as i32,
                when: SharedString::from(format_ts(updated.when)),
                kind: SharedString::from(updated.kind.clone()),
            },
        );
        app.set_entry_editor_error("".into());
    });

    refresh_entries(&app.as_weak(), &state);

    // iroh event poller
    let timer = slint::Timer::default();
    let timer_state = state.clone();
    let timer_app = app.as_weak();
    let timer_peers = connected_peers_model.clone();
    let timer_entries = entries_db.clone();
    timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(200),
        move || {
            while let Ok(event) = iroh_events.try_recv() {
                match event {
                    iroh_net::Event::EntryReceived(mut entry) => {
                        let last_id = timer_entries
                            .start_read_txn()
                            .ok()
                            .and_then(|rt| timer_entries.get_last_id(&rt).ok())
                            .unwrap_or(0);
                        entry.id = last_id + 1;
                        if let Ok(wt) = timer_entries.start_write_txn() {
                            if timer_entries.save(&wt, entry.id, &entry).is_ok() {
                                let _ = wt.commit();
                            }
                        }
                        refresh_entries(&timer_app, &timer_state);
                    }
                    iroh_net::Event::NodeId(id) => {
                        if let Some(app) = timer_app.upgrade() {
                            app.set_node_id(id.into());
                        }
                    }
                    iroh_net::Event::PeerConnected(id) => {
                        timer_peers.push(id.into());
                    }
                    iroh_net::Event::PeerDisconnected(id) => {
                        let s = SharedString::from(id.as_str());
                        for i in 0..timer_peers.row_count() {
                            if timer_peers.row_data(i) == Some(s.clone()) {
                                timer_peers.remove(i);
                                break;
                            }
                        }
                    }
                    iroh_net::Event::Error(msg) => {
                        eprintln!("iroh: {msg}");
                    }
                }
            }
        },
    );
    std::mem::forget(timer);

    app.run()
}

fn refresh_entries(
    app_handle: &slint::Weak<AppWindow>,
    state_handle: &Rc<RefCell<AppState>>,
) {
    let Some(app) = app_handle.upgrade() else {
        return;
    };
    let entries_db = state_handle.borrow().entries_db.clone();
    let txn = match entries_db.start_read_txn() {
        Ok(txn) => txn,
        Err(err) => {
            app.set_entries_error(format!("Entries read failed: {err}").into());
            return;
        }
    };
    let query = Query::ByTimestamps {
        since: None,
        until: None,
        limit: Some(50),
    };
    let entries = match entries_db.query(&txn, query) {
        Ok(entries) => entries,
        Err(err) => {
            app.set_entries_error(format!("Failed to query entries: {err}").into());
            return;
        }
    };
    let summaries = entries
        .iter()
        .map(|entry| EntrySummary {
            id: entry.id as i32,
            when: SharedString::from(format_ts(entry.when)),
            kind: SharedString::from(entry.kind.clone()),
        })
        .collect::<Vec<_>>();
    let mut state = state_handle.borrow_mut();
    state.entries = entries;
    replace_model(&state.entries_model, summaries);
    app.set_entries(ModelRc::from(state.entries_model.clone()));
    app.set_selected_entry_index(-1);
    app.set_entry_kind("".into());
    app.set_entry_when("".into());
    app.set_edit_kind_index(-1);
    state.edit_entry_model = Rc::new(VecModel::from(Vec::<KeyValueRow>::new()));
    app.set_entry_values(ModelRc::from(state.edit_entry_model.clone()));
}

fn init_entries_db(db_dir: &std::path::Path) -> EntriesDatabase {
    let entries_dir = db_dir.join("entries");
    let _ = std::fs::create_dir_all(&entries_dir);
    EntriesDatabase::init(&entries_dir).expect("Failed to open entries database")
}

fn default_data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DATA_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(android_dir) = ANDROID_DATA_DIR.get() {
        return android_dir.join("ribolita");
    }
    let base = dirs::data_local_dir()
        .or_else(|| dirs::home_dir())
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("ribolita")
}

fn now_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_when(input: &str) -> Result<Option<u64>, String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<u64>()
        .map(Some)
        .map_err(|_| "Timestamp must be a number".to_string())
}

fn format_ts(ts: u64) -> String {
    let secs = ts as i64;
    use chrono::{TimeZone, Utc};
    Utc.timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

fn collect_values(model: &VecModel<KeyValueRow>) -> Result<Vec<(String, rmpv::Value)>, String> {
    let mut values = Vec::new();
    for i in 0..model.row_count() {
        let row = model.row_data(i).unwrap_or(KeyValueRow {
            key: SharedString::from(""),
            value: SharedString::from(""),
        });
        let key = row.key.trim().to_string();
        let value = row.value.trim().to_string();
        if key.is_empty() && value.is_empty() {
            continue;
        }
        if key.is_empty() {
            return Err("Key cannot be empty".to_string());
        }
        if value.is_empty() {
            return Err(format!("Value for key '{key}' cannot be empty"));
        }
        let json_value: serde_json::Value = serde_json::from_str(&value)
            .map_err(|err| format!("Invalid JSON for key '{key}': {err}"))?;
        values.push((key, rmp_json::json_value_to_rmp_value(&json_value)));
    }
    Ok(values)
}

fn replace_model<T: Clone + 'static>(model: &Rc<VecModel<T>>, items: Vec<T>) {
    while model.row_count() > 0 {
        model.remove(0);
    }
    for item in items {
        model.push(item);
    }
}
