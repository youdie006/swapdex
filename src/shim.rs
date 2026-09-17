//! The `claude` shim: a tiny launcher placed on the user's PATH (ahead of the
//! real `claude`) that reads swapdex's default-account pointer and runs the real
//! `claude` in that account's slot. This is what makes a plain `claude` follow
//! `swapdex use`. No credential is ever moved - the shim only sets
//! `CLAUDE_CONFIG_DIR`.

use crate::paths::Paths;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Private `proxy --ensure` outcome meaning that Rust verified an intentional
/// or unmanaged direct route. Generated shims accept it only with empty stdout.
pub(crate) const PROXY_PASSTHROUGH_EXIT_STATUS: i32 = 3;

/// Where swapdex installs the shim: `<store_dir>/bin/claude`.
pub fn shim_path(paths: &Paths) -> PathBuf {
    shim_path_for(paths, "claude-code")
}

/// Where a given tool's shim lives: `<store_dir>/bin/<binary>`.
/// The directory holding the generated shims - the one that must be stepped over
/// when looking for the real tool.
pub fn shim_bin_dir(paths: &Paths) -> PathBuf {
    paths.store_dir().join("bin")
}

pub fn shim_path_for(paths: &Paths, tool: &str) -> PathBuf {
    let bin = match tool {
        "codex" => "codex",
        // Same fall-through: a gemini shim would have been installed under the
        // name "claude", replacing the shim a plain `claude` runs.
        "gemini" => "gemini",
        "antigravity" => "agy",
        _ => "claude",
    };
    paths.store_dir().join("bin").join(bin)
}

/// Single-quote a path for safe embedding in the /bin/sh shim script.
fn sh_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Validate the machine-readable result of `proxy --ensure` inside a generated
/// POSIX shell shim. Numeric comparison happens only after digit and length
/// checks, so arbitrarily long output cannot overflow a shell integer parser.
fn proxy_result_script(tool: &str) -> String {
    format!(
        r#"sx_proxy_status=$?
sx_use_proxy=no
sx_proxy_bad=no
if [ "$sx_proxy_status" -eq {passthrough} ] && [ -z "$port" ]; then
    :
elif [ "$sx_proxy_status" -ne 0 ]; then
    sx_proxy_bad=yes
else
    case "$port" in
        ''|*[!0-9]*) sx_proxy_bad=yes ;;
        *)
            sx_port=$port
            while [ "${{sx_port#0}}" != "$sx_port" ]; do sx_port=${{sx_port#0}}; done
            case "$sx_port" in
                ''|??????*) sx_proxy_bad=yes ;;
                *)
                    if [ "$sx_port" -gt 65535 ]; then
                        sx_proxy_bad=yes
                    else
                        port=$sx_port
                        sx_use_proxy=yes
                    fi
                    ;;
            esac
            ;;
    esac
fi
if [ "$sx_proxy_bad" = yes ]; then
    printf '%s\n' 'swapdex: managed proxy startup failed; run `swapdex proxy --ensure --tool {tool}` for details.' >&2
    exit 1
fi"#,
        passthrough = PROXY_PASSTHROUGH_EXIT_STATUS,
        tool = tool,
    )
}

/// The shim script body. The default-account pointer only fills in when nothing
/// has already chosen a config dir: an explicit `CLAUDE_CONFIG_DIR` (what
/// `swapdex run <account>` sets, or what a user exports by hand) is a decision
/// already made, and overriding it meant every account opened as the default one.
///
/// It also gets proxy mode for free: the shim asks `swapdex proxy --ensure`,
/// which prints the port of a running proxy and starts one in the background if
/// there is none. Managed startup must succeed with one valid port; only Rust's
/// private, verified passthrough outcome may launch directly.
pub fn shim_script(pointer: &Path, real_claude: &Path, swapdex: &Path) -> String {
    let proxy_result = proxy_result_script("claude-code").replace('\n', "\n\t");
    format!(
        "#!/bin/sh\n\
         # swapdex claude shim - launch claude in the default account's slot.\n\
         # Managed by swapdex; re-created by `swapdex shim`.\n\
         # Signing in must reach Anthropic directly. The OAuth exchange is between\n\
         # the browser and the real API, and a proxy in the middle both breaks the\n\
         # code exchange and answers with whichever account it already has - so a\n\
         # fresh slot looks signed in as somebody else, or the prompt takes no\n\
         # input at all.\n\
         # Only documented top-level authentication commands bypass managed\n\
         # routing. Prompt text and option values can contain words like login.\n\
         sx_plain=no\n\
         sx_options=yes\n\
         sx_skip=\n\
         sx_command=\n\
         sx_auth_command=\n\
         for a in \"$@\"; do\n\
         \tif [ -n \"$sx_skip\" ]; then\n\
         \t\tsx_skip=\n\
         \t\tcontinue\n\
         \tfi\n\
         \tif [ \"$sx_command\" = auth ] && [ -z \"$sx_auth_command\" ]; then\n\
         \t\tsx_auth_command=other\n\
         \t\tcase \"$a\" in login|logout|status|-h|--help) sx_auth_command=\"$a\" ;; esac\n\
         \t\tcontinue\n\
         \tfi\n\
         \tif [ -n \"$sx_command\" ] && [ \"$sx_command\" != ambiguous ]; then continue; fi\n\
         \tif [ \"$sx_options\" = no ]; then\n\
         \t\tsx_command=prompt\n\
         \t\tcontinue\n\
         \tfi\n\
         \tcase \"$a\" in\n\
         \t\t--) sx_options=no; sx_command=prompt ;;\n\
         \t\t-h|--help|-v|--version) sx_plain=yes ;;\n\
         \t\t-p|--print|--print=*|-p?*) sx_command=prompt ;;\n\
         \t\t-m|--model|--permission-mode|--settings) sx_skip=value ;;\n\
         \t\t--setting-sources|--plugin-dir|--plugin-url|--cwd) sx_skip=value ;;\n\
         \t\t--debug-file) sx_skip=value ;;\n\
         \t\t-m?*|--model=*|--permission-mode=*|--settings=*) ;;\n\
         \t\t--setting-sources=*|--plugin-dir=*|--plugin-url=*|--cwd=*) ;;\n\
         \t\t--debug-file=*) ;;\n\
         \t\t# Optional and variadic values are indistinguishable from a later\n\
         \t\t# command token, so these forms cannot authorize direct auth.\n\
         \t\t-d|--debug|--debug=*|-d?*|--mcp-config|--mcp-config=*) sx_command=ambiguous ;;\n\
         \t\t--verbose) ;;\n\
         \t\t-*) sx_command=unknown ;;\n\
         \t\t*) if [ \"$sx_command\" != ambiguous ]; then sx_command=\"$a\"; fi ;;\n\
         \tesac\n\
         done\n\
         case \"$sx_command:$sx_auth_command\" in\n\
         \tauth:login|auth:logout|auth:status|auth:-h|auth:--help|setup-token:) sx_plain=yes ;;\n\
         esac\n\
         # Match the documented opt-in value 1 for alternate providers. Empty,\n\
         # 0, and false remain managed by swapdex.\n\
         if [ -n \"$ANTHROPIC_BASE_URL\" ] ||\n\
         \t[ \"$CLAUDE_CODE_USE_BEDROCK\" = 1 ] ||\n\
         \t[ \"$CLAUDE_CODE_USE_MANTLE\" = 1 ] ||\n\
         \t[ \"$CLAUDE_CODE_USE_VERTEX\" = 1 ] ||\n\
         \t[ \"$CLAUDE_CODE_USE_FOUNDRY\" = 1 ] ||\n\
         \t[ \"$CLAUDE_CODE_USE_ANTHROPIC_AWS\" = 1 ]; then\n\
         \tsx_plain=yes\n\
         fi\n\
         # Ask swapdex for a live proxy (it starts one if needed and prints one\n\
         # validated port). Any uncertain managed state stops before Claude.\n\
         if [ \"$sx_plain\" = no ]; then\n\
         \tport=$({sx} proxy --ensure --tool claude-code 2>/dev/null)\n\
         \t{proxy_result}\n\
         \tif [ \"$sx_use_proxy\" = yes ]; then\n\
         \t\tANTHROPIC_BASE_URL=\"http://127.0.0.1:$port\"\n\
         \t\texport ANTHROPIC_BASE_URL\n\
         \tfi\n\
         fi\n\
         if [ -z \"$CLAUDE_CONFIG_DIR\" ]; then\n\
         \tdir=$(cat {ptr} 2>/dev/null)\n\
         \tif [ -n \"$dir\" ]; then\n\
         \t\tCLAUDE_CONFIG_DIR=\"$dir\"\n\
         \t\texport CLAUDE_CONFIG_DIR\n\
         \tfi\n\
         fi\n\
         if [ -n \"$CLAUDE_CONFIG_DIR\" ]; then\n\
         \texec {sx} claude-launch --native {real} -- \"$@\"\n\
         fi\n\
         exec {real} \"$@\"\n",
        sx = sh_quote(swapdex),
        ptr = sh_quote(pointer),
        real = sh_quote(real_claude),
        proxy_result = proxy_result,
    )
}

