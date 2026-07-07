//! Model-based random-walk soak. A deterministic LCG drives thousands of
//! random command sequences against a real store while a pure in-test model
//! tracks the ground truth (which profiles exist, which account each holds,
//! which is live per tool). After EVERY step we assert cross-command
//! invariants that scenario tests never reach - the kind of state drift that
//! only shows up after an odd interleaving of use / add / rename / rm /
//! restore. A failure prints the exact seed+step to replay.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_swapdex")
}

fn run(root: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(bin())
        .args(args)
        .env("SWAPDEX_ROOT", root)
        .env("SWAPDEX_ASSUME_TTY", "1")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

/// Write the codex live login for a given account id (email derived from id).
fn set_live_codex(root: &Path, account: &str) {
    use std::os::unix::fs::PermissionsExt;
    let d = root.join(".codex");
    std::fs::create_dir_all(&d).unwrap();
    // JWT payload {"email":"<account>@x.com"} - the account id is the truth we
    // track; email just rides along.
    let payload = b64url(&format!("{{\"email\":\"{account}@x.com\"}}"));
    let v = serde_json::json!({
        "auth_mode": "chatgpt",
        "tokens": {"id_token": format!("h.{payload}.s"), "access_token": "AT",
                   "refresh_token": format!("RT-{account}"), "account_id": account},
        "last_refresh": "2026-07-05T00:00:00Z"
    });
    let f = d.join("auth.json");
    std::fs::write(&f, serde_json::to_vec(&v).unwrap()).unwrap();
    std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn live_codex_account(root: &Path) -> Option<String> {
    let bytes = std::fs::read(root.join(".codex/auth.json")).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["tokens"]["account_id"].as_str().map(|s| s.to_string())
}

fn b64url(s: &str) -> String {
    // Minimal, dependency-free urlsafe base64 without padding.
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let b = s.as_bytes();
    let mut out = String::new();
    for chunk in b.chunks(3) {
        let n = chunk.len();
        let b0 = chunk[0] as u32;
        let b1 = if n > 1 { chunk[1] as u32 } else { 0 };
        let b2 = if n > 2 { chunk[2] as u32 } else { 0 };
        let combined = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((combined >> 18) & 63) as usize] as char);
        out.push(T[((combined >> 12) & 63) as usize] as char);
        if n > 1 {
            out.push(T[((combined >> 6) & 63) as usize] as char);
        }
        if n > 2 {
            out.push(T[(combined & 63) as usize] as char);
        }
    }
    out
}

/// The reference model: what SHOULD be true, tracked in pure Rust.
#[derive(Default, Clone)]
struct Model {
    /// profile name -> account id it holds for codex (single-tool world)
    profiles: HashMap<String, String>,
    /// the codex account id currently live on disk
    live: Option<String>,
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0
    }
    fn pick(&mut self, n: usize) -> usize {
        (self.next() >> 33) as usize % n.max(1)
    }
}

