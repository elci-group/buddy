use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{PidExt, ProcessExt, System, SystemExt};

const DEFAULT_MODEL: &str = "llama-3.3-70b-versatile";
const DEFAULT_API_URL: &str = "https://api.groq.com/openai/v1/chat/completions";
const DEFAULT_CONTEXT_LIMIT: usize = 2_000;
const DEFAULT_SPEECH_LIMIT: usize = 2_000;
const DEFAULT_RELEASE_REPO: &str = "elci-group/buddy";
const DEFAULT_RELEASE_TAG_PREFIX: &str = "v";
const UPDATE_CACHE_SECONDS: u64 = 6 * 60 * 60;

#[derive(Debug, PartialEq, Eq)]
enum CliCommand {
    Ask {
        question: String,
        refresh: bool,
        speak: bool,
        limit: usize,
    },
    Scan {
        root: Option<PathBuf>,
    },
    Status,
    Context {
        limit: usize,
    },
    Update {
        force: bool,
    },
    Help,
    Version,
}

#[derive(Debug, Serialize)]
struct ProcessRecord {
    pid: u32,
    name: String,
    executable: String,
    started_at_unix: u64,
}

#[derive(Debug, Serialize)]
struct FileRecord {
    path: String,
    is_dir: bool,
    size_bytes: u64,
    modified_at_unix: Option<u64>,
}

#[derive(Debug, Serialize)]
struct MachineContext {
    process_count: usize,
    indexed_entry_count: usize,
    included_entry_count: usize,
    scan_root: Option<String>,
    scanned_at_unix: Option<u64>,
    processes: Vec<ProcessRecord>,
    filesystem: Vec<FileRecord>,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
    temperature: f32,
    max_tokens: u32,
}

#[derive(Debug, Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: ChatResponseMessage,
}

#[derive(Debug, Deserialize)]
struct ChatResponseMessage {
    content: String,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    draft: bool,
    prerelease: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct StableVersion {
    major: u64,
    minor: u64,
    patch: u64,
}

impl StableVersion {
    fn parse(value: &str) -> Option<Self> {
        if value.contains('-') || value.contains('+') {
            return None;
        }
        let mut parts = value.split('.');
        let version = Self {
            // traci: allow
            major: parts.next()?.parse().ok()?,
            // traci: allow
            minor: parts.next()?.parse().ok()?,
            // traci: allow
            patch: parts.next()?.parse().ok()?,
        };
        if parts.next().is_some() {
            return None;
        }
        Some(version)
    }