/// Recognize native auth commands after their documented leading options.
/// Keep the same conservative boundary as the generated shell launcher: an
/// option value or prompt mentioning `auth login` never selects this route.
pub(crate) fn claude_authentication_command(args: &[String]) -> bool {
    let mut args = args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "auth" => {
                return matches!(
                    args.next(),
                    Some("login" | "logout" | "status" | "-h" | "--help")
                )
            }
            "setup-token" => return true,
            "--verbose" | "-h" | "--help" | "-v" | "--version" => {}
            "-m" | "--model" | "--permission-mode" | "--settings" | "--setting-sources"
            | "--plugin-dir" | "--plugin-url" | "--cwd" | "--debug-file" => {
                if args.next().is_none() {
                    return false;
                }
            }
            value
                if (value.starts_with("-m") && value.len() > 2)
                    || [
                        "--model=",
                        "--permission-mode=",
                        "--settings=",
                        "--setting-sources=",
                        "--plugin-dir=",
                        "--plugin-url=",
                        "--cwd=",
                        "--debug-file=",
                    ]
                    .iter()
                    .any(|prefix| value.starts_with(prefix)) => {}
            _ => return false,
        }
    }
    false
}

/// The `codex` shim. Same shape as Claude's - fill the tool's home from the
/// default pointer, and only when nothing has already chosen one - but with
/// Codex's own variable and pointer. It never mentions Claude's: one tool's shim
/// moving the other tool's account is exactly what the per-tool split prevents.
pub fn codex_shim_script(pointer: &Path, real_codex: &Path, swapdex: &Path) -> String {
    // Provider identity is persisted in Codex rollouts and filters its native
    // picker. Route the built-in provider by URL instead of creating an
    // ephemeral provider for each paying account.
    let proxy_result = proxy_result_script("codex");
    format!(
        r#"#!/bin/sh
# swapdex codex shim - launch codex in the default account's slot.
# Managed by swapdex; re-created by `swapdex shim`.
if [ -z "$CODEX_HOME" ]; then
    dir=$(cat {ptr} 2>/dev/null)
    if [ -n "$dir" ]; then
        CODEX_HOME="$dir"
        export CODEX_HOME
    fi
fi
# Parse the command separately from option values and prompt text. In
# particular, `exec login` is a model prompt, not an OAuth operation.
sx_plain=no
sx_skip=
sx_command=
sx_options=yes
sx_arg_index=0
sx_last_config=0
port=
sx_explicit_config() {{
    sx_key=$(printf '%s' "${{1%%=*}}" | tr -d '[:space:]')
    case "$sx_key" in model_provider|openai_base_url|model_providers.*) return 0 ;; esac
    return 1
}}
for a in "$@"; do
    sx_arg_index=$((sx_arg_index + 1))
    if [ "$sx_skip" = config ]; then
        if sx_explicit_config "$a"; then sx_plain=yes; fi
        sx_skip=
        continue
    fi
    if [ "$sx_skip" = value ]; then
        sx_skip=
        continue
    fi
    if [ "$sx_skip" = images ]; then
        case "$a" in -*) sx_skip= ;; *) continue ;; esac
    fi
    [ "$sx_options" = yes ] || continue
    case "$a" in
        --) sx_options=no ;;
        -c|--config) sx_last_config=$sx_arg_index; sx_skip=config ;;
        --config=*) sx_last_config=$sx_arg_index; if sx_explicit_config "${{a#--config=}}"; then sx_plain=yes; fi ;;
        -c?*) sx_last_config=$sx_arg_index; if sx_explicit_config "${{a#-c}}"; then sx_plain=yes; fi ;;
        -p|--profile) sx_skip=value ;;
        -p?*|--profile=*) ;;
        --remote|--remote-auth-token-env|--local-provider) sx_plain=yes; sx_skip=value ;;
        --remote=*|--remote-auth-token-env=*|--local-provider=*|--oss) sx_plain=yes ;;
        -i|--image) sx_skip=images ;;
        -C|--cd|-m|--model|-s|--sandbox|-a|--ask-for-approval|--add-dir|--enable|--disable) sx_skip=value ;;
        -h|--help|-V|--version) sx_plain=yes ;;
        -*) ;;
        *)
            if [ -z "$sx_command" ]; then
                sx_command="$a"
                case "$a" in login|logout|completion|mcp|mcp-server|debug|features|apply|help) sx_plain=yes ;; esac
            fi
            ;;
    esac
done
if [ "$sx_plain" = no ]; then
    if ! {sx} repair-codex-sessions --quiet; then
        printf '%s\n' 'swapdex: session repair was incomplete; run swapdex repair-codex-sessions for details.' >&2
    fi
    port=$({sx} proxy --ensure --tool codex 2>/dev/null)
    {proxy_result}
    if [ "$sx_use_proxy" = yes ]; then
        if [ "$sx_last_config" -eq 0 ]; then
            set -- -c openai_base_url="http://127.0.0.1:$port/v1" "$@"
        else
            # Codex can discard root -c flags when a subcommand has its own.
            # Rebuild the argument list so the managed URL has the same scope
            # as the caller's last real -c, without moving prompt text or --.
            sx_arg_index=0
            for sx_arg in "$@"; do
                if [ "$sx_arg_index" -eq 0 ]; then set --; fi
                sx_arg_index=$((sx_arg_index + 1))
                if [ "$sx_arg_index" -eq "$sx_last_config" ]; then
                    set -- "$@" -c openai_base_url="http://127.0.0.1:$port/v1"
                fi
                set -- "$@" "$sx_arg"
            done
        fi
    fi
fi
exec {real} "$@"
"#,
        sx = sh_quote(swapdex),
        ptr = sh_quote(pointer),
        real = sh_quote(real_codex),
        proxy_result = proxy_result,
    )
}

/// Where a running proxy announces itself: `<store_dir>/proxy`, holding
/// "<pid> <port>". Written on start, removed on exit.
pub fn proxy_marker(paths: &Paths) -> PathBuf {
    proxy_marker_for(paths, "claude-code")
}

/// One marker per tool, so a Claude proxy and a Codex proxy can both be up: they
/// carry different traffic on different ports, and a single marker would have
/// each mistake the other for itself and stop it.
pub fn proxy_marker_for(paths: &Paths, tool: &str) -> PathBuf {
    match tool {
        "codex" => paths.store_dir().join("proxy-codex"),
        // These two used to fall through to Claude's file. A gemini proxy
        // would then overwrite the marker Claude's own shim reads, and every
        // Claude session would be pointed at the Gemini proxy.
        "gemini" => paths.store_dir().join("proxy-gemini"),
        "antigravity" => paths.store_dir().join("proxy-antigravity"),
        // Claude's keeps the name it has always had, so an upgrade does not
        // orphan a proxy that is already running.
        _ => paths.store_dir().join("proxy"),
    }
}

/// A marker line the generated shim carries, so we can recognize (and never
/// re-exec) our own shim regardless of how its dir is spelled on PATH.
const SHIM_MARKER: &str = "swapdex claude shim";

/// The same, for the codex shim.
const SHIM_MARKER_CODEX: &str = "swapdex codex shim";

/// True if `path` is one of swapdex's own `claude` shims (by content), not the
/// real binary. Robust against path-spelling: a `~`, symlink, or relative PATH
/// entry that resolves to the shim dir would slip past a plain path comparison.
fn is_our_shim(path: &Path) -> bool {
    // The shim is a tiny text script; read only its head.
    let mut buf = [0u8; 256];
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    let n = f.read(&mut buf).unwrap_or(0);
    let head = String::from_utf8_lossy(&buf[..n]);
    head.contains(SHIM_MARKER) || head.contains(SHIM_MARKER_CODEX)
}

