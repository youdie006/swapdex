//! The `claude` shim: a tiny launcher placed on the user's PATH (ahead of the
//! real `claude`) that reads swapdex's default-account pointer and runs the real
//! `claude` in that account's slot. This is what makes a plain `claude` follow
//! `swapdex use`. No credential is ever moved - the shim only sets
//! `CLAUDE_CONFIG_DIR`.

use crate::paths::Paths;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

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
        _ => "claude",
    };
    paths.store_dir().join("bin").join(bin)
}

/// Single-quote a path for safe embedding in the /bin/sh shim script.
fn sh_quote(p: &Path) -> String {
    let s = p.to_string_lossy();
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The shim script body. The default-account pointer only fills in when nothing
/// has already chosen a config dir: an explicit `CLAUDE_CONFIG_DIR` (what
/// `swapdex run <account>` sets, or what a user exports by hand) is a decision
/// already made, and overriding it meant every account opened as the default one.
///
/// It also gets proxy mode for free: the shim asks `swapdex proxy --ensure`,
/// which prints the port of a running proxy and starts one in the background if
/// there is none. So mid-session account switching works without the user
/// launching or exporting anything, and a proxy that cannot start is not an
/// error - the shim just runs Claude directly.
pub fn shim_script(pointer: &Path, real_claude: &Path, swapdex: &Path) -> String {
    format!(
        "#!/bin/sh\n\
         # swapdex claude shim - launch claude in the default account's slot.\n\
         # Managed by swapdex; re-created by `swapdex shim`.\n\
         # Signing in must reach Anthropic directly. The OAuth exchange is between\n\
         # the browser and the real API, and a proxy in the middle both breaks the\n\
         # code exchange and answers with whichever account it already has - so a\n\
         # fresh slot looks signed in as somebody else, or the prompt takes no\n\
         # input at all.\n\
         sx_login=no\n\
         for a in \"$@\"; do\n\
         \tcase \"$a\" in login|/login|logout|/logout|setup-token) sx_login=yes ;; esac\n\
         done\n\
         # Ask swapdex for a live proxy (it starts one if needed and prints the\n\
         # port); silence and a non-zero status mean \"run without one\".\n\
         if [ \"$sx_login\" = no ]; then\n\
         \tport=$({sx} proxy --ensure 2>/dev/null)\n\
         \tif [ -n \"$port\" ]; then\n\
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
         exec {real} \"$@\"\n",
        sx = sh_quote(swapdex),
        ptr = sh_quote(pointer),
        real = sh_quote(real_claude),
    )
}

/// The `codex` shim. Same shape as Claude's - fill the tool's home from the
/// default pointer, and only when nothing has already chosen one - but with
/// Codex's own variable and pointer. It never mentions Claude's: one tool's shim
/// moving the other tool's account is exactly what the per-tool split prevents.
pub fn codex_shim_script(pointer: &Path, real_codex: &Path, swapdex: &Path) -> String {
    // The provider name is the one identity Codex prints on /status, and with the
    // proxy rewriting the bearer, the login inside CODEX_HOME is not the account
    // being charged. Naming the payer there is the difference between a screen
    // that says who pays and a screen that says nothing.
    //
    // The provider block deliberately carries no `env_key`: that omission is what
    // makes Codex attach its OWN ChatGPT OAuth bearer and account-id, which is
    // the pair the proxy rewrites. Naming a key instead would have it send an API
    // key and there would be nothing to switch.
    format!(
        "#!/bin/sh\n\
         # swapdex codex shim - launch codex in the default account's slot.\n\
         # Managed by swapdex; re-created by `swapdex shim`.\n\
         # The provider overrides belong on a run that TALKS to the model. On\n\
         # `resume` they emptied the session picker: Codex lists the sessions that\n\
         # match the configured provider, and a conversation held long before\n\
         # swapdex existed matches none. A sign-in is excluded for its own reason -\n\
         # the OAuth exchange is between the browser and the real backend, and a\n\
         # proxy in the middle answers with whichever account it already holds.\n\
         sx_plain=no\n\
         for a in \"$@\"; do\n\
         \tcase \"$a\" in login|/login|logout|/logout|resume|/resume|history|sessions) sx_plain=yes ;; esac\n\
         done\n\
         # Ask swapdex for a live proxy (it starts one if needed and prints the\n\
         # port); silence means \"run without one\", exactly as before.\n\
         if [ \"$sx_plain\" = no ]; then\n\
         \tport=$({sx} proxy --ensure --tool codex 2>/dev/null)\n\
         \t# Who pays. Codex prints the provider name on /status and nothing\n\
         \t# else about identity, so the account goes in the one field it shows.\n\
         \tsx_who=$({sx} serve --tool codex --quiet 2>/dev/null)\n\
         fi\n\
         if [ -n \"$port\" ]; then\n\
         \tsx_name=swapdex\n\
         \tif [ -n \"$sx_who\" ]; then\n\
         \t\tsx_name=\"swapdex: $sx_who\"\n\
         \tfi\n\
         \tset -- -c model_provider=swapdex \\\n\
         \t\t-c model_providers.swapdex.name=\"$sx_name\" \\\n\
         \t\t-c model_providers.swapdex.base_url=\"http://127.0.0.1:$port/v1\" \\\n\
         \t\t-c model_providers.swapdex.wire_api=responses \"$@\"\n\
         fi\n\
         if [ -z \"$CODEX_HOME\" ]; then\n\
         \tdir=$(cat {ptr} 2>/dev/null)\n\
         \tif [ -n \"$dir\" ]; then\n\
         \t\tCODEX_HOME=\"$dir\"\n\
         \t\texport CODEX_HOME\n\
         \tfi\n\
         fi\n\
         exec {real} \"$@\"\n",
        sx = sh_quote(swapdex),
        ptr = sh_quote(pointer),
        real = sh_quote(real_codex),
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
    for dir in std::env::split_paths(&path) {
        let cand = dir.join("claude");
        if cand.is_file() {
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
    for dir in std::env::split_paths(&path) {
        if dir == shim_dir {
            continue;
        }
        let cand = dir.join(bin);
        if cand.is_file() && !is_our_shim(&cand) {
            return Some(cand);
        }
    }
    None
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
    if active {
        return ShimReach::Active;
    }
    // Scoped to THIS shim directory on purpose. Matching swapdex's marker
    // comment alone would let a profile that set up some other store excuse a
    // real finding here - and on a machine where swapdex was ever installed,
    // that marker is always present.
    match profile_text {
        Some(t) if profile_already_adds(t, shim_dir) => ShimReach::ConfiguredElsewhere,
        _ => ShimReach::Missing,
    }
}

/// The shell profile's text, if there is one to read.
pub fn shell_profile_text() -> Option<(PathBuf, String)> {
    let p = shell_profile()?;
    let t = std::fs::read_to_string(&p).ok()?;
    Some((p, t))
}

fn profile_already_adds(profile_text: &str, shim_dir: &Path) -> bool {
    let full = shim_dir.to_string_lossy().to_string();
    let home = dirs::home_dir().map(|h| h.to_string_lossy().to_string());
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
fn shell_profile() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
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

/// Put the shim dir on PATH by editing the user's shell profile, because leaving
/// this to the user means the shim silently does nothing: it is installed, PATH
/// never reaches it, and `swapdex use` appears to work while changing nothing.
/// Idempotent - a profile that already carries the marker is left alone.
pub fn ensure_on_path(shim_dir: &Path) -> Result<PathSetup> {
    if already_on_path(shim_dir) {
        return Ok(PathSetup::AlreadyThere);
    }
    let Some(profile) = shell_profile() else {
        return Ok(PathSetup::Manual);
    };
    let existing = std::fs::read_to_string(&profile).unwrap_or_default();
    let line = path_line(shim_dir);
    if existing.contains(PROFILE_MARKER) || profile_already_adds(&existing, shim_dir) {
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

/// Install (or refresh) the shim. Returns (shim_path, shim_dir) so the caller
/// can print PATH guidance.
pub fn install(paths: &Paths) -> Result<(PathBuf, PathBuf)> {
    let shim = shim_path(paths);
    let shim_dir = shim
        .parent()
        .map(|p| p.to_path_buf())
        .context("shim path has no parent")?;
    let real = find_real_claude(&shim_dir)
        .context("could not find the real `claude` on PATH - install it first")?;
    let pointer = paths.store_dir().join("active-claude");
    // The shim calls back into THIS binary, by absolute path: whatever swapdex
    // installed the shim is the one that will start its proxy, even if PATH
    // later changes or a different build lands ahead of it.
    let me = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("swapdex"));
    std::fs::create_dir_all(&shim_dir).context("create shim dir")?;
    std::fs::write(&shim, shim_script(&pointer, &real, &me)).context("write shim")?;
    make_executable(&shim)?;
    Ok((shim, shim_dir))
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
        let home = dirs::home_dir().expect("a home dir");
        let shim_dir = home.join("Library/Application Support/swapdex/bin");
        let full = shim_dir.display().to_string();
        for spelling in [
            format!("export PATH=\"{full}:$PATH\""),
            "export PATH=\"$HOME/Library/Application Support/swapdex/bin:$PATH\"".to_string(),
            "export PATH=\"~/Library/Application Support/swapdex/bin:$PATH\"".to_string(),
        ] {
            assert!(
                profile_already_adds(&format!("# something\n{spelling}\n"), &shim_dir),
                "not recognised: {spelling}"
            );
        }
        // A profile that does NOT add it is left alone, and a commented-out line
        // is not an active entry.
        assert!(!profile_already_adds(
            "export PATH=\"/usr/local/bin:$PATH\"\n",
            &shim_dir
        ));
        assert!(!profile_already_adds(
            &format!("# export PATH=\"{full}:$PATH\"\n"),
            &shim_dir
        ));
        // A line merely MENTIONING the dir without touching PATH is not one.
        assert!(!profile_already_adds(&format!("echo {full}\n"), &shim_dir));
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
            s.contains("sx_login=no"),
            "it decides whether this is a sign-in: {s}"
        );
        for verb in ["login", "/login", "logout", "setup-token"] {
            assert!(s.contains(verb), "recognised: {verb}");
        }
        // And the base-url export sits INSIDE that condition, not before it.
        let guard = s.find("if [ \"$sx_login\" = no ]").expect("the guard");
        let export = s.find("ANTHROPIC_BASE_URL").expect("the export");
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
        assert!(s.contains("model_provider=swapdex"), "selects the provider");
        assert!(
            s.contains("model_providers.swapdex.base_url=\"http://127.0.0.1:$port/v1\""),
            "points it at the proxy: {s}"
        );
        assert!(
            s.contains("model_providers.swapdex.wire_api=responses"),
            "the protocol codex speaks"
        );
        assert!(
            !s.contains("env_key"),
            "declaring an api key would stop codex attaching its own OAuth"
        );
        // Without a proxy, codex runs exactly as it would have.
        assert!(
            s.contains("if [ -n \"$port\" ]"),
            "the overrides are conditional: {s}"
        );
    }

    // Codex lists the sessions matching its configured provider, so the provider
    // overrides emptied the resume picker - a machine with 158 conversations for
    // the current directory showed "No sessions yet", which reads as the history
    // being gone.
    #[test]
    fn the_codex_shim_leaves_reading_commands_alone() {
        let s = codex_shim_script(
            Path::new("/store/active-codex"),
            Path::new("/usr/bin/codex"),
            Path::new("/bin/swapdex"),
        );
        for verb in ["resume", "history", "sessions"] {
            assert!(s.contains(verb), "recognised as a plain run: {verb}");
        }
        // Those runs ask for no proxy, so no provider is set on them.
        let guard = s.find("if [ \"$sx_plain\" = no ]").expect("the guard");
        let ask = s.find("proxy --ensure").expect("the ask");
        assert!(guard < ask, "the proxy is only asked for on a talking run");
        // The home still comes from the pointer, whatever the command is: that is
        // what decides which conversations exist at all.
        let home = s.find("CODEX_HOME=").expect("home");
        assert!(home > guard, "the home is set outside the guard: {s}");
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
    fn script_gets_its_proxy_from_swapdex_and_tolerates_none() {
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
            s.contains("2>/dev/null") && s.contains("if [ -n \"$port\" ]"),
            "no proxy is not an error - claude still runs: {s}"
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