    fn from_tag(tag: &str, prefix: &str) -> Option<Self> {
        Self::parse(tag.strip_prefix(prefix)?)
    }
}

impl std::fmt::Display for StableVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

struct BuddyStore {
    connection: Connection,
    path: PathBuf,
}

impl BuddyStore {
    fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create data directory {}", parent.display()))?;
        }
        let connection =
            Connection::open(&path).with_context(|| format!("open database {}", path.display()))?;
        let store = Self { connection, path };
        store.initialize()?;
        Ok(store)
    }

    #[cfg(test)]
    fn in_memory() -> Result<Self> {
        let store = Self {
            connection: Connection::open_in_memory()?,
            path: PathBuf::from(":memory:"),
        };
        store.initialize()?;
        Ok(store)
    }

    fn initialize(&self) -> Result<()> {
        self.connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS processes (
                 pid INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 executable TEXT NOT NULL,
                 started_at_unix INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS filesystem (
                 path TEXT PRIMARY KEY,
                 is_dir INTEGER NOT NULL,
                 size_bytes INTEGER NOT NULL,
                 modified_at_unix INTEGER
             );
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );",
        )?;
        Ok(())
    }

    fn capture_processes(&mut self) -> Result<usize> {
        let mut system = System::new_all();
        system.refresh_processes();

        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM processes", [])?;
        let count = {
            let mut insert = transaction.prepare(
                "INSERT INTO processes (pid, name, executable, started_at_unix)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut count = 0;
            for (pid, process) in system.processes() {
                insert.execute(params![
                    pid.as_u32(),
                    process.name(),
                    process.exe().to_string_lossy(),
                    process.start_time(),
                ])?;
                count += 1;
            }
            count
        };
        transaction.commit()?;
        self.set_metadata("processes_at_unix", &unix_now().to_string())?;
        Ok(count)
    }

    fn capture_filesystem(&mut self, root: &Path) -> Result<(usize, usize)> {
        if !root.exists() {
            bail!("scan root does not exist: {}", root.display());
        }

        let transaction = self.connection.transaction()?;
        transaction.execute("DELETE FROM filesystem", [])?;
        let (count, skipped) = {
            let mut insert = transaction.prepare(
                "INSERT OR REPLACE INTO filesystem
                 (path, is_dir, size_bytes, modified_at_unix)
                 VALUES (?1, ?2, ?3, ?4)",
            )?;
            let mut count = 0;
            let mut skipped = 0;
            let mut pending = vec![root.to_path_buf()];
            while let Some(path) = pending.pop() {
                let metadata = match std::fs::symlink_metadata(&path) {
                    Ok(metadata) => metadata,
                    // traci: allow
                    Err(_) => {
                        skipped += 1;
                        continue;
                    }
                };
                let modified_at = match metadata.modified() {
                    Ok(time) => match time.duration_since(UNIX_EPOCH) {
                        Ok(duration) => Some(duration.as_secs()),
                        // traci: allow
                        Err(_) => None,
                    },
                    // traci: allow
                    Err(_) => None,
                };
                insert.execute(params![
                    path.to_string_lossy(),
                    metadata.is_dir(),
                    metadata.len(),
                    modified_at,
                ])?;
                count += 1;

                if metadata.is_dir() {
                    match std::fs::read_dir(&path) {
                        Ok(children) => {
                            for child in children {
                                match child {
                                    Ok(child) => pending.push(child.path()),
                                    // traci: allow
                                    Err(_) => skipped += 1,
                                }
                            }
                        }
                        // traci: allow
                        Err(_) => skipped += 1,
                    }
                }
            }
            (count, skipped)
        };
        transaction.commit()?;
        self.set_metadata("scan_root", &root.to_string_lossy())?;
        self.set_metadata("scanned_at_unix", &unix_now().to_string())?;
        Ok((count, skipped))
    }

    fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    fn metadata(&self, key: &str) -> Result<Option<String>> {
        let mut statement = self
            .connection
            .prepare("SELECT value FROM metadata WHERE key = ?1")?;
        let mut rows = statement.query([key])?;
        Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
    }

    fn process_count(&self) -> Result<usize> {
        self.count("processes")
    }

    fn file_count(&self) -> Result<usize> {
        self.count("filesystem")
    }

    fn count(&self, table: &str) -> Result<usize> {
        let query = match table {
            "processes" => "SELECT COUNT(*) FROM processes",
            "filesystem" => "SELECT COUNT(*) FROM filesystem",
            _ => bail!("unsupported table"),
        };
        Ok(self.connection.query_row(query, [], |row| row.get(0))?)
    }

    fn context(&self, limit: usize) -> Result<MachineContext> {
        let mut process_statement = self.connection.prepare(
            "SELECT pid, name, executable, started_at_unix
             FROM processes ORDER BY name, pid",
        )?;
        let processes = process_statement
            .query_map([], |row| {
                Ok(ProcessRecord {
                    pid: row.get(0)?,
                    name: row.get(1)?,
                    executable: row.get(2)?,
                    started_at_unix: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let indexed_entry_count = self.file_count()?;
        let mut file_statement = self.connection.prepare(
            "SELECT path, is_dir, size_bytes, modified_at_unix
             FROM filesystem ORDER BY path LIMIT ?1",
        )?;
        let filesystem = file_statement
            .query_map([limit], |row| {
                Ok(FileRecord {
                    path: row.get(0)?,
                    is_dir: row.get(1)?,
                    size_bytes: row.get(2)?,
                    modified_at_unix: row.get(3)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        Ok(MachineContext {
            process_count: processes.len(),
            indexed_entry_count,
            included_entry_count: filesystem.len(),
            scan_root: self.metadata("scan_root")?,
            scanned_at_unix: self
                .metadata("scanned_at_unix")?
                .map(|value| {
                    value
                        .parse()
                        .with_context(|| format!("invalid cached scan timestamp '{value}'"))
                })
                .transpose()?,
            processes,
            filesystem,
        })
    }
}

fn main() {
    if let Err(error) = run(env::args_os().skip(1)) {
        eprintln!("buddy: {error:#}");
        std::process::exit(1);
    }
}

fn run<I>(arguments: I) -> Result<()>
where
    I: IntoIterator<Item = OsString>,
{
    match parse_args(arguments)? {
        CliCommand::Help => print_help(),
        CliCommand::Version => println!("buddy {}", env!("CARGO_PKG_VERSION")),
        command => {
            let database_path = database_path()?;
            let mut store = BuddyStore::open(database_path)?;
            match command {
                CliCommand::Scan { root } => {
                    let process_count = store.capture_processes()?;
                    let root = root.unwrap_or(home_dir()?);
                    let (entry_count, skipped) = store.capture_filesystem(&root)?;
                    println!(
                        "Indexed {entry_count} filesystem entries and {process_count} processes (skipped {skipped})."
                    );
                }
                CliCommand::Status => print_status(&store)?,
                CliCommand::Update { force } => check_for_update(&store, force)?,
                CliCommand::Context { limit } => {
                    store.capture_processes()?;
                    println!("{}", serde_json::to_string_pretty(&store.context(limit)?)?);
                }
                CliCommand::Ask {
                    question,
                    refresh,
                    speak,
                    limit,
                } => {
                    store.capture_processes()?;
                    if refresh || store.file_count()? == 0 {
                        let root = home_dir()?;
                        let (entries, skipped) = store.capture_filesystem(&root)?;
                        eprintln!("Indexed {entries} filesystem entries (skipped {skipped}).");
                    }
                    let answer = ask_groq(&question, &store.context(limit)?)?;
                    println!("{answer}");
                    if speak {
                        speak_with_voxd(&answer)?;
                    }
                }
                CliCommand::Help | CliCommand::Version => unreachable!(),
            }
        }
    }
    Ok(())
}

fn parse_args<I>(arguments: I) -> Result<CliCommand>
where
    I: IntoIterator<Item = OsString>,
{
    let mut args = arguments
        .into_iter()
        .map(|value| value.to_string_lossy().into_owned());
    let Some(command) = args.next() else {
        return Ok(CliCommand::Help);
    };
    let remaining: Vec<String> = args.collect();
    match command.as_str() {
        "ask" => parse_ask(remaining),
        "scan" => {
            if remaining.len() > 1 {
                bail!("scan accepts at most one path");
            }
            Ok(CliCommand::Scan {
                root: remaining.first().map(PathBuf::from),
            })
        }
        "status" if remaining.is_empty() => Ok(CliCommand::Status),
        "context" => Ok(CliCommand::Context {
            limit: parse_limit_only(&remaining)?,
        }),
        "update" => match remaining.as_slice() {
            [] => Ok(CliCommand::Update { force: false }),
            [flag] if flag == "--force" => Ok(CliCommand::Update { force: true }),
            _ => bail!("update accepts only '--force'"),
        },
        "help" | "--help" | "-h" if remaining.is_empty() => Ok(CliCommand::Help),
        "version" | "--version" | "-V" if remaining.is_empty() => Ok(CliCommand::Version),
        "status" | "help" | "version" | "--help" | "--version" | "-h" | "-V" => {
            bail!("{command} does not accept arguments")
        }
        _ => bail!("unknown command '{command}'; run 'buddy help'"),
    }
}

fn parse_ask(arguments: Vec<String>) -> Result<CliCommand> {
    let mut refresh = false;
    let mut speak = false;
    let mut limit = env_usize("BUDDY_CONTEXT_LIMIT")?.unwrap_or(DEFAULT_CONTEXT_LIMIT);
    let mut question = Vec::new();
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--refresh" => refresh = true,
            "--speak" => speak = true,
            "--limit" => {
                index += 1;
                limit = parse_limit(arguments.get(index))?;
            }
            value if value.starts_with('-') => bail!("unknown ask option '{value}'"),
            value => question.push(value.to_owned()),
        }
        index += 1;
    }
    if question.is_empty() {
        bail!("ask requires a question");
    }
    Ok(CliCommand::Ask {
        question: question.join(" "),
        refresh,
        speak,
        limit,
    })
}

fn parse_limit_only(arguments: &[String]) -> Result<usize> {
    match arguments {
        [] => Ok(env_usize("BUDDY_CONTEXT_LIMIT")?.unwrap_or(DEFAULT_CONTEXT_LIMIT)),
        [flag, value] if flag == "--limit" => parse_limit(Some(value)),
        _ => bail!("context accepts only '--limit <number>'"),
    }
}

fn parse_limit(value: Option<&String>) -> Result<usize> {
    let raw = value.ok_or_else(|| anyhow!("--limit requires a value"))?;
    let parsed = raw
        .parse::<usize>()
        .with_context(|| format!("invalid limit '{raw}'"))?;
    if parsed == 0 {
        bail!("limit must be greater than zero");
    }
    Ok(parsed)
}

fn print_status(store: &BuddyStore) -> Result<()> {
    println!("Database: {}", store.path.display());
    println!("Processes: {}", store.process_count()?);
    println!("Filesystem entries: {}", store.file_count()?);
    println!(
        "Scan root: {}",
        store
            .metadata("scan_root")?
            .unwrap_or_else(|| "not scanned".to_owned())
    );
    println!(
        "Voxd backend: {}",
        env::var("BUDDY_VOXD_BIN").unwrap_or_else(|_| "voxd-cli".to_owned())
    );
    Ok(())
}

fn ask_groq(question: &str, context: &MachineContext) -> Result<String> {
    let api_key = env::var("GROQ_API_KEY").context("GROQ_API_KEY is not set")?;
    let model = env::var("GROQ_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_owned());
    let api_url = env::var("GROQ_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_owned());
    let context_json = serde_json::to_string(context)?;
    let prompt = format!(
        "Use only the supplied machine snapshot when making claims about this computer. Say when the snapshot does not contain enough information. Snapshot: {context_json}\nQuestion: {question}"
    );
    let request = ChatRequest {
        model: &model,
        messages: vec![
            ChatMessage {
                role: "system",
                content:
                    "You are Buddy, a concise and privacy-aware assistant for the user's computer.",
            },
            ChatMessage {
                role: "user",
                content: &prompt,
            },
        ],
        temperature: 0.2,
        max_tokens: 768,
    };

    let response = Client::builder()
        .timeout(Duration::from_secs(60))
        .build()?
        .post(api_url)
        .bearer_auth(api_key)
        .json(&request)
        .send()
        .context("send request to Groq")?;
    let status = response.status();
    let body = response.text().context("read Groq response")?;
    if !status.is_success() {
        bail!(
            "Groq returned HTTP {status}: {}",
            truncate_chars(body.trim(), 300)
        );
    }
    let response: ChatResponse = serde_json::from_str(&body).context("decode Groq response")?;
    response
        .choices
        .into_iter()
        .next()
        .map(|choice| choice.message.content.trim().to_owned())
        .filter(|content| !content.is_empty())
        .ok_or_else(|| anyhow!("Groq returned no answer"))
}

fn check_for_update(store: &BuddyStore, force: bool) -> Result<()> {
    let current = StableVersion::parse(env!("CARGO_PKG_VERSION")).ok_or_else(|| {
        anyhow!(
            "update checks are disabled for non-stable build {}",
            env!("CARGO_PKG_VERSION")
        )
    })?;
    let repository =
        env::var("BUDDY_RELEASE_REPO").unwrap_or_else(|_| DEFAULT_RELEASE_REPO.to_owned());
    validate_repository(&repository)?;
    let prefix = env::var("BUDDY_RELEASE_TAG_PREFIX")
        .unwrap_or_else(|_| DEFAULT_RELEASE_TAG_PREFIX.to_owned());

    if !force {
        let checked_at = store
            .metadata("update_checked_at_unix")?
            .map(|value| {
                value
                    .parse::<u64>()
                    .with_context(|| format!("invalid cached update timestamp '{value}'"))
            })
            .transpose()?
            .unwrap_or_default();
        let cached_repository = store.metadata("update_repository")?;
        let cached_prefix = store.metadata("update_tag_prefix")?;
        if unix_now().saturating_sub(checked_at) < UPDATE_CACHE_SECONDS
            && cached_repository.as_deref() == Some(&repository)
            && cached_prefix.as_deref() == Some(&prefix)
        {
            let version = store.metadata("update_latest_version")?;
            let url = store.metadata("update_latest_url")?;
            return print_update_result(current, version.as_deref(), url.as_deref(), true);
        }
    }

    let releases = fetch_releases(&repository)?;
    let latest = latest_stable_release(&releases, &prefix);
    store.set_metadata("update_checked_at_unix", &unix_now().to_string())?;
    store.set_metadata("update_repository", &repository)?;
    store.set_metadata("update_tag_prefix", &prefix)?;
    if let Some((version, release)) = latest {
        store.set_metadata("update_latest_version", &version.to_string())?;
        store.set_metadata("update_latest_url", &release.html_url)?;
        print_update_result(
            current,
            Some(&version.to_string()),
            Some(&release.html_url),
            false,
        )
    } else {
        store.set_metadata("update_latest_version", "")?;
        store.set_metadata("update_latest_url", "")?;
        print_update_result(current, None, None, false)
    }
}

fn fetch_releases(repository: &str) -> Result<Vec<GitHubRelease>> {
    let api_root = env::var("GITHUB_API_URL")
        .unwrap_or_else(|_| "https://api.github.com".to_owned())
        .trim_end_matches('/')
        .to_owned();
    let url = format!("{api_root}/repos/{repository}/releases?per_page=30");
    let client = Client::builder().timeout(Duration::from_secs(30)).build()?;
    let mut request = client
        .get(url)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", format!("buddy/{}", env!("CARGO_PKG_VERSION")));
    if let Ok(token) = env::var("GITHUB_TOKEN") {
        request = request.bearer_auth(token);
    }
    let response = request.send().context("check GitHub releases")?;
    let status = response.status();
    let body = response.text().context("read GitHub release response")?;
    if !status.is_success() {
        bail!(
            "GitHub returned HTTP {status}: {}",
            truncate_chars(body.trim(), 300)
        );
    }
    serde_json::from_str(&body).context("decode GitHub releases")
}

fn latest_stable_release<'a>(
    releases: &'a [GitHubRelease],
    prefix: &str,
) -> Option<(StableVersion, &'a GitHubRelease)> {
    releases
        .iter()
        .filter(|release| !release.draft && !release.prerelease)
        .filter_map(|release| {
            StableVersion::from_tag(&release.tag_name, prefix).map(|version| (version, release))
        })
        .max_by_key(|(version, _)| *version)
}

fn print_update_result(
    current: StableVersion,
    latest: Option<&str>,
    url: Option<&str>,
    cached: bool,
) -> Result<()> {
    let cache_note = if cached { " (cached)" } else { "" };
    let Some(latest) = latest.filter(|value| !value.is_empty()) else {
        println!("No stable Buddy release was found{cache_note}. Current: {current}.");
        return Ok(());
    };
    let latest_version = StableVersion::parse(latest)
        .ok_or_else(|| anyhow!("invalid cached release version '{latest}'"))?;
    if latest_version > current {
        println!("Buddy {latest_version} is available; current: {current}{cache_note}.");
        if let Some(url) = url.filter(|value| !value.is_empty()) {
            println!("Release: {url}");
        }
    } else {
        println!("Buddy is up to date at {current}{cache_note}.");
    }
    Ok(())
}

fn validate_repository(repository: &str) -> Result<()> {
    let Some((owner, name)) = repository.split_once('/') else {
        bail!("BUDDY_RELEASE_REPO must use owner/repository format");
    };
    let valid = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || ".-_".contains(character))
    };
    if !valid(owner) || !valid(name) || name.contains("..") {
        bail!("invalid BUDDY_RELEASE_REPO '{repository}'");
    }
    Ok(())
}

fn speak_with_voxd(text: &str) -> Result<()> {
    let binary = env::var("BUDDY_VOXD_BIN").unwrap_or_else(|_| "voxd-cli".to_owned());
    let project = env::var("BUDDY_VOXD_PROJECT").unwrap_or_else(|_| ".".to_owned());
    let limit = env_usize("BUDDY_TTS_MAX_CHARS")?.unwrap_or(DEFAULT_SPEECH_LIMIT);
    let spoken = truncate_chars(text, limit);
    let status = Command::new(&binary)
        .arg("speak")
        .arg("--project")
        .arg(project)
        .arg(spoken)
        .status()
        .with_context(|| format!("start Voxd backend '{binary}'"))?;
    if !status.success() {
        bail!("Voxd backend exited with {status}");
    }
    Ok(())
}

fn truncate_chars(text: &str, limit: usize) -> &str {
    match text.char_indices().nth(limit) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

fn database_path() -> Result<PathBuf> {
    if let Some(path) = env::var_os("BUDDY_DB_PATH") {
        return Ok(PathBuf::from(path));
    }
    if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(data_home).join("buddy/buddy.db"));
    }
    Ok(home_dir()?.join(".local/share/buddy/buddy.db"))
}

fn home_dir() -> Result<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("HOME is not set; pass an explicit path to 'buddy scan'"))
}

fn env_usize(name: &str) -> Result<Option<usize>> {
    match env::var(name) {
        Ok(value) => {
            let parsed = value
                .parse::<usize>()
                .with_context(|| format!("{name} must be a positive integer"))?;
            if parsed == 0 {
                bail!("{name} must be greater than zero");
            }
            Ok(Some(parsed))
        }
        // traci: allow
        Err(env::VarError::NotPresent) => Ok(None),
        // traci: allow
        Err(env::VarError::NotUnicode(_)) => bail!("{name} is not valid UTF-8"),
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn print_help() {
    println!(
        "Buddy — local machine context with Groq intelligence and Voxd speech\n\n\
         USAGE:\n  \
           buddy scan [PATH]\n  \
           buddy ask [--refresh] [--speak] [--limit N] <QUESTION>\n  \
           buddy context [--limit N]\n  \
           buddy update [--force]\n  \
           buddy status\n\n\
         COMMANDS:\n  \
           scan       Index a directory (HOME by default) and running processes\n  \
           ask        Ask Groq about the saved snapshot; --speak uses Voxd\n  \
           context    Print the bounded JSON context without contacting Groq\n  \
           update     Check stable GitHub releases (cached for six hours)\n  \
           status     Show database and backend status\n\n\
         ENVIRONMENT:\n  \
           GROQ_API_KEY          Required by ask\n  \
           GROQ_MODEL            Model override ({DEFAULT_MODEL})\n  \
           BUDDY_DB_PATH         SQLite database override\n  \
           BUDDY_CONTEXT_LIMIT   Maximum filesystem entries sent (2000)\n  \
           BUDDY_VOXD_BIN        Voxd executable override (voxd-cli)\n  \
           BUDDY_VOXD_PROJECT    Stable Voxd project voice path (.)\n  \
           BUDDY_TTS_MAX_CHARS   Maximum spoken answer length (2000)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    #[test]
    fn parses_spoken_refreshing_question() {
        let command = parse_args(strings(&[
            "ask",
            "--speak",
            "--refresh",
            "--limit",
            "42",
            "what",
            "is running?",
        ]))
        .unwrap();
        assert_eq!(
            command,
            CliCommand::Ask {
                question: "what is running?".to_owned(),
                refresh: true,
                speak: true,
                limit: 42,
            }
        );
    }

    #[test]
    fn rejects_zero_limit() {
        let error = parse_args(strings(&["context", "--limit", "0"])).unwrap_err();
        assert!(error.to_string().contains("greater than zero"));
    }

    #[test]
    fn unicode_truncation_stays_on_character_boundaries() {
        assert_eq!(truncate_chars("a🦀bc", 2), "a🦀");
        assert_eq!(truncate_chars("a🦀bc", 20), "a🦀bc");
    }

    #[test]
    fn empty_store_has_bounded_empty_context() {
        let store = BuddyStore::in_memory().unwrap();
        let context = store.context(10).unwrap();
        assert_eq!(context.process_count, 0);
        assert_eq!(context.indexed_entry_count, 0);
        assert_eq!(context.included_entry_count, 0);
    }

    #[test]
    fn stable_versions_sort_numerically() {
        let old = StableVersion::parse("1.9.12").unwrap();
        let new = StableVersion::parse("1.10.0").unwrap();
        assert!(new > old);
        assert!(StableVersion::parse("1.10.0-beta.1").is_none());
    }

    #[test]
    fn release_selection_ignores_drafts_prereleases_and_other_tags() {
        let releases = vec![
            GitHubRelease {
                tag_name: "buddy-v1.2.0".to_owned(),
                html_url: "stable".to_owned(),
                draft: false,
                prerelease: false,
            },
            GitHubRelease {
                tag_name: "buddy-v9.0.0".to_owned(),
                html_url: "draft".to_owned(),
                draft: true,
                prerelease: false,
            },
            GitHubRelease {
                tag_name: "v8.0.0".to_owned(),
                html_url: "other-project".to_owned(),
                draft: false,
                prerelease: false,
            },
        ];
        let (version, release) = latest_stable_release(&releases, "buddy-v").unwrap();
        assert_eq!(version, StableVersion::parse("1.2.0").unwrap());
        assert_eq!(release.html_url, "stable");
    }
}