/// What a plain `claude` typed in THIS environment resolves to: the first
/// `claude` file on PATH. The bool says whether that is swapdex's own shim (by
/// content marker, robust to path spelling). `None` when PATH has no `claude`.
/// Feeds doctor's engagement check - an installed shim that PATH never reaches
/// LOOKS set up while `swapdex use` silently does nothing.
pub(crate) fn resolved_claude() -> Option<(PathBuf, bool)> {
    let path = std::env::var_os("PATH")?;
    let cwd = std::env::current_dir().ok();
    for dir in std::env::split_paths(&path) {
        let dir = if dir.is_absolute() {
            dir
        } else if let Some(cwd) = &cwd {
            cwd.join(dir)
        } else {
            continue;
        };
        let cand = dir.join("claude");
        if is_executable_file(&cand) {
            let ours = is_our_shim(&cand);
            return Some((cand, ours));
        }
    }
    None
}

/// The first `claude` on PATH that is NOT swapdex's own shim - the real one the
/// shim should exec. Skips the shim dir AND any `claude` that is itself one of
/// our shims (so re-running `swapdex shim` can never bake a self-reference).
fn find_real_claude(shim_dir: &Path) -> Option<PathBuf> {
    find_real(shim_dir, "claude")
}

/// The real `bin` on PATH, skipping our own shim dir and any shim we wrote.
fn find_real(shim_dir: &Path, bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let cwd = std::env::current_dir().ok();
    for dir in std::env::split_paths(&path) {
        // PATH entries are resolved by the shell relative to the cwd at lookup
        // time. A generated shim runs later from arbitrary project folders, so
        // persist that resolution now. Do not canonicalize it: package managers
        // deliberately put a stable symlink in PATH in front of versioned files.
        let dir = if dir.is_absolute() {
            dir
        } else if let Some(cwd) = &cwd {
            cwd.join(dir)
        } else {
            continue;
        };
        if dir == shim_dir {
            continue;
        }
        let cand = dir.join(bin);
        if is_executable_file(&cand) && !is_our_shim(&cand) {
            return Some(cand);
        }
    }
    None
}

/// Match executable lookup rather than mere directory contents. A regular file
/// without an execute bit does not win PATH and must not be baked into a shim.
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Does this profile already put the shim dir on PATH?
///
/// Compared by MEANING, not by exact text: the same directory can be written as
/// `$HOME/...`, `~/...`, or in full, and matching only the spelling this version
/// happens to emit appended the line again on every install. A real profile ended
/// up with three copies of it.
/// How far the shim actually reaches, as far as THIS process can tell.
///
/// The distinction that matters is between "not set up" and "set up, but this
/// particular shell never read the file that sets it up". A non-interactive
/// shell - a cron job, a script, `ssh host cmd` - does not source `.zshrc`, so
/// the shim directory is missing from its PATH even though every interactive
/// terminal on the machine has it. Reporting that as a fault sends someone to
/// fix a configuration that was already correct.
#[derive(Debug, PartialEq)]
pub enum ShimReach {
    /// A plain `claude` goes through the shim here and now.
    Active,
    /// The profile adds it; this shell just did not read that profile.
    ConfiguredElsewhere,
    /// Nothing puts it on PATH. This one is a real finding.
    Missing,
}

/// Decide between those three from facts the caller has already gathered.
/// Pure, so the interesting case can be tested without a shell to run in.
pub fn shim_reach(active: bool, profile_text: Option<&str>, shim_dir: &Path) -> ShimReach {
    shim_reach_at_home(active, profile_text, shim_dir, dirs::home_dir().as_deref())
}

/// The same decision using the home supplied by the caller's resolved Paths.
pub fn shim_reach_for(
    paths: &Paths,
    active: bool,
    profile_text: Option<&str>,
    shim_dir: &Path,
) -> ShimReach {
    shim_reach_at_home(active, profile_text, shim_dir, Some(paths.home()))
}

fn shim_reach_at_home(
    active: bool,
    profile_text: Option<&str>,
    shim_dir: &Path,
    home: Option<&Path>,
) -> ShimReach {
    if active {
        return ShimReach::Active;
    }
    // Scoped to THIS shim directory on purpose. Matching swapdex's marker
    // comment alone would let a profile that set up some other store excuse a
    // real finding here - and on a machine where swapdex was ever installed,
    // that marker is always present.
    match profile_text {
        Some(t) if profile_already_adds_at_home(t, shim_dir, home) => {
            ShimReach::ConfiguredElsewhere
        }
        _ => ShimReach::Missing,
    }
}

/// The shell profile's text, if there is one to read.
pub fn shell_profile_text() -> Option<(PathBuf, String)> {
    let paths = Paths::resolve().ok()?;
    shell_profile_text_for(&paths)
}

/// The shell profile's text under a specific resolved home.
pub fn shell_profile_text_for(paths: &Paths) -> Option<(PathBuf, String)> {
    let p = shell_profile_at(paths.home())?;
    let t = std::fs::read_to_string(&p).ok()?;
    Some((p, t))
}

fn profile_already_adds_at_home(profile_text: &str, shim_dir: &Path, home: Option<&Path>) -> bool {
    let full = shim_dir.to_string_lossy().to_string();
    let home = home.map(|h| h.to_string_lossy().to_string());
    // The same dir with the home prefix written the other two ways.
    let alts: Vec<String> = home
        .iter()
        .filter_map(|h| full.strip_prefix(h.as_str()))
        .flat_map(|rest| [format!("$HOME{rest}"), format!("~{rest}")])
        .collect();
    profile_text.lines().any(|l| {
        let l = l.trim();
        if !l.contains("PATH") || l.starts_with('#') {
            return false;
        }
        l.contains(&full) || alts.iter().any(|a| l.contains(a))
    })
}

/// The line a shell profile needs so the shim is found first.
fn path_line(shim_dir: &Path) -> String {
    format!("export PATH=\"{}:$PATH\"", shim_dir.display())
}

/// A marker so the block can be recognised, skipped on a re-run, and found by a
/// human wondering what edited their profile.
const PROFILE_MARKER: &str = "# added by swapdex (claude shim)";

/// The shell profile to teach: the one belonging to $SHELL, since that is the
/// shell the user actually gets. Returns `None` for a shell we should not guess at
/// (fish and friends keep PATH somewhere else entirely).
fn shell_profile_at(home: &Path) -> Option<PathBuf> {
    let shell = std::env::var("SHELL").unwrap_or_default();
    let name = shell.rsplit('/').next().unwrap_or("");
    match name {
        "zsh" => Some(home.join(".zshrc")),
        "bash" => {
            // Login shells on macOS read .bash_profile; .bashrc elsewhere. Prefer
            // whichever already exists so the line lands where it is read.
            let bp = home.join(".bash_profile");
            if bp.exists() {
                Some(bp)
            } else {
                Some(home.join(".bashrc"))
            }
        }
        _ => None,
    }
}

/// Is the shim dir already on PATH for this session?
fn already_on_path(shim_dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d == shim_dir))
        .unwrap_or(false)
}

/// What `ensure_on_path` did, so the caller can say the right thing.
pub enum PathSetup {
    /// Already reachable; nothing to do.
    AlreadyThere,
    /// The line was appended to this profile; a new shell will pick it up.
    Added(PathBuf),
    /// We will not guess for this shell - the caller prints the line to add.
    Manual,
}

/// Read a shell profile that is about to be rewritten.
///
/// Absent is an empty profile - that is a first install and appending is right.
/// Unreadable is NOT. This used to be `read_to_string(..).unwrap_or_default()`,
/// so a profile we could not read became an empty string and the write below
/// put it back with only our two lines in it, taking the user's PATH, aliases
/// and version managers with it. The one case swapdex must never get wrong is
/// the one where it destroys a file it does not own.
fn read_profile_for_edit(profile: &Path) -> Result<String> {
    match std::fs::read_to_string(profile) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => bail!(
            "cannot read {} ({e}) - refusing to rewrite it, because rewriting \
             what we could not read would replace it",
            profile.display()
        ),
    }
}

/// Put the shim dir on PATH by editing the user's shell profile, because leaving
/// this to the user means the shim silently does nothing: it is installed, PATH
/// never reaches it, and `swapdex use` appears to work while changing nothing.
/// Idempotent - a profile that already carries the marker is left alone.
pub fn ensure_on_path(shim_dir: &Path) -> Result<PathSetup> {
    let paths = Paths::resolve().context("resolve paths for shell profile")?;
    ensure_on_path_for(&paths, shim_dir)
}