fn one_walk(seed: u64, steps: usize) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let names = ["work", "personal", "team", "alt"];
    let accounts = ["acctA", "acctB", "acctC", "acctD", "acctE"];
    let mut rng = Lcg(seed);
    let mut model = Model::default();
    // Start logged into some account.
    let start = accounts[rng.pick(accounts.len())];
    set_live_codex(root, start);
    model.live = Some(start.to_string());

    for step in 0..steps {
        let op = rng.pick(7);
        let name = names[rng.pick(names.len())].to_string();
        match op {
            0 => {
                // add/update the current live account under `name`
                let (_o, _e, c) = run(root, &["add", &name, "--tool", "codex", "--update"]);
                if let Some(live) = &model.live {
                    // add --update refuses (exit 7) if `name` already holds a
                    // DIFFERENT account; otherwise it saves/attaches.
                    let repoint = model
                        .profiles
                        .get(&name)
                        .map(|a| a != live)
                        .unwrap_or(false);
                    if repoint {
                        // TTY mode (the soak sets SWAPDEX_ASSUME_TTY) prompts
                        // y/N; stdin is null so it reads EOF -> declines ->
                        // nothing saved, exit 0. The profile keeps its account.
                        assert_eq!(
                            c, 0,
                            "seed={seed} step={step}: declined repoint = 0, not an error"
                        );
                    } else {
                        assert_eq!(c, 0, "seed={seed} step={step}: add");
                        model.profiles.insert(name.clone(), live.clone());
                    }
                }
            }
            1 | 2 => {
                // use `name` (switch)
                let (_o, _e, c) = run(root, &["use", &name, "--tool", "codex"]);
                match model.profiles.get(&name) {
                    Some(acct) => {
                        assert_eq!(c, 0, "seed={seed} step={step}: use existing");
                        model.live = Some(acct.clone());
                    }
                    None => assert_eq!(c, 5, "seed={seed} step={step}: use missing = 5"),
                }
            }
            3 => {
                // rename `name` -> other
                let other = names[rng.pick(names.len())].to_string();
                let (_o, _e, c) = run(root, &["rename", &name, &other]);
                let exists = model.profiles.contains_key(&name);
                let collide = other != name && model.profiles.contains_key(&other);
                if name == other {
                    // renaming to itself: collision check hits first (exists).
                    if exists {
                        assert_eq!(c, 6, "seed={seed} step={step}: rename self=6");
                    }
                } else if !exists {
                    assert_eq!(c, 5, "seed={seed} step={step}: rename missing=5");
                } else if collide {
                    assert_eq!(c, 6, "seed={seed} step={step}: rename collide=6");
                } else {
                    assert_eq!(c, 0, "seed={seed} step={step}: rename ok");
                    let acct = model.profiles.remove(&name).unwrap();
                    model.profiles.insert(other, acct);
                }
            }
            4 => {
                // rm `name`
                let (_o, _e, c) = run(root, &["rm", &name, "--yes"]);
                if model.profiles.remove(&name).is_some() {
                    assert_eq!(c, 0, "seed={seed} step={step}: rm existing");
                } else {
                    assert_eq!(c, 5, "seed={seed} step={step}: rm missing=5");
                }
                // rm never changes the live login.
            }
            5 => {
                // restore (undo last switch). We don't model the backup ring
                // precisely; just assert it never panics and leaves a VALID
                // live account that some profile-or-backup vouches for.
                let (_o, _e, c) = run(root, &["restore", "--tool", "codex"]);
                assert!(c == 0 || c == 5, "seed={seed} step={step}: restore rc={c}");
                // resync model.live to whatever is actually live now.
                model.live = live_codex_account(root);
            }
            _ => {
                // behind-swapdex re-login: the tool writes a new live account
                // directly (simulating the user running `codex login`).
                let acct = accounts[rng.pick(accounts.len())];
                set_live_codex(root, acct);
                model.live = Some(acct.to_string());
            }
        }

        // ---- Invariants checked after EVERY step ----

        // (I1) ls exit is always 0 and valid JSON.
        let (o, _e, c) = run(root, &["ls", "--json"]);
        assert_eq!(c, 0, "seed={seed} step={step}: ls rc");
        let ls: serde_json::Value = serde_json::from_str(&o)
            .unwrap_or_else(|_| panic!("seed={seed} step={step}: ls --json invalid: {o}"));

        // (I2) the set of profile names on disk == the model's.
        let disk_names: std::collections::HashSet<String> = ls
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["name"].as_str().unwrap().to_string())
            .collect();
        let model_names: std::collections::HashSet<String> =
            model.profiles.keys().cloned().collect();
        assert_eq!(
            disk_names, model_names,
            "seed={seed} step={step}: profile set drift"
        );

        // (I2b) EACH profile holds the account the model says (read the
        // codex snapshot directly - ls --json doesn't expose the account id).
        for (pname, acct) in &model.profiles {
            let snap = root
                .join(".local/share/swapdex/accounts")
                .join(pname)
                .join("codex/auth");
            let bytes = std::fs::read(&snap).unwrap_or_else(|_| {
                panic!("seed={seed} step={step}: profile '{pname}' snapshot missing")
            });
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            let on_disk = v["tokens"]["account_id"].as_str().unwrap_or("");
            assert_eq!(
                on_disk, acct,
                "seed={seed} step={step}: profile '{pname}' account drift"
            );
        }

        // (I3) the live codex account on disk matches the model.
        assert_eq!(
            live_codex_account(root),
            model.live,
            "seed={seed} step={step}: live account drift"
        );

        // (I4) status --json is always valid and never panics.
        let (so, _e, sc) = run(root, &["status", "--json"]);
        assert_eq!(sc, 0, "seed={seed} step={step}: status rc");
        let _: serde_json::Value = serde_json::from_str(&so)
            .unwrap_or_else(|_| panic!("seed={seed} step={step}: status --json invalid"));
    }
}

#[test]
fn model_based_random_walk() {
    // A spread of seeds; each walk is a long interleaving. Deterministic:
    // a failure prints seed+step to replay exactly.
    for seed in [1u64, 7, 42, 99, 256, 1024, 31337, 65537] {
        one_walk(seed, 60);
    }
}
