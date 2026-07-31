# Buddy

Buddy is a small Rust command-line assistant that indexes local machine context,
stores it in SQLite, and lets a Groq model answer questions about the snapshot.
Answers can be spoken through Voxd; Voxd's Mimic integration provides phonetic
memoization so repeated speech can reuse cached audio instead of paying for a
full synthesis every time.

Repository: <https://github.com/elci-group/buddy>

Buddy never sends the database itself. `ask` sends a bounded JSON view containing
the process list and at most 2,000 indexed filesystem entries by default.

## Build and install

```bash
cargo build --release
cargo install --path .
```

Set a Groq key before asking questions:

```bash
export GROQ_API_KEY="your-key"
```

The default model is `llama-3.3-70b-versatile`. Override it with `GROQ_MODEL`.

## Use

```bash
# Index HOME, or pass a smaller path for a faster/private snapshot.
buddy scan
buddy scan ~/projects

# Ask about the saved snapshot. Processes refresh on every question.
buddy ask "Which development tools are running?"

# Refresh the filesystem snapshot first and speak the response through Voxd.
buddy ask --refresh --speak "Summarise this machine"

# Inspect exactly what Buddy can send, without making a network request.
buddy context --limit 100
buddy status

# Check stable releases; repeated checks are cached for six hours.
buddy update
buddy update --force
```

The first `ask` automatically scans `HOME` if no filesystem snapshot exists.
Later asks reuse the stored scan unless `--refresh` is supplied.

## Voxd and Mimic

`--speak` invokes `voxd-cli speak --project <path>`. This keeps provider keys,
playback, voice selection, daemon lifecycle, and caching inside Voxd. When Voxd
is configured with Mimic, missing speech spans are synthesized while reusable
phrase, word, morpheme, and diphone audio is served from Mimic's local cache.
Buddy caps spoken output at 2,000 characters by default.

Useful overrides:

- `BUDDY_VOXD_BIN` — alternate `voxd-cli` executable.
- `BUDDY_VOXD_PROJECT` — project path used for the stable Voxd voice (default `.`).
- `BUDDY_TTS_MAX_CHARS` — spoken character cap.
- `BUDDY_CONTEXT_LIMIT` — filesystem entries included in model context.
- `BUDDY_DB_PATH` — SQLite path; defaults to `$XDG_DATA_HOME/buddy/buddy.db` or
  `~/.local/share/buddy/buddy.db`.
- `GROQ_API_URL` — Groq-compatible chat completions endpoint.
- `BUDDY_RELEASE_REPO` — GitHub `owner/repository` release source (defaults to
  `elci-group/buddy`).
- `BUDDY_RELEASE_TAG_PREFIX` — stable release tag prefix (defaults to
  `v`, such as `v1.2.0`).

Speech is opt-in. Without `--speak`, Buddy does not start Voxd or play audio.
The local `context` and `status` commands never contact Groq.

## Stable update detection

`buddy update` reads GitHub Releases, rejects drafts and prereleases, requires
an exact three-part semantic version after the configured tag prefix, and picks
the numerically newest release rather than trusting API order. Results are
cached in Buddy's database for six hours; `--force` bypasses the cache.

Update checks are read-only: Buddy reports the release page but never downloads
or executes an installer. Exact prefix filtering prevents unrelated tag formats
from being mistaken for a stable Buddy build. Set `GITHUB_TOKEN` for private
repositories or higher GitHub API limits.

## Privacy notes

- File contents are never indexed—only paths, type, size, and modification time.
- Symlinks are recorded but not followed.
- Unreadable entries are skipped and reported.
- Use `buddy scan <narrow-path>` and `--limit` to minimize shared metadata.
- The database and `.env` files are ignored by Git.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo build --release
deliver --spec deliver.toml --strict
```

Kaptaind is configured in `kaptaind.toml` to watch this project, run the Rust
test suite before commits, and stage only Buddy-owned paths from the otherwise
shared parent repository.