/// Put the shim on PATH using the home selected by `paths`, including a rooted
/// library call that has no matching process-wide HOME or SWAPDEX_ROOT.
pub fn ensure_on_path_for(paths: &Paths, shim_dir: &Path) -> Result<PathSetup> {
    if already_on_path(shim_dir) {
        return Ok(PathSetup::AlreadyThere);
    }
    let Some(profile) = shell_profile_at(paths.home()) else {
        return Ok(PathSetup::Manual);
    };
    let existing = read_profile_for_edit(&profile)?;
    let line = path_line(shim_dir);
    if existing.contains(PROFILE_MARKER)
        || profile_already_adds_at_home(&existing, shim_dir, Some(paths.home()))
    {
        // Written before but not active yet: the user has not started a new shell.
        return Ok(PathSetup::Added(profile));
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("\n{PROFILE_MARKER}\n{line}\n"));
    std::fs::write(&profile, out).with_context(|| format!("edit {}", profile.display()))?;
    Ok(PathSetup::Added(profile))
}

/// Whether it is safe to pin the proxy's address into the tool's own config.
#[derive(Debug, PartialEq)]
pub enum PinVerdict {
    /// Pin it at this port: a service keeps the proxy alive.
    Pin(u16),
    /// Do not pin: nothing would restart the proxy, and a pinned address with
    /// no proxy behind it makes the tool unusable rather than merely unswitched.
    RefuseNoService,
}

/// PATH is not a reliable way to reach the proxy.
///
/// The shim only fires when it WINS the PATH, and on a real machine another
/// `claude` sat ahead of it - so the proxy was never used and `serve` silently
/// changed nothing, on two machines, for a day. Every competing proxy-based
/// switcher writes the base URL into the tool's own config instead, which no
/// PATH ordering can undo.
///
/// The catch is the failure mode: a pinned address with no proxy behind it
/// makes the tool unusable, not merely unswitched. So this only says yes when
/// something will restart the proxy.
pub fn pin_verdict(service_installed: bool, port: u16) -> PinVerdict {
    if service_installed {
        PinVerdict::Pin(port)
    } else {
        PinVerdict::RefuseNoService
    }
}

/// The settings object with the proxy address added, everything else intact.
///
/// These settings hold the user's model, hooks and permissions; rewriting them
/// to add one key would be a far worse bug than the one being fixed.
/// The loopback port a settings file pins Claude Code to, if it pins one.
///
/// `pin_base_url` refuses to write the address unless a proxy is alive, so the
/// moment of pinning is safe - and then nothing watched it. When the proxy later
/// went away, every session on that machine got "Connection refused", because
/// the settings still named an address nobody was answering. Reading the pin
/// back is what lets the health check say so.
///
/// `None` for an address this check cannot speak for: only a loopback pin is
/// swapdex's to verify.
/// Is the pinned address this proxy's own?
///
/// When the proxy stops, the address in settings.json goes on naming a port
/// nobody answers, and a session started in that window is bricked for its whole
/// life - the address is read once, at startup. Withdrawing the pin on the way
/// out lets those sessions go direct instead.
///
/// Only ours: a pin naming another port belongs to a second proxy or to a choice
/// the user made by hand, and taking that away would be its own outage.
pub fn pin_is_ours(pinned: Option<u16>, mine: u16) -> bool {
    pinned == Some(mine)
}

/// Take the pin out of the settings file, if it is this proxy's.
///
/// Every other key is preserved - the file holds the user's model, hooks and
/// permissions - and the write is atomic, so a stop cannot leave a half-written
/// settings file behind.
pub fn withdraw_pin(paths: &Paths, mine: u16) -> bool {
    let file = paths.claude_dir().join("settings.json");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return false;
    };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return false;
    };
    if !pin_is_ours(pinned_port(&v), mine) {
        return false;
    }
    let emptied = match v.get_mut("env").and_then(|e| e.as_object_mut()) {
        Some(env) => {
            env.remove("ANTHROPIC_BASE_URL");
            env.is_empty()
        }
        None => false,
    };
    if emptied {
        if let Some(o) = v.as_object_mut() {
            o.remove("env");
        }
    }
    let Ok(body) = serde_json::to_vec_pretty(&v) else {
        return false;
    };
    crate::atomic::write_secret(&file, &body).is_ok()
}

pub fn pinned_port(settings: &serde_json::Value) -> Option<u16> {
    let url = settings.get("env")?.get("ANTHROPIC_BASE_URL")?.as_str()?;
    let rest = url.strip_prefix("http://")?;
    let (host, port) = rest.trim_end_matches('/').rsplit_once(':')?;
    if host != "127.0.0.1" && host != "localhost" {
        return None;
    }
    port.parse().ok()
}

pub fn with_base_url(settings: &serde_json::Value, port: u16) -> serde_json::Value {
    let mut out = settings.clone();
    if !out.is_object() {
        out = serde_json::json!({});
    }
    let obj = out.as_object_mut().expect("object");
    let env = obj.entry("env").or_insert_with(|| serde_json::json!({}));
    if !env.is_object() {
        *env = serde_json::json!({});
    }
    env.as_object_mut().expect("env object").insert(
        "ANTHROPIC_BASE_URL".to_string(),
        serde_json::Value::String(format!("http://127.0.0.1:{port}")),
    );
    out
}

/// Pin the proxy's address into the tool's own settings, so reaching it no
/// longer depends on winning the PATH.
///
/// Returns the path written, or `None` when there is no service to keep the
/// proxy alive - pinning then would trade "switching does nothing" for "the
/// tool does not start", which is worse.
pub fn pin_base_url(paths: &Paths, port: u16, service_installed: bool) -> Result<Option<PathBuf>> {
    if pin_verdict(service_installed, port) == PinVerdict::RefuseNoService {
        return Ok(None);
    }
    let file = paths.claude_dir().join("settings.json");
    let current: serde_json::Value = std::fs::read(&file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let next = with_base_url(&current, port);
    if next == current {
        return Ok(Some(file));
    }
    // These settings hold the user's model, hooks and permissions. Keep a copy
    // before touching them, and write atomically so a crash cannot leave the
    // file half-written and the tool unable to start.
    if file.exists() {
        let _ = std::fs::copy(&file, file.with_extension("json.swapdex-bak"));
    }
    if let Some(d) = file.parent() {
        std::fs::create_dir_all(d).ok();
    }
    let bytes = serde_json::to_vec_pretty(&next).context("serialize settings.json")?;
    // 0600 via the same atomic path the credential writes use: this file is
    // the user's own and only ever read as them, and a half-written
    // settings.json would stop the tool from starting.
    crate::atomic::write_secret(&file, &bytes).context("write settings.json")?;
    Ok(Some(file))
}

/// Where the shim stands in the PATH race.
#[derive(Debug, PartialEq)]
pub enum PathVerdict {
    /// The shim's directory is the first one holding this tool: it will run.
    Wins,
    /// On the PATH, but an earlier directory holds the same name and wins.
    Shadowed(String),
    /// Not on the PATH at all.
    Absent,
}

/// Does the shim actually WIN the PATH, or merely appear on it?
///
/// swapdex used to check membership only and report "a plain `claude` goes
/// through it" - while an earlier entry held the name, so the shim never fired,
/// the proxy was never used, and `swapdex serve` silently did nothing. The
/// install said everything was fine, which is why nobody suspected the PATH.
pub fn path_verdict(shim_dir: &std::path::Path, entries: &[&str]) -> PathVerdict {
    path_verdict_for(shim_dir, entries, "claude")
}

/// Report whether the shim for one concrete executable wins `PATH`.
pub fn path_verdict_for(shim_dir: &std::path::Path, entries: &[&str], binary: &str) -> PathVerdict {
    path_verdict_with(shim_dir, entries, &|d| {
        is_executable_file(&std::path::Path::new(d).join(binary))
    })
}

/// The same, with the "does this directory hold the tool" test injected so the
/// ordering logic is testable without touching the filesystem.
pub fn path_verdict_with(
    shim_dir: &std::path::Path,
    entries: &[&str],
    holds_tool: &dyn Fn(&str) -> bool,
) -> PathVerdict {
    let shim = shim_dir.to_string_lossy();
    if !entries.iter().any(|e| *e == shim) {
        return PathVerdict::Absent;
    }
    for e in entries {
        if *e == shim {
            return PathVerdict::Wins;
        }
        // Only a directory that actually holds the tool can shadow it; naming
        // an empty earlier entry would send the reader to fix the wrong thing.
        if holds_tool(e) {
            return PathVerdict::Shadowed((*e).to_string());
        }
    }
    PathVerdict::Absent
}

/// Install (or refresh) the shim. Returns (shim_path, shim_dir) so the caller
/// can print PATH guidance.
pub fn install(paths: &Paths) -> Result<(PathBuf, PathBuf)> {
    install_claude(paths)?.context("could not find the real `claude` on PATH - install it first")
}

/// Install the Claude shim when Claude is available. A machine that only uses
/// another supported client is valid, so absence is reported to the caller.
pub fn install_claude(paths: &Paths) -> Result<Option<(PathBuf, PathBuf)>> {
    let shim = shim_path(paths);
    let shim_dir = shim
        .parent()
        .map(|p| p.to_path_buf())
        .context("shim path has no parent")?;
    let Some(real) = find_real_claude(&shim_dir) else {
        return Ok(None);
    };
    let pointer = paths.store_dir().join("active-claude");
    // The shim calls back into THIS binary, by absolute path: whatever swapdex
    // installed the shim is the one that will start its proxy, even if PATH
    // later changes or a different build lands ahead of it.
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("swapdex"));
    std::fs::create_dir_all(&shim_dir).context("create shim dir")?;
    std::fs::write(&shim, shim_script(&pointer, &real, &me)).context("write shim")?;
    make_executable(&shim)?;
    Ok(Some((shim, shim_dir)))
}

