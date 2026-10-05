use anyhow::anyhow;
use clap::{Parser, Subcommand};
use mimalloc::MiMalloc;
use ribolita::{EntriesMdbxDatabase, EntriesStore, Query, RunEnvironment, StateLMDBDatabase};
use ribolita_common::{entry::Entry, rmp_json};
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

        /// open the entries in a text editor and save the changes (replays state if --filename is given)
        #[arg(long)]
        edit: bool,
    },
    /// rebuild the state from scratch by running every entry through the program again
    Replay,
}

type Env = RunEnvironment<StateLMDBDatabase, EntriesMdbxDatabase>;

fn load_env(cli: &Cli) -> anyhow::Result<Env> {
    let filename = cli
        .filename
        .as_ref()
        .ok_or_else(|| anyhow!("--filename (the .wasm program) is required for this command"))?;
    let wasm = std::fs::read(filename)?;
    Ok(RunEnvironment::new(&cli.dir, &wasm)?)
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
        Commands::Entry {
            ref when,
            ref kind,
            ref data,
        } => {
            let when = *when;
            let when = when.unwrap_or_else(|| {
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
            });
            let entry = Entry {
                id: 0,
                when,
                kind: kind.clone(),
                values: data.clone(),
            };

            let id = load_env(&cli)?.run(entry)?;
            writeln!(std::io::stderr(), "added entry:{id}")?;
        }
        Commands::Replay => {
            let count = load_env(&cli)?.replay()?;
            writeln!(std::io::stderr(), "replayed {count} entries")?;
        }
        Commands::State { ref path } => {
            let json_value = load_env(&cli)?.state_json(path.as_deref().unwrap_or(""))?;
            writeln!(
                std::io::stdout(),
                "{}",
                serde_json::to_string_pretty(&json_value).unwrap()
            )?;
        }
        Commands::Entries {
            ref id,
            since,
            last,
            ref format,
            edit,
        } => {
            let id = id.clone();
            let entries_dir = cli.dir.join("entries");
            let entries_db = EntriesMdbxDatabase::init(&entries_dir).map_err(|e| anyhow!(e))?;

            let query = if !id.is_empty() {
                Query::ByIds(id)
            } else {
                Query::ByTimestamps {
                    since,
                    until: None,
                    limit: last,
                }
            };
            let entries = entries_db.query(query)?;

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

                // now update the database with the edited ones; the state was derived from the old
                // entries, so it has to be rebuilt
                let edited_refs: Vec<&Entry> = edited_entries.iter().collect();
                if !edited_entries.is_empty() {
                    if cli.filename.is_some() {
                        drop(entries_db); // the environment opens its own handle
                        let count = load_env(&cli)?.update_entries(&edited_refs)?;
                        writeln!(std::io::stderr(), "replayed {count} entries")?;
                    } else {
                        entries_db.save(&edited_refs)?;
                        writeln!(
                            std::io::stderr(),
                            "state is now stale: run `replay --filename <program.wasm>`"
                        )?;
                    }
                }
                for entry in &edited_entries {
                    writeln!(std::io::stderr(), "updated entry:{}", entry.id)?;
                }
            } else {
                writeln!(std::io::stdout(), "{}", serialized)?;
            }
        }
    }

    Ok(())
}
