use anyhow::anyhow;
use clap::{Parser, Subcommand};
use mimalloc::MiMalloc;
use ribolita::{ApplyReport, MdbxStore, Query, RunEnvironment};
use ribolita_common::{entry::EntryId, rmp_json};
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

    /// the program (.wasm)
    #[arg(long)]
    filename: String,
}

#[derive(Subcommand)]
enum Commands {
    Entry {
        /// when the event happened, in seconds since the unix epoch (default: now)
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
        id: Vec<EntryId>,

        /// select entries that happened since a timestamp (seconds)
        #[arg(long)]
        since: Option<u64>,

        /// get the last n entries
        #[arg(long)]
        last: Option<usize>,

        /// format: yaml, json or toml
        #[arg(long, default_value = "yaml")]
        format: String,

        /// open the entries in a text editor and save the changes; entries removed from the
        /// list are deleted
        #[arg(long)]
        edit: bool,
    },
    /// apply entries that were stored but not applied yet
    CatchUp,
    /// rebuild the state from scratch by running every entry through the program again
    Replay,
    /// sync the entries with a peer (another device, or a server) over iroh, once
    Sync {
        /// the peer's iroh node id
        #[arg(long)]
        peer: String,
        /// the program's id on the peer
        #[arg(long)]
        program: String,
        /// for servers: the program's api key
        #[arg(long)]
        token: Option<String>,
        /// give up after this many seconds
        #[arg(long, default_value_t = 60)]
        timeout: u64,
    },
}

type Env = RunEnvironment<MdbxStore>;

fn load_env(cli: &Cli) -> anyhow::Result<Env> {
    let wasm = std::fs::read(&cli.filename)?;
    let author =
        ribolita_node::net::author_of(&ribolita_node::net::load_or_create_secret(&cli.dir)?);
    Ok(RunEnvironment::new(&cli.dir, &wasm, author)?)
}

fn print_report(report: &ApplyReport) -> anyhow::Result<()> {
    writeln!(std::io::stderr(), "applied {} entries", report.applied)?;
    for (id, err) in &report.failed {
        writeln!(std::io::stderr(), "  entry {id} was rejected: {err}")?;
    }
    Ok(())
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
    let mut env = load_env(&cli)?;

    match cli.command {
        Commands::Entry { when, kind, data } => {
            let (entry, report) = env.add_with_report(kind, data, when)?;
            writeln!(std::io::stderr(), "added entry:{}", entry.id)?;
            for (id, err) in &report.failed {
                writeln!(std::io::stderr(), "  entry {id} is now rejected: {err}")?;
            }
        }
        Commands::CatchUp => print_report(&env.catch_up()?)?,
        Commands::Sync {
            peer,
            program,
            token,
            timeout,
        } => {
            use ribolita_node::net;
            let secret = net::load_or_create_secret(&cli.dir)?;
            let env = std::sync::Arc::new(std::sync::Mutex::new(env));
            let host = net::SingleProgram {
                program: program.clone(),
                env,
            };
            let (node, events) = net::spawn(host, secret);
            node.cmd_tx
                .send(net::Cmd::Connect {
                    peer,
                    program,
                    token,
                })
                .map_err(|_| anyhow!("sync node stopped"))?;
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout);
            loop {
                let left = deadline.saturating_duration_since(std::time::Instant::now());
                match events.recv_timeout(left) {
                    Ok(net::Event::Synced {
                        received,
                        sent,
                        report,
                        ..
                    }) => {
                        writeln!(std::io::stderr(), "synced: {received} in, {sent} out")?;
                        print_report(&report)?;
                        break;
                    }
                    Ok(net::Event::Error(err)) => return Err(anyhow!(err)),
                    Ok(_) => {}
                    Err(_) => return Err(anyhow!("timed out")),
                }
            }
            return Ok(());
        }
        Commands::Replay => print_report(&env.replay()?)?,
        Commands::State { path } => {
            let json_value = env.state_json(path.as_deref().unwrap_or(""))?;
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
            let query = if !id.is_empty() {
                Query::ByIds(id)
            } else {
                Query::ByTime {
                    since,
                    until: None,
                    limit: last,
                }
            };
            let entries = env.entries(query)?;

            // serialize the entries in a format that can be edited as text
            let editable_entries: Vec<EditableEntry> =
                entries.into_iter().map(|e| e.into()).collect();
            let serialized = match format.as_str() {
                "toml" => toml::to_string_pretty(&editable_entries).map_err(|e| anyhow!(e))?,
                "yaml" => serde_yaml::to_string(&editable_entries).map_err(|e| anyhow!(e))?,
                _ => serde_json::to_string_pretty(&editable_entries).map_err(|e| anyhow!(e))?,
            };

            if !edit {
                writeln!(std::io::stdout(), "{}", serialized)?;
                return Ok(());
            }

            // open up an editor so they can be edited, then parse back the edited ones
            let edited = edit::edit(serialized)?;
            let edited_entries: Vec<EditableEntry> = match format.as_str() {
                "toml" => toml::from_str(&edited).map_err(|e| anyhow!(e))?,
                "yaml" => serde_yaml::from_str(&edited).map_err(|e| anyhow!(e))?,
                _ => serde_json::from_str(&edited).map_err(|e| anyhow!(e))?,
            };

            for orig in &editable_entries {
                let id: EntryId = orig.id.parse()?;
                match edited_entries.iter().find(|e| e.id == orig.id) {
                    // this equality check ignores the is_atomic field
                    Some(changed) if changed != orig => {
                        let mut changed = changed.clone();
                        changed.is_atomic = orig.is_atomic.clone();
                        let (when, kind, values) = changed.to_parts();
                        let (replacement, report) = env.edit(&id, Some(when), kind, values)?;
                        print_report(&report)?;
                        writeln!(
                            std::io::stderr(),
                            "replaced entry:{id} with {}",
                            replacement.id
                        )?;
                    }
                    Some(_) => {}
                    None => {
                        print_report(&env.delete(&id)?)?;
                        writeln!(std::io::stderr(), "deleted entry:{id}")?;
                    }
                }
            }
        }
    }

    Ok(())
}