/// Install the `codex` shim beside Claude's. Returns the path, or `None` when
/// there is no real `codex` on PATH to wrap - not having Codex installed is not
/// an error, it just means there is nothing to shim.
pub fn install_codex(paths: &Paths) -> Result<Option<PathBuf>> {
    let shim = shim_path_for(paths, "codex");
    let shim_dir = shim
        .parent()
        .map(|p| p.to_path_buf())
        .context("shim path has no parent")?;
    let Some(real) = find_real(&shim_dir, "codex") else {
        return Ok(None);
    };
    let pointer = paths.store_dir().join("active-codex");
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("swapdex"));
    std::fs::create_dir_all(&shim_dir).context("create shim dir")?;
    std::fs::write(&shim, codex_shim_script(&pointer, &real, &me)).context("write codex shim")?;
    make_executable(&shim)?;
    Ok(Some(shim))
}

fn make_executable(p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755))
            .context("chmod shim")?;
    }
    Ok(())
}

#[cfg(test)]
mod reach_tests {
    use super::*;

    /// The case that sent me chasing a non-existent bug: doctor run over ssh
    /// reported the shim inactive, because `ssh host cmd` starts a shell that
    /// never reads .zshrc. The machine was configured correctly the whole time.
    #[test]
    fn a_shell_that_never_read_the_profile_is_not_a_broken_setup() {
        let dir = Path::new("/Users/x/Library/Application Support/swapdex/bin");
        let zshrc = "export PATH=\"/Users/x/Library/Application Support/swapdex/bin:$PATH\"\n";
        assert_eq!(
            shim_reach(false, Some(zshrc), dir),
            ShimReach::ConfiguredElsewhere
        );
    }

    #[test]
    fn nothing_putting_it_on_path_is_still_a_real_finding() {
        let dir = Path::new("/Users/x/Library/Application Support/swapdex/bin");
        assert_eq!(
            shim_reach(false, Some("export EDITOR=vim\n"), dir),
            ShimReach::Missing
        );
        assert_eq!(shim_reach(false, None, dir), ShimReach::Missing);
    }

    /// Caught by an existing doctor test rather than by me: swapdex writes a
    /// marker comment when it edits a profile, so on any machine where it has
    /// ever run, matching that marker alone would silence a genuine finding for
    /// a different store. The profile has to add THIS directory.
    #[test]
    fn a_profile_that_set_up_some_other_store_excuses_nothing() {
        let mine = Path::new("/tmp/store-a/bin");
        let theirs = "# added by swapdex\nexport PATH=\"/tmp/store-b/bin:$PATH\"\n";
        assert_eq!(shim_reach(false, Some(theirs), mine), ShimReach::Missing);
    }

    #[test]
    fn a_shim_that_works_here_needs_no_explaining() {
        let dir = Path::new("/tmp/bin");
        assert_eq!(shim_reach(true, None, dir), ShimReach::Active);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    // Codex reads CODEX_HOME the way Claude reads CLAUDE_CONFIG_DIR, so its shim
    // is the same shape: fill the home from the pointer, and only when nothing
    // has already chosen one - `swapdex run` sets it explicitly, and overriding
    // that would open every account as the default one.
    // The shim is also how Codex reaches proxy mode. Codex only sends its own
    // OAuth to a provider that declares no api key, so the block names a base
    // url and a wire protocol and nothing else - adding an env_key would make it
    // send an API key instead and the whole mechanism would fall over.
    // The same directory can be spelled three ways, and matching only the one
    // this version emits appended the line again on every install - a real
    // profile ended up with three copies.
    #[test]
    fn an_existing_path_line_is_recognised_however_it_is_spelled() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path();
        let shim_dir = home.join("Library/Application Support/swapdex/bin");
        let full = shim_dir.display().to_string();
        for spelling in [
            format!("export PATH=\"{full}:$PATH\""),
            "export PATH=\"$HOME/Library/Application Support/swapdex/bin:$PATH\"".to_string(),
            "export PATH=\"~/Library/Application Support/swapdex/bin:$PATH\"".to_string(),
        ] {
            assert!(
                profile_already_adds_at_home(
                    &format!("# something\n{spelling}\n"),
                    &shim_dir,
                    Some(home),
                ),
                "not recognised: {spelling}"
            );
        }
        // A profile that does NOT add it is left alone, and a commented-out line
        // is not an active entry.
        assert!(!profile_already_adds_at_home(
            "export PATH=\"/usr/local/bin:$PATH\"\n",
            &shim_dir,
            Some(home),
        ));
        assert!(!profile_already_adds_at_home(
            &format!("# export PATH=\"{full}:$PATH\"\n"),
            &shim_dir,
            Some(home),
        ));
        // A line merely MENTIONING the dir without touching PATH is not one.
        assert!(!profile_already_adds_at_home(
            &format!("echo {full}\n"),
            &shim_dir,
            Some(home),
        ));
    }

    // Signing in must reach the vendor directly: the OAuth exchange is between
    // the browser and the real API, and a proxy in the middle both breaks the code
    // exchange and answers with whichever account it already holds - so a fresh
    // slot looks signed in as someone else, or its prompt takes no input at all.
    #[test]
    fn the_shim_does_not_proxy_a_sign_in() {
        let s = shim_script(
            Path::new("/store/active-claude"),
            Path::new("/usr/bin/claude"),
            Path::new("/bin/swapdex"),
        );
        // The proxy is asked for only when this is not a sign-in.
        assert!(
            s.contains("sx_plain=no"),
            "it decides whether this is a sign-in: {s}"
        );
        for command in ["auth:login", "auth:logout", "auth:status", "setup-token:"] {
            assert!(s.contains(command), "recognised: {command}");
        }
        // And the base-url export sits INSIDE that condition, not before it.
        let guard = s.find("if [ \"$sx_plain\" = no ]").expect("the guard");
        let export = s
            .find("ANTHROPIC_BASE_URL=\"http://")
            .expect("the proxy export");
        assert!(
            guard < export,
            "the proxy address is only set when not signing in"
        );
    }

