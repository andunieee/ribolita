use anyhow::anyhow;
use clap::{Parser, Subcommand};
use mimalloc::MiMalloc;
use ribolita::{EntriesDatabase, Query, ReadTransaction, StateDatabase, StateLMDBDatabase};
use ribolita_common::{
    entry::Entry,
    key::{self},
    rmp_json,
    state::State,
    value,
};
use std::io::Write;

mod editable_entry;
use crate::editable_entry::EditableEntry;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[derive(Parser)]
#[command(name = "ribolita")]
#[command(about = "cli for ribolita")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(long, default_value = "./data")]
    dir: std::path::PathBuf,

    #[arg(long)]
    filename: Option<String>,
}

#[derive(Subcommand)]
enum Commands {
    Entry {
        /// timestamp for when the entry was created
        #[arg(long)]
        when: Option<u64>,

        /// kind of the entry
        #[arg(long, required = true, short = 'k')]
        kind: String,

        /// key-value pairs for entry data (format: key=value)
        #[arg(long = "data", short = 'd', value_parser = parse_key_val)]
        data: Vec<(String, rmpv::Value)>,
    },
    State {
        /// optional path like "field.index.field.field"
        path: Option<String>,
    },
    Entries {
        /// select entries by id
        #[arg(long)]
        id: Vec<u64>,

        /// select entries since timestamp
        #[arg(long)]
        since: Option<u64>,

        /// get the last n entries
        #[arg(long)]
        last: Option<usize>,

        /// format: yaml, json or toml
        #[arg(long, default_value = "yaml")]
        format: String,

        // --edit invokes a text editor
        #[arg(long)]
        edit: bool,
    },
}

fn parse_key_val(s: &str) -> Result<(String, rmpv::Value), anyhow::Error> {
    let parts: Vec<&str> = s.splitn(2, '=').collect();
    if parts.len() != 2 {
        return Err(anyhow!("invalid key=value format: {}", s));
    }

    let jval: serde_json::Value = serde_json::from_str(parts[1])?;
    let mpval = rmp_json::json_value_to_rmp_value(&jval);
    Ok((parts[0].to_string(), mpval))
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Entry { when, kind, data } => {
            let filename = cli
                .filename
                .as_ref()
                .ok_or_else(|| anyhow!("filename is required for entry command"))?;
            let when = when.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
            });
            let entry = Entry {
                id: 0,
                when,
                kind,
                values: data,
            };

            let b = std::fs::read(filename)?;
            let mut env: ribolita::RunEnvironment<StateLMDBDatabase> =
                ribolita::RunEnvironment::new(&cli.dir, &b)?;
            env.run(entry)?;
        }
        Commands::State { path } => {
            let state_dir = cli.dir.join("state");
            let state_db = StateLMDBDatabase::init(&state_dir).map_err(|e| anyhow!(e))?;
            let txn = state_db.start_read_txn()?;

            let path_parts = path.as_ref().map(|s| key::parse(s)).unwrap_or_default();
            let prefix = key::to_bytes(&path_parts);

            let (num_rows, data) = txn.get(&prefix)?;
            let state: State = value::from_bytes(num_rows, &data)?;

            let json_value = rmp_json::rmp_value_to_json_value(&state.0);
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&json_value).unwrap()
            )?;
        }
        Commands::Entries {
            id,
            since,
            last,
            format,
            edit,
        } => {
            let entries_dir = cli.dir.join("entries");
            let entries_db = EntriesDatabase::init(&entries_dir).map_err(|e| anyhow!(e))?;
            let txn = entries_db.start_write_txn()?;

            let query = if !id.is_empty() {
                Query::ByIds(id)
            } else {
                Query::ByTimestamps {
                    since,
                    until: None,
                    limit: last,
                }
            };
            let entries = entries_db.query(&txn, query).map_err(|e| anyhow!(e))?;

            // serialize the entries in a format that can be edited as text
            let mut editable_entries: Vec<EditableEntry> =
                entries.into_iter().map(|e| e.into()).collect();
            let serialized = match format.as_str() {
                "toml" => toml::to_string_pretty(&editable_entries).map_err(|e| anyhow!(e))?,
                "yaml" => serde_yaml::to_string(&editable_entries).map_err(|e| anyhow!(e))?,
                _ => serde_json::to_string_pretty(&editable_entries).map_err(|e| anyhow!(e))?,
            };

            if edit {
                // open up an editor so they can be edited
                let edited = edit::edit(serialized)?;

                // parse back the edited ones
                let mut edited_editable_entries: Vec<EditableEntry> = match format.as_str() {
                    "toml" => toml::from_str(&edited).map_err(|e| anyhow!(e))?,
                    "yaml" => serde_yaml::from_str(&edited).map_err(|e| anyhow!(e))?,
                    _ => serde_json::from_str(&edited).map_err(|e| anyhow!(e))?,
                };

                // filter only the ones that were actually edited, and convert them back to Entry
                let edited_entries: Vec<Entry> = edited_editable_entries
                    .iter_mut()
                    .filter_map::<Entry, _>(|se| {
                        // check if it was edited otherwise skip
                        if let Some((i, _)) = editable_entries
                            .iter()
                            .enumerate()
                            .find(|(_, orig)| orig.id == se.id)
                        {
                            // assign the is_atomic bit back to them
                            let orig = editable_entries.swap_remove(i);

                            // check if it was edited (this equality check ignores the is_atomic field)
                            if orig != *se {
                                // this was edited
                                se.is_atomic = orig.is_atomic;
                                Some(se.into())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    })
                    .collect();

                // now update the database with the edited ones
                for entry in edited_entries {
                    entries_db
                        .save(&txn, entry.id, &entry)
                        .map_err(|e| anyhow!(e))?;
                    writeln!(std::io::stderr(), "updated entry:{}", entry.id)?;
                }
                txn.commit()?;
            } else {
                writeln!(std::io::stdout(), "{}", serialized)?;
            }
        }
    }

    Ok(())
}