    #[test]
    fn the_codex_shim_routes_through_a_running_proxy() {
        let s = codex_shim_script(
            Path::new("/store/active-codex"),
            Path::new("/usr/bin/codex"),
            Path::new("/bin/swapdex"),
        );
        assert!(
            s.contains("proxy --ensure --tool codex"),
            "asks swapdex for a live codex proxy: {s}"
        );
        assert!(
            s.contains("openai_base_url=\"http://127.0.0.1:$port/v1\""),
            "routes the built-in provider without changing session identity: {s}"
        );
        assert!(!s.contains("set -- -c model_provider="));
        // A validated port is the only result that adds the proxy override.
        assert!(
            s.contains("if [ \"$sx_use_proxy\" = yes ]"),
            "the overrides are conditional: {s}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn codex_proxy_override_shares_the_last_real_config_scope() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;

        let root = tempfile::tempdir().unwrap();
        let pointer = root.path().join("active-codex");
        let real = root.path().join("real codex");
        let swapdex = root.path().join("fake swapdex");
        let shim = root.path().join("codex shim");
        std::fs::write(&pointer, root.path().join("codex-home").to_str().unwrap()).unwrap();
        std::fs::write(&real, "#!/bin/sh\nprintf '%s\\0' \"$@\"\n").unwrap();
        std::fs::write(
            &swapdex,
            "#!/bin/sh\ncase \"$1\" in proxy) printf 8788 ;; esac\n",
        )
        .unwrap();
        std::fs::write(&shim, codex_shim_script(&pointer, &real, &swapdex)).unwrap();
        for path in [&real, &swapdex] {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let base = "openai_base_url=http://127.0.0.1:8788/v1";
        for (input, expected) in [
            (vec!["exec", "prompt"], vec!["-c", base, "exec", "prompt"]),
            (
                vec!["exec", "-c", "model_reasoning_effort=low", "prompt"],
                vec![
                    "exec",
                    "-c",
                    base,
                    "-c",
                    "model_reasoning_effort=low",
                    "prompt",
                ],
            ),
            (
                vec!["exec", "--config", "model_reasoning_effort=low", "prompt"],
                vec![
                    "exec",
                    "-c",
                    base,
                    "--config",
                    "model_reasoning_effort=low",
                    "prompt",
                ],
            ),
            (
                vec!["-c", "model_reasoning_effort=low", "exec", "prompt"],
                vec![
                    "-c",
                    base,
                    "-c",
                    "model_reasoning_effort=low",
                    "exec",
                    "prompt",
                ],
            ),
            (
                vec!["-c", "model=first", "exec", "-c", "model=second", "prompt"],
                vec![
                    "-c",
                    "model=first",
                    "exec",
                    "-c",
                    base,
                    "-c",
                    "model=second",
                    "prompt",
                ],
            ),
            (
                vec![
                    "exec",
                    "resume",
                    "--last",
                    "-c",
                    "model_reasoning_effort=low",
                    "prompt",
                ],
                vec![
                    "exec",
                    "resume",
                    "--last",
                    "-c",
                    base,
                    "-c",
                    "model_reasoning_effort=low",
                    "prompt",
                ],
            ),
            (
                vec!["exec", "-c", "model_reasoning_effort = \"low\"", "prompt"],
                vec![
                    "exec",
                    "-c",
                    base,
                    "-c",
                    "model_reasoning_effort = \"low\"",
                    "prompt",
                ],
            ),
            (
                vec!["exec", "-cmodel_reasoning_effort=low", "prompt"],
                vec!["exec", "-c", base, "-cmodel_reasoning_effort=low", "prompt"],
            ),
            (
                vec!["exec", "--config=model_reasoning_effort=low", "prompt"],
                vec![
                    "exec",
                    "-c",
                    base,
                    "--config=model_reasoning_effort=low",
                    "prompt",
                ],
            ),
            (
                vec!["exec", "-C", "-c", "-c", "model=second", "--", "-c"],
                vec![
                    "exec",
                    "-C",
                    "-c",
                    "-c",
                    base,
                    "-c",
                    "model=second",
                    "--",
                    "-c",
                ],
            ),
            (
                vec!["exec", "--", "-c", "prompt"],
                vec!["-c", base, "exec", "--", "-c", "prompt"],
            ),
            (
                vec!["exec", "-c", "model_provider=fixture", "prompt"],
                vec!["exec", "-c", "model_provider=fixture", "prompt"],
            ),
        ] {
            let output = Command::new("sh")
                .arg(&shim)
                .args(&input)
                .env("CODEX_HOME", root.path().join("codex-home"))
                .env("SWAPDEX_ROOT", root.path())
                .output()
                .unwrap();
            assert!(output.status.success(), "{input:?}: {:?}", output.stderr);
            let mut parts: Vec<_> = output.stdout.split(|byte| *byte == 0).collect();
            assert_eq!(parts.pop(), Some(&b""[..]), "missing final NUL");
            let actual: Vec<_> = parts
                .into_iter()
                .map(|bytes| String::from_utf8(bytes.to_vec()).unwrap())
                .collect();
            assert_eq!(actual, expected, "{input:?}");
        }
    }

    #[test]
    fn the_codex_shim_repairs_the_resolved_home_before_routing() {
        let s = codex_shim_script(
            Path::new("/store/active-codex"),
            Path::new("/usr/bin/codex"),
            Path::new("/bin/swapdex"),
        );
        let home = s.find("CODEX_HOME=").unwrap();
        let repair = s.find("repair-codex-sessions --quiet").unwrap();
        let proxy = s.find("proxy --ensure").unwrap();
        assert!(home < repair && repair < proxy);
    }

    #[test]
    fn the_codex_shim_points_codex_home_at_the_default_slot() {
        let s = codex_shim_script(
            Path::new("/store/active-codex"),
            Path::new("/usr/bin/codex"),
            Path::new("/bin/swapdex"),
        );
        assert!(s.starts_with("#!/bin/sh"));
        assert!(
            s.contains("/store/active-codex"),
            "reads codex's own pointer"
        );
        assert!(s.contains("/usr/bin/codex"), "execs the real codex");
        assert!(s.contains("CODEX_HOME="), "sets the slot env");
        assert!(
            s.contains("if [ -z \"$CODEX_HOME\" ]"),
            "an explicit CODEX_HOME is a decision already made"
        );
        assert!(s.contains("exec "), "replaces the process");
        // It must never touch Claude's variables - one tool's shim moving the
        // other tool's account is the bug this whole split exists to prevent.
        assert!(!s.contains("CLAUDE_CONFIG_DIR"));
        assert!(!s.contains("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn script_references_pointer_real_claude_and_config_dir() {
        let s = shim_script(
            Path::new("/store/active-claude"),
            Path::new("/usr/bin/claude"),
            Path::new("/bin/swapdex"),
        );
        assert!(s.starts_with("#!/bin/sh"));
        assert!(s.contains("/store/active-claude"), "reads the pointer");
        assert!(s.contains("/usr/bin/claude"), "execs the real claude");
        assert!(s.contains("CLAUDE_CONFIG_DIR="), "sets the slot env");
        assert!(s.contains("exec "), "replaces the process");
    }

    // A running proxy is picked up automatically, and a STALE marker is not: the
    // pid gate is what keeps a killed proxy from sending claude at a dead port.
    // `swapdex run <account>` sets CLAUDE_CONFIG_DIR to that account's slot and
    // then execs claude - which finds the shim. If the shim overwrote it with the
    // default pointer (it did), every account opened as the default one, so
    // signing a second account in was impossible.
    #[test]
    fn an_explicit_config_dir_wins_over_the_default_pointer() {
        let s = shim_script(
            Path::new("/store/active-claude"),
            Path::new("/usr/bin/claude"),
            Path::new("/bin/swapdex"),
        );
        assert!(
            s.contains("if [ -z \"$CLAUDE_CONFIG_DIR\" ]"),
            "the pointer only fills in when nothing chose a dir: {s}"
        );
        // The pointer is still applied when nothing else has.
        assert!(s.contains("/store/active-claude"), "{s}");
        assert!(s.contains("CLAUDE_CONFIG_DIR="), "{s}");
    }

    #[test]
    fn script_gets_its_proxy_from_swapdex_and_checks_the_result() {
        let s = shim_script(
            Path::new("/store/active-claude"),
            Path::new("/usr/bin/claude"),
            Path::new("/bin/swapdex"),
        );
        assert!(
            s.contains("'/bin/swapdex' proxy --ensure"),
            "asks swapdex by absolute path, so the user starts nothing: {s}"
        );
        assert!(
            s.contains("ANTHROPIC_BASE_URL"),
            "points claude at the proxy"
        );
        assert!(
            s.contains("http://127.0.0.1:$port"),
            "loopback only, port from swapdex"
        );
        assert!(
            s.contains("2>/dev/null")
                && s.contains("sx_proxy_status=$?")
                && s.contains("if [ \"$sx_use_proxy\" = yes ]"),
            "the proxy status and validated port decide whether Claude is routed: {s}"
        );
    }

    #[test]
    fn script_quotes_paths_with_spaces() {
        let s = shim_script(
            Path::new("/a b/active-claude"),
            Path::new("/c d/claude"),
            Path::new("/e f/swapdex"),
        );
        assert!(s.contains("'/a b/active-claude'"), "pointer is quoted");
        assert!(s.contains("'/e f/swapdex'"), "swapdex path is quoted");
        assert!(s.contains("'/c d/claude'"), "real claude is quoted");
    }

    #[test]
    fn recognizes_our_own_shim_by_marker() {
        // The generated shim carries the marker, so find_real_claude never bakes
        // a self-reference even if the shim dir is spelled oddly on PATH.
        let dir = tempfile::tempdir().unwrap();
        let shim = dir.path().join("claude");
        std::fs::write(
            &shim,
            shim_script(Path::new("/p"), Path::new("/real"), Path::new("/sx")),
        )
        .unwrap();
        assert!(is_our_shim(&shim), "our shim is recognized by its marker");
        let real = dir.path().join("real-claude");
        std::fs::write(&real, "#!/bin/sh\nexec node /opt/claude \"$@\"\n").unwrap();
        assert!(!is_our_shim(&real), "a real claude is not flagged");
    }
}

/// The swapdex binary a generated shim calls, recovered from the shim itself.
///
/// A shim embeds an ABSOLUTE path to whichever swapdex wrote it. With two copies
/// installed - npm and brew, say - updating one leaves the shims calling the
/// other, and nothing on screen says so: a fix ships, the user updates, and the
/// tool goes on running the old binary. That went unnoticed for a full day once.
pub fn swapdex_path_in(text: &str) -> Option<PathBuf> {
    // `sh_quote` wraps the path in single quotes, doubling any quote inside. Match
    // the CALL, not the word: the script's own comments mention a proxy before it
    // ever asks for one, and anchoring on " proxy" alone read one of those.
    let at = text.find(" proxy --ensure")?;
    // The call is `port=$('<path>' proxy --ensure ...)`, so the token starts right
    // after the substitution opens. Scanning back for a quote instead lands INSIDE
    // the `'\''` escape that a path containing a quote is written with.
    let start = text[..at].rfind("$(")? + 2;
    let token = text[start..at].trim();
    let inner = token.strip_prefix('\'')?.strip_suffix('\'')?;
    let path = inner.replace("'\\''", "'");
    (!path.is_empty()).then(|| PathBuf::from(path))
}

#[cfg(test)]
mod embedded_path_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn the_path_comes_back_out_of_a_shim_we_wrote() {
        for shim in [
            shim_script(
                Path::new("/store/active-claude"),
                Path::new("/usr/bin/claude"),
                Path::new("/opt/homebrew/bin/swapdex"),
            ),
            codex_shim_script(
                Path::new("/store/active-codex"),
                Path::new("/usr/bin/codex"),
                Path::new("/opt/homebrew/bin/swapdex"),
            ),
        ] {
            assert_eq!(
                swapdex_path_in(&shim).as_deref(),
                Some(Path::new("/opt/homebrew/bin/swapdex"))
            );
        }
    }

    /// A home directory with a quote in it is rare and still has to round-trip -
    /// getting it wrong would report a mismatch that is not there.
    #[test]
    fn a_quoted_path_survives_the_round_trip() {
        let odd = Path::new("/Users/o'brien/.local/bin/swapdex");
        let shim = codex_shim_script(Path::new("/p"), Path::new("/usr/bin/codex"), odd);
        assert_eq!(swapdex_path_in(&shim).as_deref(), Some(odd));
    }

    #[test]
    fn something_that_is_not_our_shim_yields_nothing() {
        assert_eq!(
            swapdex_path_in("#!/bin/sh\nexec /usr/bin/claude \"$@\"\n"),
            None
        );
    }
}

/// Every distinct `swapdex` executable reachable on `path_var`, in PATH order and
/// with symlinks resolved, so two entries pointing at one file count once.
///
/// Two real copies - npm and brew, say - means one of them is shadowed, and
/// updating the shadowed one changes nothing anybody can see.
pub fn swapdex_copies_on(path_var: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in path_var.split(':').filter(|d| !d.is_empty()) {
        let cand = Path::new(dir).join("swapdex");
        if !cand.is_file() {
            continue;
        }
        let real = std::fs::canonicalize(&cand).unwrap_or(cand);
        if !out.contains(&real) {
            out.push(real);
        }
    }
    out
}

#[cfg(test)]
mod copies_tests {
    use super::*;

    #[test]
    fn two_entries_for_one_file_are_one_install() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("cellar");
        std::fs::create_dir_all(&real).unwrap();
        std::fs::write(real.join("swapdex"), b"#!/bin/sh\n").unwrap();
        let linked = root.path().join("bin");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(real.join("swapdex"), linked.join("swapdex")).unwrap();

        let path = format!("{}:{}", linked.display(), real.display());
        assert_eq!(
            swapdex_copies_on(&path).len(),
            1,
            "a symlink is not a second install"
        );
    }

    #[test]
    fn two_real_files_are_two_installs_in_path_order() {
        let root = tempfile::tempdir().unwrap();
        let (a, b) = (root.path().join("npm"), root.path().join("brew"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("swapdex"), b"#!/bin/sh\n").unwrap();
        }
        let found = swapdex_copies_on(&format!("{}:{}", a.display(), b.display()));
        assert_eq!(found.len(), 2, "both are real, and one shadows the other");
        // Compare against the RESOLVED path: macOS canonicalizes a temp dir from
        // /var/... to /private/var/..., so the raw path is not a prefix of the
        // answer even when it is the same file.
        let a_real = std::fs::canonicalize(&a).unwrap();
        assert!(
            found[0].starts_with(&a_real),
            "the one that wins comes first"
        );
    }

    #[test]
    fn nothing_installed_is_not_a_problem() {
        assert!(swapdex_copies_on("/nonexistent-a:/nonexistent-b").is_empty());
    }
}

/// A line that marks a file as one of our shims, for tests.
#[cfg(test)]
fn shim_marker_line() -> String {
    format!("#!/bin/sh\n# {SHIM_MARKER_CODEX}\n")
}

/// The real tool binary for `tool`, skipping our own shim wherever it sits.
///
/// Signing in must not go through the shim. For Codex the shim adds the proxy
/// provider on any run it does not recognise as a plain one, and a bare launch
/// is not recognised - so the sign-in went through the proxy, which answered
/// with the account it was already serving. An account with no login of its own
/// came up looking signed in, and every turn in it was billed elsewhere.
pub fn real_tool(paths: &Paths, tool: &str) -> Option<PathBuf> {
    find_real(&shim_bin_dir(paths), crate::commands::tool_binary(tool))
}

#[cfg(test)]
mod real_tool_tests {
    use super::*;

    /// Whatever else changes, a sign-in must never run the shim: that is the
    /// path that puts the proxy in front of it.
    #[test]
    fn the_shim_dir_is_stepped_over() {
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::rooted(root.path());
        let dir = shim_bin_dir(&paths);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("codex"), shim_marker_line()).unwrap();
        if let Some(found) = real_tool(&paths, "codex") {
            assert!(
                !found.starts_with(&dir),
                "resolved {} inside the shim dir",
                found.display()
            );
        }
    }
}

#[cfg(test)]
mod shadow_tests {
    use super::*;

    /// Being ON the PATH is not the same as WINNING it. swapdex checked only
    /// membership and reported "a plain `claude` goes through it" while another
    /// entry earlier in the PATH was the one actually run - so the shim never
    /// fired, the proxy was never used, and `swapdex serve` silently did
    /// nothing. Found on a real machine after three wrong diagnoses.
    #[test]
    fn a_shim_that_loses_the_path_is_reported_as_shadowed() {
        let shim = std::path::Path::new("/home/u/.local/share/swapdex/bin");
        // The shim's directory comes first: it wins.
        assert_eq!(
            path_verdict(shim, &["/home/u/.local/share/swapdex/bin", "/usr/bin"]),
            PathVerdict::Wins
        );
        // Present, but something earlier holds the name. The "holds it" test is
        // injected so the ordering rule is proven without a real filesystem.
        assert_eq!(
            path_verdict_with(
                shim,
                &["/home/u/.local/bin", "/home/u/.local/share/swapdex/bin"],
                &|d| d == "/home/u/.local/bin"
            ),
            PathVerdict::Shadowed("/home/u/.local/bin".into())
        );
        // Not on the PATH at all - a different problem with a different fix.
        assert_eq!(
            path_verdict(shim, &["/usr/bin", "/bin"]),
            PathVerdict::Absent
        );
    }

    /// Only a directory that actually HOLDS a `claude` can shadow one. An
    /// earlier PATH entry with no such file is irrelevant, and naming it would
    /// send the reader to fix the wrong thing.
    #[test]
    fn an_earlier_directory_without_the_tool_does_not_shadow() {
        let shim = std::path::Path::new("/shim/bin");
        assert_eq!(
            path_verdict_with(shim, &["/empty", "/shim/bin"], &|d| d != "/empty"),
            PathVerdict::Wins
        );
    }
}

#[cfg(test)]
mod pin_base_url_tests {
    use super::*;

    /// PATH is not a reliable way to reach the proxy. The shim only fires when
    /// it WINS the PATH, and on a real machine another `claude` sat ahead of
    /// it - so the proxy was never used and `serve` silently changed nothing,
    /// on two machines, for a day. Every competing proxy-based switcher
    /// (cc-switch, claude-code-router, codex-pooler) writes the base URL into
    /// the tool's own config instead, which no PATH ordering can undo.
    ///
    /// The catch: a pinned base URL with no proxy behind it makes the tool
    /// unusable rather than merely unswitched. So it is only safe to pin when
    /// something restarts the proxy - a service - and refusing is the right
    /// answer otherwise.
    #[test]
    fn a_base_url_is_pinned_only_when_a_service_keeps_the_proxy_alive() {
        assert_eq!(pin_verdict(true, 8787), PinVerdict::Pin(8787));
        assert_eq!(pin_verdict(false, 8787), PinVerdict::RefuseNoService);
    }

    /// Pinning must not disturb the rest of the file: these settings hold the
    /// user's model, hooks and permissions, and rewriting them to add one key
    /// would be a far worse bug than the one being fixed.
    #[test]
    fn pinning_preserves_every_other_setting() {
        let before = serde_json::json!({
            "model": "opus",
            "env": {"FOO": "1"},
            "permissions": {"allow": ["Bash"]}
        });
        let after = with_base_url(&before, 8787);
        assert_eq!(after["model"], "opus");
        assert_eq!(after["permissions"]["allow"][0], "Bash");
        assert_eq!(after["env"]["FOO"], "1");
        assert_eq!(after["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:8787");
    }

    /// A file with no env object at all still gets one.
    #[test]
    fn pinning_works_on_settings_that_have_no_env_yet() {
        let after = with_base_url(&serde_json::json!({"model": "opus"}), 9001);
        assert_eq!(after["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:9001");
        assert_eq!(after["model"], "opus");
    }
}

#[cfg(test)]
mod pinned_port_tests {
    use super::*;

    /// The pin has to be readable back, or nothing can check it is still good.
    ///
    /// `pin_base_url` refuses to write the address unless a proxy is alive, so
    /// the moment of pinning is safe. Nothing watched it afterwards. When the
    /// proxy later went away, every session on that machine got "Connection
    /// refused" - the settings still named an address nobody was answering -
    /// and `doctor` said the service was fine, because it looked at the unit
    /// and the process and never at the address itself.
    #[test]
    fn the_pinned_port_can_be_read_back_out_of_settings() {
        let pinned = serde_json::json!({
            "env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:8787"},
            "model": "opus"
        });
        assert_eq!(pinned_port(&pinned), Some(8787));

        let other_port = serde_json::json!({
            "env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:9001"}
        });
        assert_eq!(pinned_port(&other_port), Some(9001));

        // Not pinned at all, and pinned somewhere this check cannot speak for.
        assert_eq!(pinned_port(&serde_json::json!({"model": "opus"})), None);
        assert_eq!(
            pinned_port(
                &serde_json::json!({"env": {"ANTHROPIC_BASE_URL": "https://api.anthropic.com"}})
            ),
            None
        );
    }
}

#[cfg(test)]
mod unpin_on_exit_tests {
    use super::*;

    /// A pin must be withdrawn only when it points at THIS proxy.
    ///
    /// When the proxy stops, the address in settings.json goes on naming a port
    /// nobody answers - and a session started in that window is bricked for its
    /// whole life, because the address is read once at startup. Withdrawing it
    /// on the way out lets those sessions go direct instead.
    ///
    /// It must never touch a pin that belongs to something else: a second proxy
    /// on another port, or an address the user set by hand.
    #[test]
    fn only_this_proxy_pin_is_withdrawn() {
        assert!(pin_is_ours(Some(8787), 8787), "our own port");
        assert!(!pin_is_ours(Some(9001), 8787), "another proxy port");
        assert!(
            !pin_is_ours(None, 8787),
            "nothing pinned - nothing to withdraw"
        );
    }
}

#[cfg(test)]
mod withdraw_pin_file_tests {
    use super::*;

    /// Withdrawing keeps every other setting, and leaves other pins alone.
    #[test]
    fn it_removes_only_our_pin_and_keeps_the_rest() {
        let td = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(td.path());
        std::fs::create_dir_all(paths.claude_dir()).unwrap();
        let f = paths.claude_dir().join("settings.json");
        let write = |v: serde_json::Value| std::fs::write(&f, v.to_string()).unwrap();
        let read = || -> serde_json::Value {
            serde_json::from_str(&std::fs::read_to_string(&f).unwrap()).unwrap()
        };

        // Ours: withdrawn, and the neighbouring keys survive.
        write(serde_json::json!({
            "env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:8787", "KEEP": "1"},
            "model": "opus", "hooks": {}
        }));
        assert!(withdraw_pin(&paths, 8787));
        let v = read();
        assert!(v["env"]["ANTHROPIC_BASE_URL"].is_null(), "the pin is gone");
        assert_eq!(v["env"]["KEEP"], "1", "other env survives");
        assert_eq!(v["model"], "opus", "other settings survive");

        // Someone else's port: untouched.
        write(serde_json::json!({"env": {"ANTHROPIC_BASE_URL": "http://127.0.0.1:9001"}}));
        assert!(!withdraw_pin(&paths, 8787), "not ours to withdraw");
        assert_eq!(read()["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:9001");
    }
}

#[cfg(test)]
mod profile_read_tests {
    use super::*;

    /// Absent and unreadable are different. Only the first one means "empty".
    #[test]
    fn a_missing_profile_reads_as_empty() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("no-such-rc");
        assert_eq!(read_profile_for_edit(&p).unwrap(), "");
    }

    /// The whole point. `unwrap_or_default()` turned any read failure into an
    /// empty profile, and the very next line wrote that empty profile back with
    /// our two lines appended - replacing the user's PATH, aliases and version
    /// managers with a swapdex stanza. A directory stands in for every
    /// non-absence failure (permissions, I/O); it fails the same way for every
    /// user, root included.
    #[test]
    fn an_unreadable_profile_is_an_error_not_an_empty_one() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("rc-is-a-dir");
        std::fs::create_dir(&p).unwrap();
        let err = read_profile_for_edit(&p).unwrap_err().to_string();
        assert!(err.contains("refusing"), "unhelpful: {err}");
    }
}

#[cfg(test)]
mod per_tool_path_tests {
    use super::*;

    /// `paths.proxy_log` already names a file per tool. These two did not: the
    /// marker and the shim both fell through to Claude's name, so a gemini
    /// proxy would overwrite the marker Claude's own shim reads - pointing
    /// every Claude session at the Gemini proxy - and a gemini shim would
    /// overwrite the Claude shim binary.
    #[test]
    fn every_tool_gets_its_own_marker_and_shim() {
        let d = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(d.path());
        let tools = ["claude-code", "codex", "gemini", "antigravity"];
        let markers: Vec<_> = tools.iter().map(|t| proxy_marker_for(&paths, t)).collect();
        let shims: Vec<_> = tools.iter().map(|t| shim_path_for(&paths, t)).collect();
        let serving: Vec<_> = tools
            .iter()
            .map(|t| crate::proxy::serving_file_for(&paths, t))
            .collect();
        for i in 0..tools.len() {
            for j in (i + 1)..tools.len() {
                assert_ne!(
                    markers[i], markers[j],
                    "{} and {} share a proxy marker",
                    tools[i], tools[j]
                );
                assert_ne!(
                    shims[i], shims[j],
                    "{} and {} share a shim path",
                    tools[i], tools[j]
                );
                assert_ne!(
                    serving[i], serving[j],
                    "{} and {} share a serving file",
                    tools[i], tools[j]
                );
            }
        }
    }

    /// Claude's marker and shim keep the names they have, or an upgrade
    /// orphans a proxy that is already running and a shim already on PATH.
    #[test]
    fn the_established_names_do_not_move() {
        let d = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::rooted(d.path());
        assert!(proxy_marker_for(&paths, "claude-code").ends_with("proxy"));
        assert!(proxy_marker_for(&paths, "codex").ends_with("proxy-codex"));
        assert!(shim_path_for(&paths, "claude-code").ends_with("claude"));
        assert!(shim_path_for(&paths, "codex").ends_with("codex"));
    }
}
