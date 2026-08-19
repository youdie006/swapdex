# Changelog

All notable changes to swapdex are documented here. This project follows
[Semantic Versioning](https://semver.org) and
[Keep a Changelog](https://keepachangelog.com).

## [0.71.0] - 2026-08-19

### Fixed
- **Renaming a Codex account actually renames it.** `rename` looked the account up in Claude'"'"'s registry only, so a Codex slot was never found: the saved snapshot took the new name and the slot kept the old one. The machine then had one account answering to two - `swapdex ls` saying `codex` while the registry still said `A` - and anything reading the registry went on using a name the user had already changed. It searches every tool now. `rm` had been fixed for the same reason and `rename` had not.

## [0.70.0] - 2026-08-19

### Fixed
- **A window the server never reports is no longer called unused.** Codex publishes only a weekly window - `secondary_window` comes back null every time - and the empty session column said `not started`, which is a CLAIM: the window exists and you have not touched it. True for Claude, whose 5h window begins on first use; false for Codex, where it read as room beside an account the owner had just watched run out in Codex itself. It says `not reported` there now. swapdex still cannot see that limit; it no longer pretends the silence means anything good.
- **Codex accounts kept as snapshots are read too.** `swapdex quota` walked slots only, so a machine whose accounts are saved snapshots got no Codex section at all - not a number, not a heading, no reason. Found on one where `swapdex ls` listed three Codex accounts and `quota` said nothing about any of them. Each account is now read from wherever it lives, and one that cannot be read says why.

## [0.69.0] - 2026-08-19

### Fixed
- **The dashboard marks the account that is actually paying, not the one you asked for.** Pressing Enter records a choice; whether it takes is a separate question, and with auto on the proxy sends each turn to whichever account can serve. On a real machine `rnd` was chosen at 13:45 and every turn afterwards went to `bsgong` - because rnd refuses on overage despite 95% of its windows being free - while the row said `rnd active 95% left` for half an hour. The ask still wins at first, so the key feels immediate instead of lagging a turn; but once the proxy has served someone ELSE since the ask, the ask demonstrably did not take and the mark follows what is happening. Decided from the two records timestamps: the ask is written when you pick, the proxy writes when it forwards.
- **And it says why.** The chosen account row now carries `asked for rnd - it cannot serve, so turns are going to bsgong`. Resolving the disagreement silently would leave you looking at an account you did not pick with no idea how it got there.

## [0.68.0] - 2026-08-19

### Removed
- **`swapdex continue`**, added earlier the same day and made pointless hours later by sharing the history. It carried one conversation at a time into another account'"'"'s store; with every account reading one store there is nothing to carry, and it fails outright on the setup swapdex now creates - `already migrated: ... already exists`. It was the symptom being treated one conversation at a time. Switching accounts is `swapdex use`, and every conversation is already visible from wherever you land.

### Fixed
- **A switch no longer warns that conversations will go out of view.** That warning was true and useful when each account kept its own history; it is false now, and a warning that has stopped being true is worse than none, because it teaches a rule the tool no longer follows. An account that has NOT been repaired yet still says so, and names `share-history` as the fix - silence there would be the same failure in reverse.

## [0.67.1] - 2026-08-19

### Fixed
- **`share-history --dry-run` no longer does the thing it is previewing.** The count of conversations to carry was taken by actually copying them, so the preview said "4 conversations" and the real run that followed said "0" - the first had already done the work. A preview that changes what it previews is worse than none: its answer is only true the first time it is asked. It counts and writes nothing now, and gives the same answer however often it runs. Nothing was lost when this fired on a real machine - the files were ones the merge would have copied anyway, and it never overwrites or deletes - but the command did not do what it said.

## [0.67.0] - 2026-08-19

### Fixed
- **Every conversation is reachable from every account.** A transcript carries no account or organisation identifier anywhere in it - so it belongs to the person, not to whichever account happened to pay for those turns. Slots kept their own `projects/` (Codex: `sessions/`), so `swapdex use B` made every conversation started on A vanish from `claude --resume`: not lost, but invisible, which for a resume list is the same thing. That defeats the point of the tool, where switching accounts is meant to change who PAYS and nothing else. Transcripts are shared across slots now, alongside settings. The credential and the file naming the account stay per-slot, as they must.
- **`swapdex share-history`** repairs accounts made before this. It carries over the conversations a slot held alone, then points it at the shared store. Nothing is deleted: an entry already shared is never overwritten, the copies are copies rather than moves so a half-finished merge leaves every original readable, and the slot own directory is renamed aside rather than removed. `--dry-run` says what it would touch.

## [0.66.0] - 2026-08-19

### Added
- **`swapdex continue` - hand the conversation you were in to an account that still has room.** The proxy already moves a RUNNING session between accounts; this is the other case, where the turn is over and you are out. It picks the account with the most left (never the one you are on - a handoff to yourself changes nothing), carries the conversation into that account's own store, switches to it, and prints the resume command. Under the slot model each account has its own `CLAUDE_CONFIG_DIR` with its own `projects/`, so continuing elsewhere means crossing that boundary; sessionwiki does the carrying, which is why this orchestrates rather than implements. `--account` picks the target by hand, `--dry-run` says what would happen and changes nothing. Nothing is silent: a step that cannot run says so and leaves the account as it was, rather than letting the next step look like it worked. Needs sessionwiki 0.23+ (`migrate --config-dir`), and says so if it is missing. Accounts with no usage reading are not offered - absence is not an offer.

## [0.65.5] - 2026-08-18

### Fixed
- **A 429 now counts as a refusal.** The refusing list was assembled from two witnesses that both miss it: the `anthropic-ratelimit-unified-*-status: rejected` header, which a 429 from this API does not carry, and the sideline set, which holds only 401 and 403. So the commonest refusal there is went unrecorded and its account kept reading as fine. A third witness reads the stamps - when this account last refused, and whether anything has gone through since. It lapses like the window it came from, so one 429 cannot mark an account for the life of the proxy, and a turn that succeeds clears it at once.
- **A rewritten request that the server calls malformed is retried as the client wrote it.** swapdex changes a request body in two places - it aligns the account identity, and past the wall it may ask for a fallback model instead of the one requested. Both are guesses about what the server will accept, and when the answer is 400 or 422 the rewrite is the first suspect. The client's own body is known-good by construction, being what would have been sent with no proxy at all, so one try is spent on it before handing back a failure. Once only, and only for a refusal about the REQUEST: a 429 is about quota and a 5xx is the server's own trouble, and re-sending either would change nothing while hiding what happened.

### Added
- **A remembered Codex reading carries why the account is refusing.** Codex states `rate_limit_reached_type` on the same response as its windows and it was being parsed and dropped, leaving a row able to report a refusal but not what would clear it - the whole value of the field. It is stored with the numbers now, in the account's own words including who can act on it, and survives a restart along with them.

## [0.65.4] - 2026-08-18

### Fixed
- **"(on credits)" is only said when credits are actually carrying the account.** The usage endpoint reports that extra usage is ENABLED - a setting - and that was being printed as though it described what the account was doing. So an account serving normally with 88% of both windows left read `5h 88% left · 7d 86% left (on credits)`, which suggests either an account in trouble or a bill being run up, when neither was happening. Credits become load-bearing only once a window is spent; until then the account is on its plan and they have not been touched. The dashboard already worked this way - this brings the proxy's usage line in line with it.

## [0.65.3] - 2026-08-18

### Fixed
- **"(on credits)" stops flickering back onto an account that is refusing.** A refusal record lapses on purpose - a rate limit is a window, not a verdict - but the label was coming back with it. On a real machine one account read `refusing turns`, then `(on credits)`, then `refusing turns` again within a few minutes, and in the middle of that it offered a way through that had already been tried and refused. The label now waits for a turn that actually goes through: the proxy remembers when each account last refused and when one last succeeded, and only a success since the last refusal restores the promise. The credits reading comes from the usage endpoint, which describes a setting; the refusals come from turns that were really attempted. When they disagree the turns win.
- **A refusal counts even when it names no window.** The refusing list was built only from `anthropic-ratelimit-unified-*-status: rejected`, and a 429 from this API carries none of those headers at all - so most refusals were never recorded as such. Accounts held out of rotation after a refusal are counted too now. Where both sources know, the one that names the window wins, since "refusing turns (overage)" and a bare "refusing turns" send the reader to different places.

## [0.65.2] - 2026-08-18

### Fixed
- **"Every account is past the threshold" is no longer said about accounts that have plenty left.** When nothing else can take a turn, the rotation filter has rejected each candidate for one of six reasons - disabled, sidelined by a refusal, out of quota, past the threshold, no readable token, or too close to be worth the move - and all six were reported as the threshold. On a real machine that printed `every account is past the threshold` while one account sat at **97% left**, refusing because its organisation's usage credits were at zero. That sends you to a quota page where nothing is wrong. The reason is now taken from the measurements: only when every other measured account really is above the line does it say so; otherwise it says they are refusing. Accounts that could not be measured are left out of the judgement rather than counted as spent - not knowing is not evidence.

## [0.65.1] - 2026-08-18

### Fixed
- **Streaming answers no longer break partway with "Connection lost mid-response".** The HTTP client this proxy uses negotiates gzip and DECODES the body on the way in, so what reaches the client is plain bytes - but the upstream's `content-encoding: gzip` was being echoed along with them. The client then tried to gunzip text that was already text, and the stream died mid-answer. Claude Code reported it as `API Error: Connection lost mid-response. The response above may be incomplete.`; the proxy log recorded the same event as `gzip decompression failed`. It struck only when upstream chose to compress a response, which is why it came and went. The label is dropped now, along with `content-length`, since both describe bytes that no longer exist by the time the client sees them. Filtered separately from the upward direction, where a client may legitimately send an encoded request body that this proxy passes through untouched.

## [0.65.0] - 2026-08-18

Codex does send its quota on the response headers. Measured through the proxy
on a real account, and the verbatim headers are now the fixtures these tests
run against.

### Fixed
- **A window of zero minutes is not a window.** Codex sends the whole `secondary` set zeroed on an account that has no session window - `x-codex-secondary-window-minutes: 0`, `-reset-after-seconds: 0`, `-reset-at:` empty - and 0.64.0 read that as a real window at 0% used. It went to disk as `five_h: 0.0` and would have drawn a 5h gauge reading `100% left` for something that does not exist, the moment the endpoint was throttled or the machine offline. Guarded on both paths, header and endpoint, so the phantom cannot return through the other one.
- **A spent Codex account is still remembered.** The cache discards readings pinned at the ceiling, a rule that exists for one Claude-era bug where `utilization` was misread as a fraction and every account above 1% clamped to exactly 100. Codex never had that bug, and the rule was throwing away the reading you most need - a spent account's - leaving its row blank rather than saying it is out. Scoped to Claude, where the misread happened.
- **Credits seen on a response are remembered with the numbers.** Without it a full window flipped the row back to "spent" between live reads, on an account that was still answering turns because its credits carried it.

- **A refusal says why.** A non-2xx reached the log as three digits and nothing else, so a 400 the user saw as "API error" had no explanation anywhere on the machine - and 25 of them were sitting in a real proxy log with no way to tell what the API had objected to. The API always sends a reason; it was simply never read. Only ERROR bodies are read, and they are handed to the client untouched: the body is buffered and the reader replaced with one over the same bytes, so nothing is taken away from the client to gain the log line. A success is never buffered, since that would hold a whole streamed conversation in memory to report nothing.

### Added
- **The response carries more than its windows, and all of it is read now.** `x-codex-plan-type`, the credit flags and balance (`x-codex-credits-has-credits` / `-balance` / `-unlimited`, written capitalised the way Python prints a bool), and **the per-model limits** under their own ids - `x-codex-bengalfox-primary-used-percent` beside the plan's `x-codex-primary-used-percent`. That last shape was documented in 0.64.0 as one this code had never seen and would not guess at; it has now been seen, so it is parsed. Ids are discovered rather than listed, matched on the `-used-percent` suffix so `x-codex-primary-over-secondary-limit-percent` - which also ends in `percent` and shares the prefix - cannot invent a limit on every response.

## [0.64.0] - 2026-08-18

### Added
- **A Codex account is asked for its own usage instead of being inferred from.** Until now a Codex account's numbers came only from the `rate_limits` blocks Codex happens to write into its transcripts, which meant a home with no transcripts had no row at all - a saved account that has not been driven through this machine was a permanent blank, and one on this machine sat at 16% remaining without ever saying so. `chatgpt.com/backend-api/wham/usage`, asked with the account's own token, answers per CREDENTIAL and names itself: `email`, `plan_type`, the windows, the per-model limits, the credit balance, and `rate_limit_reached_type` when it is refusing. So attribution stops being an inference. The live answer wins where there is one; the transcript still answers when the endpoint is throttled or the machine is offline, and a throttled endpoint falls back rather than blanking the row - it says nothing about the account behind it. Same discipline as `swapdex quota`: read-only, curl with its config on stdin so the token never reaches `ps`, an honest User-Agent, and no HTTP client added to the dependency graph. The `chatgpt-account-id` header is omitted when the saved id is one of Codex's `email_`/`local_` placeholders, which the endpoint rejects outright. Found by reading icoretech/codex-pooler, which uses the same endpoint. Two sibling paths, `/backend-api/codex/usage` and `/api/codex/usage`, answer 403 and are not used.
- The response also settles a question that was open: it reports `secondary_window` as null, so Codex genuinely publishes only a weekly window today. The 5h gauge is empty because there is nothing to put in it, not because swapdex fails to look.
- **The proxy reads a Codex response's own quota headers.** `x-codex-primary-used-percent` / `-window-minutes` / `-reset-at`, the `secondary` set, and `x-codex-rate-limit-reached-type`, taken off responses the proxy is already carrying. This costs nothing - no extra request - and the reading belongs to the account that SERVED the turn, so unlike a transcript there is nothing to attribute. It exists because the other two sources fail in different places: the endpoint needs the network and can be throttled, and a transcript is bound to a home rather than a credential. Whether Codex actually sends these headers is undocumented and had never been checked here - swapdex only ever looked for `anthropic-ratelimit-unified-*` - so the first one to arrive says so in the proxy log, once, and their absence stays visible as that line never appearing. A response carrying none of them leaves the remembered reading alone rather than overwriting it with a blank, and a percentage arriving without its window length is discarded rather than guessed into a column. Only the plan windows are read; per-model limits are documented as arriving under their own `x-<limit-id>-*` names, but none has been seen here and a parser for an unobserved shape is a guess.
- Between the two local sources - what the proxy saw and what a transcript says - the NEWER one answers, rather than a fixed rank. Both are honest; the stale one is simply older. A live endpoint answer still outranks both, being taken just now.

- **Everything the endpoint says is now shown, not just the windows.** The identity, plan, credit balance, per-model limits and refusal reason were parsed and tested while nothing displayed them. A Codex row now carries the plan it never had (`polarisairnd@gmail.com [pro]`), and when the account keeps serving on credits its full window stops reading as the end of it. A refusal is named in the account's own terms, including who can clear it - `workspace credits spent - its owner has to top them up` sends you somewhere different from `out of quota`, and a reason nobody has words for is shown verbatim rather than swallowed. `swapdex quota` grows a Codex section for what a one-line row cannot hold: a bar per window, the per-model limits under the names the endpoint gives them, the credit balance, and the refusal. Labels there are padded to the widest of the account's own, since a per-model name runs far past the width `7d` needs and unpadded bars step sideways down the block.
- **A row says so when the server disagrees with it about whose account it is.** A Codex row is labelled from the home's saved `id_token`, which cannot know the server thinks otherwise - and that gap is the shape of the mix-up where signing in as one account leaves another connected. The live answer wins and the disagreement is stated beside it: `actually-c@example.com [business] (saved as saved-b@example.com)`. It goes in the identity column rather than the status word, because a home that disagrees still serves turns perfectly well - the doubt is about the name, not the account.

### Fixed
- `swapdex quota` on a machine with no Claude accounts printed a note about Claude and stopped, showing nothing about the Codex accounts it did have.

### Changed
- Remembered readings are kept per tool (`codex-quota-cache.json` beside the existing `quota-cache.json`). Slot names are unique only within a tool, so one flat cache would let a Codex `work` overwrite a Claude `work` and show one account's windows under the other's name. Claude's file keeps its name, so an upgrading install loses no history.

### Fixed
- **A Codex usage reading is credited to the account it was read from.** Codex writes its rate limits into the transcripts under a home directory, and swapdex was captioning each reading with whoever the switch timeline said was PAYING at the moment it was written. Those are different questions, and on a machine where one account serves while another's home holds the sessions they gave different answers: an account whose home holds no transcripts at all was shown carrying real numbers, while the home holding every one of them showed none. A reading found in a home belongs to that home. It stays the fallback source now that the account can be asked directly, and the rule still holds for it. What upstream will not do is name the account in the transcript; openai/codex#16323 asked for that and was declined.

## [0.63.0] - 2026-08-14

### Fixed
- **An account that is refusing is not labelled as running on credits.** The usage endpoint answers that question from `extra_usage.is_enabled` and `spend_limit_reached` and carries no BALANCE, so an organisation whose pre-purchased credits have run to zero still reports extra usage enabled and under its cap. A real account printed `(on credits)` on the same line as `refusing turns (overage)` - one half promising a way through, the other reporting that there is none. An observed refusal wins: the endpoint describes a setting, the refusal is what happened to a request.

## [0.62.0] - 2026-08-14

### Fixed
- **A refusing account says which window closed.** The response names it, and the request log printed it, but the account report - the place you look to understand your accounts - said only "refusing turns". Beside "96% left" that reads as a contradiction, and it sends you to the quota page when the block is not about quota: `rnd 5h 96% left, 7d 90% left - refusing turns (overage)`. One word separates "you are out of quota" from "something else is stopping this account", and they are fixed in different places.

## [0.61.0] - 2026-08-14

### Fixed
- **A dead end says which kind it is.** Two situations reach it and they are not the same news: every account past its threshold, or every account refusing this turn whatever their windows say. Only the first had words, so three accounts sitting at 98-100% left - all refusing because an organisation's overage budget was spent - were reported as "past the threshold", with the three lines contradicting it printed directly above. It is also said once per episode now rather than on every turn.

### Changed
- **Four helpers that nothing called were removed.** Each had been superseded by something that answers the same question better, and each still carried passing tests - `codex_limits::latest` (a fixed `~/.codex` where `for_slot` takes the slot), `refresh::needs_refresh` (five minutes of slack where the serving path already asks through `slot_token_expired` and `has_usable_login`), `pick::rotate_target` (rejection only, where `next_usable` also weighs identity, login and disabled), and `quota_cache::age_secs`. Ten green tests between them, none exercising anything the product does; the contracts worth keeping were re-pointed at the live functions rather than deleted with the dead ones.

## [0.60.0] - 2026-08-13

### Fixed
- **Codex's `/status` says which account is paying.** With a custom `model_provider` configured Codex drops its account section entirely, so running through the proxy costs you the only place that answered "which account am I on". The shim already put the payer in the provider name - the one field Codex still prints - but it put the SLOT NAME: `swapdex: work`. A slot name is a label its owner chose. It carries the account now: `swapdex: work (polarisairnd@gmail.com)`.
- **And where the session actually lives, when that is somewhere else.** swapdex keeps two pointers on purpose - `serve` decides who pays, `use` decides where new sessions live - and with one field to show, a session billed to `work` while its history piled up in `codex-main` read as though it were running as `work`. The home is named only when the two differ: `swapdex: work (polarisairnd@gmail.com) - home: codex-main`.
- **Reset times always show minutes.** Dropping them on the hour reads the way a person speaks - `3pm` - but these sit in a column, and a three-character time beside a six-character one leaves a hole that reads as a broken layout.

## [0.59.0] - 2026-08-13

### Fixed
- **A window with no reset yet keeps its column.** The five-hour window starts on first use, so an account that has not touched it has no reset time to show - and rendering that as nothing pulled everything after it left on that row alone, putting its 7d block several columns ahead of the rows around it. The slot now holds its width across the whole frame and says `not started`, so the reason for the blank is on screen instead of being left to the reader.
- **A benched account is announced once, not every turn.** A `serve` pointer naming an account that has been benched redirects every turn to the same place, and the line saying so printed on every one of them until nobody read any of them. It is said once per episode now - again when the destination changes, or after a turn goes through without redirection, because a bench that came and went is news the second time too. The opposite mistake was the previous one: that path used to say nothing at all.
- **The reset time no longer reads as part of the gauge.** It sits outside the bar, but only one space away, and the gauge ends in a dark track cell - so the time butted against it and read as though it were inside, which is the confusion moving it out was meant to end. Two spaces now separate them.

## [0.58.0] - 2026-08-12

### Fixed
- **A row no longer says "no login" beside its own live usage figures.** The dashboard asked the question two different ways: slot rows through the reader that knows a Keychain which will not open is not an account nobody signed into, profile rows through the wrapper that throws that distinction away. An account with both a profile and a slot of the same name keeps only the profile row, so the lossy answer was the only one shown - `rnd` read as signed out while reporting 100% and 94% left. Both now ask through one helper, and the distinction itself is pinned by a test that can run off a Mac.

## [0.57.0] - 2026-08-12

### Fixed
- **The proxy's usage line says what is LEFT, like everything else.** It printed the percentage USED, with no word - so `5h 0%` meant a full window and read as an empty one, while the dashboard gauge beside it said `62% left` and `swapdex quota` said `39% left`. One tool cannot hold two conventions and expect either to be trusted. Every window now reads `5h 100% left, resets 1:47pm`. The conversion happens at the edge, so every decision inside still reasons about usage.

## [0.56.0] - 2026-08-12

### Fixed
- **A restart no longer asks about every account at once.** The proxy started with nothing remembered, so every account was due immediately however recently it had been read - and several arriving together is exactly the burst the usage endpoint throttles. Three service restarts in an afternoon put every account on a real machine into "usage endpoint throttled" simultaneously, which also means the threshold cannot apply and preemptive rotation stops. It now starts from the readings on disk, whose recorded age survives the restart: an account read moments ago is not asked again, one read an hour ago is. Windows that have since turned over were already dropped on load, so nothing stale is carried forward as current. Each round writes back what it actually read, leaving carried-over values with their original age rather than restamping them as fresh.

## [0.55.0] - 2026-08-12

### Changed
- **The dashboard gauge holds one reading; the reset time sits beside it.** Crammed inside, `62% left 6d` read as two quantities of quota, and it forced the bar wide enough to hold a sentence. Outside, there is room: `5h [ 62% left ] resets 1:47pm   7d [ 27% left ] resets Tue 9am`. The bar is back to its natural width, the two numbers can no longer be mistaken for each other, and a narrow terminal drops the word, then the time, while the readings always survive.

## [0.54.0] - 2026-08-12

### Changed
- **The proxy reports each window separately, with its own reset time.** It used to print one number per account - the larger of the 5h and the 7d - and one reset, the sooner of the two. Those answer different questions: the 5h says when this afternoon frees up, the 7d says which day the account is out until, and a single number answered neither. Both now appear, each with the time it returns.
- **That report is one account per line.** Two windows and two reset times ran a joined line past 150 characters, which defeats the point of printing it; names are padded so the readings form a column, the way `swapdex quota` already lays them out.

## [0.53.0] - 2026-08-12

### Changed
- **Reset times are clock times everywhere, not countdowns.** `62% left 6d` on the dashboard read as more quota - a second bare number beside a percentage does. A time cannot: `5pm` is unmistakably a time, needs no word to be understood, and is shorter than the countdown it replaces, which is what lets it fit at all. `swapdex quota` moved the same way: it prints once and may be read, scrolled back to, or piped long afterwards, and a countdown is true for one second while the time it names stays true. Both first-party CLIs (Claude Code, Codex) show resets this way and neither ships a countdown anywhere.
- **The dashboard's gauge widens when the terminal allows**, so the reset can appear at all. It was capped at 12 columns - too narrow for anything but the percentage - which is why 0.52.0's attempt to name the countdown never reached the screen. Where there is no room the reset is dropped rather than shown bare; the reading always survives.

## [0.52.0] - 2026-08-12

### Changed
- **A reset countdown says that it is one.** The dashboard drew `62% left 6d`, and a second number beside a percentage with no word attached reads as more quota - it is the time until that window comes back. The word goes in whenever the bar can carry it, and the bare countdown only stands in on a bar too narrow for it. Every tool surveyed writes the word: Claude Code `Resets 5am`, Codex `(resets 09:25)`, teamclaude `reset 30m`.
- **The proxy's usage line names when a spent window returns.** It carries a clock time rather than a countdown, because that line is written once and read later - a countdown printed into a log is right for a second and then overstates the wait by however long ago it was written. Same-day stays bare (`resets 3pm`); anything further carries its day. It rides along only for a window actually at its limit: until then it is a number nobody acts on, and a line nobody reads is worse than a line missing a detail.

## [0.51.0] - 2026-08-11

### Fixed
- **doctor stops concluding that an account name and its login are different accounts.** It read an organisation in `.claude.json` beside a `max` or `pro` credential and declared the two belonged to different people, then told you to sign in again. The data cannot support that: someone in an organisation may hold a personal plan, which is an ordinary setup, and the credential carries no account identifier at all - only a plan name and scopes - so nothing local can tell one account from two. It now states what it sees, says plainly when that is normal, names the check that settles it, and is not counted among the problems. A problem list that includes maybes is one people stop reading.

## [0.50.0] - 2026-08-11

### Fixed
- **A supervised proxy takes the port back instead of restarting forever.** A proxy the shim started outlives its shell, is reparented to launchd, and keeps the port. The service agent then cannot bind, exits 1, and `KeepAlive` restarts it into that same failure for as long as the machine is on - 166 times on a real Mac before anyone looked. When the port is held by another swapdex proxy for the SAME tool, the new one now displaces it and binds. Anything else on that port belongs to someone else and is left alone, so a genuine conflict still surfaces as an error rather than a silent takeover.

## [0.49.0] - 2026-08-11

### Fixed
- **The dashboard names the account that actually serves.** A saved snapshot is a copy nothing refreshes - the slot answers for the account - but it carries credentials, so it read as "signed in" and took the row from a live slot that needed one. On a real machine that showed `claude`, a snapshot from weeks earlier, while the slot `personal` sat unnamed and unsigned-in: the one account needing attention was the one hidden. A slot now outranks a snapshot outright rather than as a tiebreak, and the row still borrows the label from whichever half knows the email, so a never-signed-in slot is not shown nameless.
- **The proxy says when a benched account sends the turn back.** After the threshold moved the session and the new account reported a spent window, every following turn fell back to the original account through the quietest path in the proxy - no line, turn after turn, directly under one saying that account was near its limit.

## [0.48.0] - 2026-08-11

### Fixed
- **An account that is refusing turns no longer reads as a reserve.** A percentage is what an account's windows say; whether it will actually take a turn is what its last answer said, and those can disagree. On a real machine the usage line read `rnd 0% (on credits)` two lines below rnd's own replies saying its overage was spent - so the threshold handed rnd the session, rnd refused twice, and the session came straight back to an account at 98%. The rotation was right; only the line describing it was wrong, and it promised a fresh account that did not exist. The measurement is still shown, with what the account is actually doing next to it.

## [0.47.2] - 2026-08-11

### Fixed
- **An account is named once on the usage line.** A number carried over from an earlier round and a failed re-read this round were both printed, so one account appeared twice and the line contradicted itself - `bsgong 89% (on credits), bsgong (usage endpoint throttled)`. An account that has a number keeps it: a failed re-read does not erase what was already known, which is also what lets the threshold keep working across a throttled round.

## [0.47.1] - 2026-08-11

### Fixed
- **"login not readable" now says which of the two it is.** The measurement path was still asking through the lossy reader, so a keychain that will not release a secret to this process and a slot with nothing signed into it printed the same words - and their remedies are opposites (fix the environment, versus sign in). It asks for the reason now, the same way `swapdex quota` already did.

## [0.47.0] - 2026-08-11

### Fixed
- **The proxy says which account it could not measure, and why.** Every failed usage read was dropped by one `if let Fetch::Ok`, and two checks before it skipped without a word, so a throttled endpoint, a token that had lapsed, and a login that could not be read all produced the same thing: the account vanished from the `usage:` line, and a partial round read exactly like a complete one. That silence is not cosmetic - an account with no measurement cannot be held to the threshold, so the one that quietly disappears is the one that stops stepping off before it hits a wall. On one real machine that was the account actually serving. The lapsed-token case is worth naming on its own: serving renews a token on the way past and measuring does not, so an account answering 200s all day can still be unmeasurable.

## [0.46.0] - 2026-08-11

### Added
- **`swapdex doctor` says when two slots hold the same login.** Two directories can carry one account, and nothing on screen said so - the fleet reads as more accounts than exist, and a rate limit hit on one applies to its twin. It is stated rather than counted as a problem, because keeping two directories for one account is a fair thing to do as long as you know that is what you have. A slot whose identity cannot be read is never grouped: two unknowns are not evidence of one account.

## [0.45.0] - 2026-08-11

### Fixed
- **A spent account no longer hands the turn to its own twin.** One account can sit in two slots - two directories, one login - and everything about rotation was keyed on the slot NAME while a rate limit belongs to the ACCOUNT. So when one hit the wall, the next turn went to the other, which is behind that same wall: a rotation that looked like one and bought nothing but a second refusal and a round trip. Slots that share an account uuid are now ruled out together. An identity that cannot be read rules nothing out, because unknown is not the same as spent - otherwise the first refusal would bench every remaining account at once.

## [0.44.0] - 2026-08-09

### Fixed
- **`swapdex quota` no longer reports a locked keychain as a missing login.** Reading a Keychain secret needs an unlocked login keychain, which a remote or non-interactive shell does not have. That refusal was collapsed into the same "no saved token" line printed for an account nobody has signed into, so over ssh every macOS account read as having no login - and what that implies is to go sign in again, to accounts that were signed in the whole time. The distinction already existed one layer down; quota was discarding it.

## [0.43.0] - 2026-08-09

### Fixed
- **doctor no longer calls a correct shim setup broken.** A shell that never reads your shell profile - a script, a cron job, `ssh host cmd` - has no shim directory on its PATH, and doctor reported that as the shim not taking effect, printing as the fix the very line already sitting in the profile. The same wrong message appeared in a terminal that started before `swapdex shim` edited that profile. doctor now separates three states - active here, set up but not on this shell's PATH, and nothing configuring it anywhere - and only the last is counted a problem. The check is scoped to the shim directory in question, so a profile that set up a different store cannot excuse a real finding.

### Changed
- **The release script takes the version from `Cargo.toml`.** It used to default to its own `package.json` version - a field the script rewrites on every publish - so a bare re-run after a release tried to publish that same version again and npm refused. One source of truth is what keeps the four channels from drifting apart.

## [0.42.0] - 2026-08-09

### Added
- **`swapdex export` / `swapdex import` set up a second machine without redoing it by hand.** Export writes the shape of your setup - account names, which tool each belongs to, and your rotation settings - and import re-creates it somewhere else. What it deliberately does NOT carry is a login, or even a path: a file that can hold a token is a file that eventually gets pasted into a chat window, and swapdex exists to keep credentials where they already are. Each account still signs in on its own machine. An account already present is left alone rather than overwritten, because the local one is the one with a login in it and the incoming file has none; `--dry-run` shows what would be created first.

## [0.41.0] - 2026-08-09

### Added
- **The dashboard adds the accounts up for you.** Each tool's heading now carries what the whole group comes to - `2/3 ready · 47% left` - so the question a dashboard exists to answer is answered before the per-account detail. It sits on the heading line that was already there rather than costing a row per tool, and it says nothing at all when a group has one account, because restating the line below it is noise. An account past its window but carrying credits still counts as ready, and one nobody has measured is not reported as empty - an unread gauge and a spent account must not read the same.

## [0.40.1] - 2026-08-09

### Fixed
- **A bare refusal no longer benches an account for a quarter of an hour.** Two different verdicts were sharing one signal. A 429 is reason enough to serve the turn somewhere else - the account said no, and arguing costs a turn. But writing "spent" against it holds it out of the rotation for fifteen minutes, and a 429 carrying no rate-limit headers is a throttle as often as a wall; benching on that is how an account with quota left sits idle. Marking an account spent now needs the response's own `*-status: rejected`, or refusals that keep coming past the retries - saying no over and over being its own explanation. Moving the turn along is unchanged.

## [0.40.0] - 2026-08-09

### Added
- **`swapdex fallback-model` asks for a cheaper model rather than let a turn hit the wall.** A new axis: until now swapdex could only change WHICH ACCOUNT pays, so once every account was out there was nothing left to do. It can now ask for a smaller model instead - but only then. Changing the model gives you something other than what you asked for, so rotating to an account with room always comes first, the setting is off unless you turn it on, and the proxy says on the turn it happens. The corner is recognised two ways: every account measured past the threshold, or every account having refused this very turn - the second needs no usage reading, so it works where that endpoint cannot be reached.

## [0.39.1] - 2026-08-07

### Fixed
- **Usage reads are paced per account instead of all on one clock.** Every account was read on the same fixed interval, which is wrong at both ends: the one near its limit - whose number a rotation actually turns on - went stale between reads, while one sitting at 3% was asked over and over for an answer that could not change. Reading them all together is also what got the usage endpoint to rate-limit swapdex during a survey of its own rivals. An account now waits in proportion to how close it is to mattering: a minute when it is nearly out or has never been measured, fifteen when it has most of its window left. More room can never mean a shorter wait, so the pacing cannot invert at a boundary and hammer exactly the account that needs it least.

## [0.39.0] - 2026-08-07

### Added
- **`swapdex strategy consume-first` spends the quota that is about to reset.** Auto-continue reached for the account with the most left, which is the largest buffer for a burst but lets a nearly-reset window lapse unused. `consume-first` takes the soonest-resetting one instead: quota minutes from turning over costs nothing to spend, and spending it leaves the long windows intact. `roomiest` stays the default and the old behaviour. An account with nothing left is skipped under either, because "soonest" means nothing when there is nothing to spend, and an explicit priority still outranks both.

### Fixed
- **The session stops trading places over a marginal difference.** Only a time cooldown stood between two accounts either side of the threshold, so they handed the session back and forth every time it lapsed - and every hop throws away a warm prompt cache, which is organisation-scoped and expensive to rebuild. A move now has to buy at least ten points of headroom. (Not under `consume-first`, where moving to a smaller window is the whole point.)

## [0.38.1] - 2026-08-07

### Fixed
- **Each service agent binds its own port.** The unit named no port, so it took the command line's default - which is Claude's. The Codex agent therefore tried to bind a port the Claude agent already held, exited, and was restarted into that same failure by the supervisor. `proxy --ensure` had always known to give Codex the next port; the unit does not go through `--ensure`, so the rule now lives in one place both use.
- **A first install no longer looks like it failed.** Installing boots the agent out before loading it, because "already loaded" is the normal case on a re-install - but on a FIRST install there is nothing to boot out, and launchctl's complaint about it was printed as though something had gone wrong.

## [0.38.0] - 2026-08-07

### Added
- **`swapdex service install` hands the proxy to launchd or systemd.** Two things this fixes, both learned the hard way. A proxy the shim starts dies with the shell that started it, so closing a terminal quietly removes it. And one started over ssh on macOS cannot open the Keychain, so it answers every turn by forwarding the client's own login - which is how a broken proxy served for a full day unnoticed. An agent runs in the user's own login session, has that access, restarts if it stops, and keeps its output in a file rather than sending it nowhere. `service status` says what is installed and whether it is up; `service uninstall` takes it away. The unit is a USER agent, never a system one: this process holds one person's credentials. Installing stops whatever the shim already started first, because with restart-on-failure set, a supervised proxy that cannot bind the port would be restarted into that same failure forever.

## [0.37.0] - 2026-08-07

### Added
- **Accounts stop dying of neglect.** An OAuth refresh token rotates when it is used and goes stale when it is not, so an account nobody touches eventually needs a browser sign-in to come back - three accounts on the machine this was built for died that way and stayed dead a week. `swapdex refresh` only ever renewed a token that had already lapsed (or was five minutes from it), which answers "can this serve a turn right now", not "will this account still work tomorrow". A keep-alive sweep now renews any idle account within six hours of expiry: the running proxy does it every 30 minutes, and `swapdex refresh --keep-alive` runs the same sweep by hand or from cron, for a machine where the proxy is not always up. The guard is unchanged and is the whole reason this is safe - a slot the tool is running in is never renewed, because rotating its token is what logs a live session out.

## [0.36.1] - 2026-08-07

### Fixed
- **A proxy that can serve nothing refuses to start.** It used to bind the port anyway, answer every turn by forwarding the CLIENT's own login, and say nothing about it - so it looked like it was working while doing none of what it exists for. Started from an ssh session with a locked Keychain, that state served for a full day before anyone noticed. It now checks before binding and fails with the reason, distinguishing "the Keychain will not open here" (the accounts ARE signed in; the fix is to start it from a terminal on the Mac) from "nothing is signed in" (the fix is `swapdex run <name>`). Failing is the better outcome: the shim gets no port and the tool runs with no proxy, which is the login the user already had. A single unreadable account among several still falls back per request, as before.

## [0.36.0] - 2026-08-07

Both of these come from reading what the other tools in this space already do -
see the survey in the project notes.

### Fixed
- **A refusal no longer sidelines you for half an hour.** Claude Code reads a `Retry-After` over 20 seconds as "cool down for thirty minutes"; under that, it just sleeps. So relaying a spent window's hour-long wait cost the user thirty minutes over something they could step around by pressing Enter. When another account could have taken the turn, the wait handed back is capped at 20s. When there is nowhere to go the real wait stands, because a capped value would only walk the client into the same wall every twenty seconds.
- **A lapsed subscription no longer answers for the whole fleet.** `403` appeared nowhere in the proxy: only `401` and `429` moved a turn along, so an unentitled account took every request, refused it, and stopped, while accounts with quota sat unused. 403 says "this ACCOUNT cannot serve" - the same shape as the other two - and the account is held out rather than asked again. `400` and `404` are deliberately not in that set: a bad request is bad on every account, and retrying it elsewhere spends a second account's quota to be told the same thing.

## [0.35.7] - 2026-08-07

### Fixed
- **A setting reaches the proxy that is already running.** `swapdex auto on` and `swapdex threshold 0.8` were read once, at startup, so changing one did nothing until somebody restarted the proxy - and nothing tells a user to, or does it for them. The pointers deciding who serves have always been read per request; the settings are now too. An explicit `--auto` / `--no-auto` still wins, because that is a decision about one run rather than a default. (Updating swapdex itself already needed no such step: the shim asks for a proxy on every launch, and one from an older build is replaced on the same port.)
- **Credits no longer keep a capped account in front.** Stepping off at the threshold exists to reach an account with free room, and treating extra usage as a reason to stay meant swapdex spent money while another account sat idle with quota to spare. Where credits actually matter is the fallback, and that needed no flag: when nothing else is below the threshold the proxy stays put anyway, and the credits carry that turn.

## [0.35.6] - 2026-08-06

### Fixed
- **The mark follows the key press, not the last turn.** The order of authority behind it had the past outranking the instruction: `proxy-serving` records what the proxy LAST did, which until the next turn goes out is still the previous account, and it was consulted first. So pressing Enter handed the turns over and the row went on naming the old account, with nothing on screen to say the key had worked. What was asked for now wins; a rotation still shows, because it happens when nobody asked for anything, which is exactly when the proxy's own record is the only answer there is.

## [0.35.5] - 2026-08-06

### Fixed
- **Pressing Enter moves the mark, whatever kind of row it is.** The active mark was given one resolver - a running proxy's own record, else the account handed the turns, else the default - but only the SLOT rows were wired to it. Profile rows went on asking their own question, "is this the default account?", and so never saw the serving pointer at all. An account that is both a saved profile and a slot draws as one row, and when the profile half won that merge, Enter moved who pays and left the row reading "ready". The switch had worked the whole time; only the screen disagreed.

## [0.35.4] - 2026-08-05

### Fixed
- **A full window is not the end of an account that can bill credits.** Anthropic keeps serving past the session cap when extra usage is enabled, which is why an account reading "0% left" was answering turns all afternoon. swapdex never read that part of the response, so it called the account spent - on screen, and, worse, in the proxy, which rotated the conversation onto an account nobody had chosen to avoid a wall that was not there. It now reads `extra_usage`, keeps such an account in the rotation, and says `credits` rather than `spent`. Only what the response actually states counts: enabled, and the spend cap not reached. Silence is not permission.
- **One account, one row.** A saved snapshot and a slot can carry the same name - that name IS the account, since every command resolves by it - but rows merged on the email alone, which the not-yet-signed-in half does not have. So `work` appeared twice: once with its ChatGPT address, once as "no login", with nothing to say they were the same thing. The same name on different tools is still two accounts and still two rows.

## [0.35.3] - 2026-08-05

### Fixed
- **A usage gauge no longer contradicts its own caption.** The number counted what was LEFT and the bar filled with what had been SPENT, so an account nobody had touched read "100% left" across an empty bar, and a spent one read "0% left" across a full one. The fill now measures the same thing the number says: full when the window is fresh, empty when it is gone. The warning tone needed no change and reads better for it - a nearly-spent account is now a small red remainder rather than a wall of red. A test had pinned the old behaviour deliberately, on the reasoning that a bar filling as an account is used is the picture people expect; seeing it on a real screen settled that it is not.

## [0.35.2] - 2026-08-05

Both of these were found by using the thing: a sign-in that opened an account
already signed in, and a gauge reading empty on an account that was not.

### Fixed
- **The dashboard's sign-in key signs in.** It ran a BARE `codex` off PATH - which is swapdex's own shim, and the shim puts the proxy in front of any Codex run it does not recognise as plain. A bare launch is not recognised, so the sign-in talked to the proxy, which answered with the account it was already serving: an account with no login of its own came up looking signed in, and anything done in it was billed elsewhere. The command line had this right all along (`codex login --device-auth`, with the signal handling that keeps a Ctrl+C from taking swapdex down with it) - two ways to build one invocation is how they drifted apart, so now there is one, taking the account's home as a parameter. It also resolves the REAL binary rather than whatever PATH answers, so nothing swapdex installs can sit in front of a sign-in.
- **A usage reading is dropped when the window it describes ends.** Readings are remembered so a rate-limited endpoint cannot blank the display, but the remembered number is about a WINDOW, and a window ends. Past its reset it is not stale, it is wrong: the account has a fresh allowance while the gauge goes on drawing the spent one. Found on a machine reading "0% left" ten minutes after the 5-hour window had turned over. The two windows lapse separately, so each is judged on its own - the weekly reading beside it was still good and stays.

## [0.35.1] - 2026-08-05

An update that silently does nothing looks exactly like one that worked. Both
things here come from that: a scope typo made every `npm i -g` a 404 nobody
read, and before it, two installs left the shims calling the copy that had not
been updated. In each case the tool went on running an old binary while every
update reported success.

### Added
- **`swapdex doctor` says which swapdex is actually in use.** Three answers it could not give before: whether the `claude` and `codex` shims call THIS binary (they embed an absolute path to whichever one wrote them, so replacing the other changes nothing they do), how many copies are reachable on PATH (two means one is shadowed, and updating the shadowed one is invisible), and whether the running version is behind what is published. The version check is the only thing in swapdex that reaches the network, it runs in `doctor` and nowhere else, and it is skipped entirely under a sandboxed root.
- **`man swapdex` works when installed from npm.** The page was only ever installed by the Homebrew formula. It is now generated by the binary itself at publish time - so it cannot drift from the command set - and shipped in the npm package, which declares it for npm to link.

## [0.35.0] - 2026-08-04

The account a screen names is now the account that is charged. Every fix below
came out of one question asked repeatedly against the real machine and a second
independent audit: where can what swapdex SAYS and what it DOES come apart?

### Fixed
- **A screen no longer names an account that paid nothing.** The proxy wrote down who was serving the moment it CHOSE a slot - before finding out whether that slot could pay. When it cannot, the proxy steps aside and forwards the client's own credential, so the mark sat on an account that was charged nothing, and stayed there for as long as that account kept being chosen. It goes down where a credential is actually committed to now, inside the retry loop so a same-turn rotation carries it with the token, and the fallback erases it: nobody is the true answer, and every screen already renders that.
- **Handing turns to an account with no login is refused.** That state failed quietly rather than loudly - the turn worked, the dashboard and the status line both named the account, and the user's own account paid. Where it is still reachable, a default pointer naming a slot that was created and never signed into, the label says `work (no login)` instead of claiming it pays.
- **A locked Keychain is not a missing login.** On macOS the Claude credential lives in the Keychain, and a shell that cannot open it - remote, non-interactive - fails to read a token that is perfectly well there. Read as "never signed in", it refused a working account and marked every Claude row "needs login", sending the user to fix something that is not broken.
- **A removed account can no longer be the one paying.** The serving pointer holds a path, and a path outlives the account that owned it: one machine had turns nominally directed at a home with no login in it at all, months after that account was deleted, while `serve` reported the payer as "(unknown)". The pointer now answers only for a directory some registered account still holds, removal takes it along, and adopting that directory back does not silently resume paying.
- **A 401 no longer sidelines an account for the life of the proxy.** It was recorded in two places and cleared in none. The remedy the proxy itself prints is "sign it in again" - and after doing exactly that, the account was still skipped, silently, until the user found and killed a background process nobody had told them about. The exclusion expires.
- **A rate limit is a window, not a verdict.** "Spent" was written and never unset, so a single 429 benched an account for as long as the proxy ran. The response says when the window resets; past that the account is usable and holding it out only strands it. A refusal that names no reset lapses on a fixed window instead.
- **Codex reads a 429 the way Claude does.** It knew only one meaning: every 429 marked the account spent and moved the turn elsewhere, so "slow down for a second" - which the response says explicitly, and which Claude's path has read correctly since 2026-07-27 - cost the user an account. Both share the classifier and one retry counter now.
- **`--account` is honoured on a Codex retry.** It pins a run to one account: every turn is that account's and a refusal is its own answer to give. Claude's path checked the pin before rotating and Codex's did not, so a pinned run quietly billed a different account the moment the pinned one was refused.
- **The dashboard's active mark follows who pays, on Codex too.** Claude's rows asked the running proxy who was serving and fell back to the pointers; Codex's rows asked only the pointer. So pressing Enter, which hands turns to that account, moved who pays and left the mark where it was - the change read as nothing having happened. One resolver answers for both tools.
- **Codex usage belongs to whoever paid for it.** The numbers come back on the token that SERVED the turns and are written into the transcript of the home Codex RAN in, and under `serve` those are different accounts - so one account's usage appeared under another's name, the same mistake this project had already fixed once on the Claude side. Each home is read separately now and captioned with whoever was paying when the record was written. An account that is only a slot - which is what `run`, `adopt` and `onboard` create - also got no usage bar at all, because only the bare `~/.codex` was ever looked at.
- **A Codex reading is as old as the record, not the file.** The observation time came from the transcript's mtime, which moves every time Codex writes anything at all, so a conversation that kept running without the API restating its windows made an hours-old snapshot look freshly taken. With no endpoint to ask, that age is the entire caveat.
- **The sign-in key opens the login for the account's own tool.** Which tool an account belonged to was worked out separately in five places and the versions disagreed; one fell back to Claude whenever the slot registry did not know the name, so pressing it on a Codex account opened Claude's login.
- **A bare `swapdex` opens the dashboard for a slot-only install.** It counted saved profiles and live logins and never slots - which is what `run`, `adopt` and `onboard` create - so the model swapdex steers people into did not count as having accounts.
- **`serve` starts no detached daemon under a sandboxed root**, and `SWAPDEX_TIMING` marks no longer print onto the dashboard they are measuring.

### Added
- **Codex `/status` names the account paying for the turn.** It prints the provider name and nothing else about identity, and with the proxy rewriting the bearer, the login inside `CODEX_HOME` is not who is charged - so the one identity on that screen was the wrong one. It now reads `swapdex: work`. The label is read once at launch, and `serve` says so rather than let a window already open argue with the truth.
- **`swapdex serve --quiet`** prints just the name of the account that pays the next turn, for a shell prompt or a status line.
- **Who paid is recorded.** `serve` wrote nothing to the timeline, so the action that changes the payer left no history at all. It does now, and the two questions stay apart: where a conversation lives is still answered by `use`, never by `serve`.

## [0.34.1] - 2026-08-04

### Fixed
- **Removing an account works on the accounts the dashboard lists.** The delete key only touched the store, and every account the dashboard lists is a slot - so it answered "no profile named X" for exactly what it was showing. `rm` itself searched only Claude's registry, so a Codex account could not be removed at all, from the dashboard or the command line. The confirmation also says what happens now: an account's folder and login both survive, so calling it "delete" invited declining a harmless action, or expecting a folder gone that is still there.

### Changed
- clap moves to 4.6.5, and the token renewal added in 0.34.0 now has the test it was missing. Its success path had never been executed - verifying it needs an account that is idle AND holds a refresh token the server still honours, and there was not one to spare - so it runs against a token endpoint of our own: the request built, the answer merged, the credential written back, and the account reading as current afterwards.

## [0.34.0] - 2026-08-03

### Changed
- **Changing accounts keeps the conversation, by default.** The point of the tool is one place where every conversation lives with accounts swapped underneath, but the most natural action - Enter in the dashboard - ran `use`, which moves the store those conversations live in. So the ordinary way to change accounts was also the way to split a history in two: one machine ended up with 256 Codex conversations under one account and 2 under the other. Enter and the `/swap` command now **serve**: the account pays for the turns and the conversation stays put. `swapdex use` remains for actually changing where new sessions start. `swapdex serve` also starts the proxy it needs - directing turns with nothing to carry them was a setting that quietly did nothing.

### Added
- **`swapdex refresh`: renew a lapsed access token without signing in again.** An access token lives about an hour and only the account's own refresh token can renew it, which swapdex would not do - so any account idle for an hour looked expired, the proxy stepped over it, and `quota` reported it dead. Those are exactly the accounts with quota left. A slot is never renewed while its tool is running there: a refresh token rotates, the running process holds the old one, and renewing it out from under that process is how an account gets logged out. The proxy and `quota` renew before writing an account off.
- **`swapdex whereis` finds Codex conversations too.** Codex has the same property Claude does - a conversation lives inside the home the tool was launched with - and it files transcripts by date with the working directory inside each, so they are found by reading rather than by listing. The line printed reopens one with `CODEX_HOME` named in full.
- **`SWAPDEX_TIMING=1` reports where startup time went**, for a delay that only happens on one machine.

### Fixed
- **The codex shim no longer empties the session picker.** Codex lists the sessions matching its configured provider, and the shim set a swapdex provider on every run including `resume` - so a directory holding 158 conversations showed "No sessions yet". The provider overrides now go only on runs that talk to the model.
- **The dashboard counts down, like everything else that reports quota.** The gauges printed how much had been SPENT with no word to say so, while Claude's own status and `swapdex quota` both count what is LEFT - so a window that had just reset showed "2%" and read as almost nothing left when it meant almost nothing used.
- **Opening the dashboard no longer freezes it.** Reading every account's usage ran on the event loop, so with several accounts it took the keyboard and the cursor away for seconds on every launch. It runs on a thread, the last reading is drawn immediately (marked "checking..." while a live one is in flight), and the accounts are read at once rather than one after another: six seconds became one.
- **Signing in works from the dashboard.** Three separate faults: the shim set a proxy address on `/login`, the proxy rewrote the OAuth exchange for a `/login` typed inside a running session, and the sign-in key tore the dashboard down instead of returning to it. Mouse capture is also released while the sign-in runs - with it on, a mouse paste never arrives as text, which is what a code prompt asks for.
- **Each tool's proxy records its own serving account.** Both wrote one file, so whichever answered last decided what BOTH dashboards read, and stopping one erased the other's.
- **An account that cannot serve is not called "ready"**, renaming from the dashboard works on the accounts it lists, the dashboard opens for a slot-only install, the rename prompt looks like something you type into, and the word "slot" no longer appears in anything a person reads.
- **A test that failed for the calendar rather than for the code**: fixtures carried literal timestamps from the week they were written and started failing when those moments went by.

## [0.33.0] - 2026-07-31

### Fixed
- **Every Claude account displayed as spent no matter how little had been used.** The usage endpoint reports `utilization` as a PERCENTAGE, and it was read as a 0..1 fraction and multiplied by 100 - so anything above 1% clamped to "100% used". A session 4% in showed as exhausted, and the proxy's threshold stepped off accounts that had almost everything left. A week of recorded readings held only 0.0 and 100.0 and nothing in between, which is what the clamp looks like from outside; the endpoint itself answers `"utilization": 4.0` beside `"limits":[{"kind":"session","percent":4}]`. Two tests asserted the fraction, so the bug was pinned rather than caught - they were assumptions about the endpoint written as if they were observations. Readings already recorded under the misread are dropped when the cache is read, since they are all exactly 100 and would otherwise keep showing accounts as spent after the fix.
- **Signing in works from the dashboard, and no longer goes through the proxy.** Three separate faults, each fixed where it lived. The shim set `ANTHROPIC_BASE_URL` unconditionally, so `claude /login` did its OAuth exchange against swapdex; it now recognises login, logout, and setup-token and asks for no proxy on those runs, and `swapdex run` clears an inherited address. That guard can only see a launch's arguments, so the proxy itself now passes oauth, login, logout, authorize, and auth paths straight through with the credential the client sent - which is what `/login` typed inside a running session needs. And the sign-in key tore the dashboard down: it is a child process now, with the terminal handed back for its duration and taken again after, so several accounts can be added in one sitting. Mouse capture is released for that duration too - it is not part of ratatui's restore, and while it is on a mouse paste never arrives as text, which is what a code prompt asks for.
- **The dashboard opens for a slot-only install.** The check counted store profiles and live logins and not slots, so with nothing signed in yet it reported no accounts - and signing in is what the dashboard is for.
- **Renaming from the dashboard works on the accounts it lists.** It only ever touched the store, and every account it lists is a slot.

## [0.32.0] - 2026-07-29

### Added
- **`swapdex serve <name>`: hand turns to an account without moving where your conversations live.** One pointer was answering two different questions - where a new session starts, which decides the conversations `claude -r` can offer, and who pays for a turn - so a user who only meant to change the payer also moved the store, and their work appeared to have vanished. `serve` answers only the second: turns go to that account, the launch store is untouched, and with proxy mode running the conversation already open changes account from its next turn. `swapdex serve --off` returns to each session paying for itself, and `swapdex use` still settles both so the two cannot drift into a combination nobody asked for. This is the arrangement the tool was built for - one place where every conversation lives, accounts swapped underneath as they run out. A switcher that copies credentials gets it by only ever having one store, and pays for it with tokens that go stale and revoke each other; isolating accounts is what stopped that, and it is also what split the conversations, so the split had to be undone deliberately rather than by giving the isolation back.
- **`swapdex whereis [project]`: find which account holds a conversation.** Claude keeps conversations inside the config dir it was launched with, so switching accounts also switches which ones `-c` and `-r` can see. Nothing is lost, but "no conversation found" reads exactly like the work is gone. This searches every account's store directly - newest first, with the one line that reopens each, config dir spelled out in full - and reads the filesystem rather than inferring from the switch timeline, so it answers for conversations that predate any switch swapdex recorded and needs no sessionwiki.

### Fixed
- **A switch is never reported as live when nothing running moved.** `use` on a slot printed only that the default had changed, so an assistant driving `/swap` reported "this account serves you from the next turn" while the conversation carried on with the account it began with - the work continuing on the account the user believed they had left. The line now says which of the two happened, names the open session as unaffected when there is no proxy, and spells out the command that would move it.
- **The bare `~/.claude` is an account.** It was excluded from discovery, so the account everyone starts from was the one account swapdex could not switch back to - and every conversation begun before the first switch lived in it. doctor also reports it directly when the default store holds conversations and is not registered.
- **An account name that reads as the tool's own home is refused.** A slot called `claude` looks like it points at `~/.claude` and does not; a user went looking for their conversations in it and the ones they wanted were in a different account entirely. Refused at create, adopt, and rename, with a usable alternative named; `migrate` (where the collision was minted) names such a slot around the clash and says so; doctor names any that already exist.
- **The same PATH line is no longer appended on every install.** "Already there" compared the exact text this version emits, but the same directory can be written as `$HOME/...`, `~/...`, or in full - a real profile ended up with three copies. It is compared by meaning now.
- **The `swapdex ui` cursor is visible.** Moving up and down changed only the weight of the letters. The selected row carries a band behind its text columns and a solid mark at its left edge; the band stops where the quota bars begin, because a bar's fill colour is its reading.
- **Starting a new conversation is reachable again.** The entries were built by filtering an account's saved tool list, and a slot account has none - so every slot offered no way to start anything, on the screen whose purpose is starting things. They also sat below the fold after the recent sessions. They lead now, and `o` opens the screen even with a proxy running.

## [0.31.0] - 2026-07-28

### Added
- **Codex accounts now get the same permanent slots Claude has, and the same mid-session switching.** Codex reads `CODEX_HOME` the way Claude reads `CLAUDE_CONFIG_DIR`, so everything that stopped Claude accounts rotting applies unchanged: the account lives in its own home, Codex refreshes that credential in place, and switching moves a pointer instead of copying `auth.json`. Copying is what made saved Codex logins go stale and what let a running session revoke the account being switched away from. `swapdex run <name> --tool codex` creates and launches one, `--no-launch` just creates it, `swapdex adopt <name> <dir> --tool codex` registers a `CODEX_HOME` you already run by hand (in place - never moved, copied, or written to), `swapdex use <name> --tool codex` repoints, and `swapdex slots` lists both tools with the account each plain launch would use. Each tool keeps its own default pointer, so switching Codex never moves where Claude launches; slots recorded before tools were distinguished are Claude's, the only kind that existed.
- **`swapdex proxy --tool codex`: change accounts inside a Codex conversation that is already running.** Swapping `auth.json` only ever affects the NEXT session - the token a running Codex holds is in its memory, which is why every other Codex account switcher tells you to quit and relaunch. Codex leaves a way past that: a model provider declaring no `env_key` makes it attach its own ChatGPT OAuth bearer and `ChatGPT-Account-ID` to each turn, so the request arrives already shaped and changing accounts is a rewrite of that pair with the one held by the slot serving the turn. The pair moves together or not at all - a token from one account with another's account-id is a request the backend refuses - and both headers are dropped before the new ones are added, so a client that sent them in different casing cannot leave a stale copy behind. With `--auto`, a refused turn is handed to another account and re-served rather than returned as a failure. The `codex` shim asks for a live proxy and prepends the provider overrides when there is one: nothing is written to `config.toml`, and a proxy that cannot start leaves Codex running exactly as it would have. Each tool's proxy has its own marker and default port, so both can be up at once. Stepping off BEFORE the wall stays Claude-only - it needs a zero-spend usage reading, which Codex has no endpoint for, so Codex accounts move when one actually refuses. Verified on real accounts: one `codex` process made four turns, and the account changed between the first and the second while it ran.
- **`swapdex proxy`: change accounts in a conversation that is already running, and optionally continue on another account when one runs out.** Switching credentials on disk can never affect a live `claude` - it holds its auth in memory, so a switch only lands in the next session. Proxy mode intercepts the API traffic instead (a loopback HTTP server; Claude reaches it via `ANTHROPIC_BASE_URL`), which makes every turn its own decision: `swapdex use <name>` - or Enter in the TUI - moves the conversation you are in the middle of, and with `--auto` an account that reports itself spent hands the session to another one by itself. Rotation happens at the turn boundary, so a completed answer is never severed. If the `claude` shim is installed, a plain `claude` picks up a running proxy on its own (the proxy publishes its pid and port; a stale marker from a hard kill is ignored rather than sending Claude at a dead port), so there is no env var to remember. Tokens are read straight from each account's slot and never copied anywhere - unlike a switcher that keeps its own copy of your tokens, there is no second store to go stale. Quota state comes free from the rate-limit headers on responses you were already making, so nothing probes the API and no keep-warm traffic exists. The request body's `metadata.user_id` is realigned to the account actually serving the turn, since otherwise a rotated request would contradict itself; nothing else in the body, and never the prompt, is touched. Loopback-only bind, and the log records the account, path and status - never a body, never a token. Linux, WSL and macOS.
- **`swapdex doctor` now reports each account slot's login health.** A slot that was never signed into is named with the one step that fixes it (`swapdex run <name>` once signs it in), and a slot whose login sat unrefreshed past the 30-day stale window is flagged with `login idle ~Nd`. Routine access-token expiry (hours) is deliberately NOT flagged - Claude refreshes that silently on the next run, so flagging it would mean a daily false "expired". The freshness signal is read-only: the `expiresAt` in the slot's own `.credentials.json`, or on macOS the slot's Keychain item modification date via an attribute-only lookup (no secret read, no ACL prompt), taking whichever is newest so a leftover pre-Keychain file cannot make a healthy slot look idle. A login artifact that exists but cannot be parsed stays quiet rather than being misreported as "no login yet" - doctor flags only what it can determine.
- **`swapdex doctor` now checks whether the `claude` shim actually takes effect, not just whether it exists.** An installed shim that PATH never reaches is a trap: the setup looks complete while a plain `claude` still runs the real binary, so `swapdex use` flips the default pointer and nothing ever reads it - a switch that silently does nothing. doctor now resolves what a plain `claude` really runs and reports `shim active` only when that is the shim; otherwise it says the shim is NOT taking effect, names the binary winning on PATH, and prints the one `export PATH=...` line that fixes it.

### Fixed
- **`swapdex doctor` no longer starts a proxy as a side effect of running.** Checking whether a tool was on PATH ran `<tool> --version`; with the shim installed that executes the shim, and the shim starts a proxy - so a command whose whole job is to observe launched a daemon every time it ran. It now looks for an executable on PATH instead of running one.
- **The dashboard no longer shows a healthy account as an empty pair of bars.** Four separate things could blank a Claude row, and all four looked identical to an account with nothing left. (1) Asking for several accounts in a row rate-limits the usage endpoint; the accounts at the back of the queue lost every time, so reads are now spaced apart and backed off up to three times. (2) The list shows one row per account, and the row that survives a merge often carries the other name - a reading taken for `rnd-slot` never reached the row named `rnd`. A merged row now keeps the names it absorbed, and its usage is looked up under any of them, preferring whichever name actually carries numbers. (3) The last successful reading per account is remembered and fills the gaps, shown with its age so a remembered number is never mistaken for a live one. It is written where the reading is taken (`swapdex quota`), not where it is displayed - the dashboard cannot record a reading it never received, which is exactly the case that needed it. (4) A row that still has no numbers now says why - `saved token expired`, `endpoint busy - retrying` - in the column that otherwise carries a figure's age.
- **A saved account whose token had expired reported the endpoint as busy.** The usage endpoint refuses a lapsed token the same way it refuses a burst of requests, so every inactive snapshot said "endpoint busy just now - the account is fine, try again in a moment". It was not busy, and the advice could never come true: the token was dead. The expiry recorded in the credential is now read before asking, so an expired account says so and names the fix, and the three retries it used to spend are no longer taken from the accounts that can answer. A live login and a slot are exempt - both are refreshed in place by the tool that owns them, and only a snapshot can rot.
- **`swapdex proxy` no longer stalls a turn to refresh its usage readings.** Refreshing means waiting out the endpoint's throttling, which is seconds, with the user sitting in front of a prompt. Only the first reading is waited for - with nothing measured there is nothing to steer by, and the turn would start on the very account the threshold exists to step off; every later refresh runs off the request path against the previous reading.

### Changed
- **`swapdex ui`: the `r` key now switches to the account you used before (`use -`), not `restore`.** `restore` puts back the login that was live before the *last switch*, which in hub-and-spoke use (always switching away from one base account) is always that same base - so `r` appeared to return to one fixed account rather than the account you actually used before. `r` is now the previous-account toggle and the key hint reads `previous`. `swapdex restore` remains available as an explicit command and safety net for undoing a switch.

## [0.30.0] - 2026-07-22

Never published: the version was bumped but no tag, crate, package, or formula ever carried it. Everything below ships in 0.31.0.

### Added
- **The `swapdex ui` main screen shows each account's 5h utilization as a right-aligned bar + percent.** Every Claude account row carries its own filled/empty bar and `NN%` of the 5h limit used (calm green -> amber -> red as it fills) at the right edge - the way a team dashboard shows per-member usage - so you can see who's near their limit at a glance. It's the real quota utilization (from the live usage endpoint), fetched once lazily after the first frame (the UI still opens instantly; the bars fill in) and per account, so it does one network read per account when you open the UI. `u` still opens the local token breakdown; `%` opens the full quota view.
- **Saving a profile now shows which account was captured.** The message went from `saved profile <name> (<tools>)` to `saved profile <name> (claude-code = <email>)`, so saving a profile while a stale Claude `oauthAccount` is the live identity (which would silently attach the wrong account, only surfacing later on `use`) is caught at save time. Claude has no token-side account id to cross-check, so the human seeing the email is the guard.

### Changed
- **npm distribution no longer runs an install script.** `@youdie006/swapdex` used a `postinstall` (`node install.js`) that downloaded the platform binary, which trips npm's allow-scripts prompt (`npm install -g --allow-scripts=@youdie006/swapdex` friction). It now ships the prebuilt binary as per-platform packages (`@youdie006/swapdex-{darwin,linux}-{arm64,x64}`) listed in `optionalDependencies`, `os`/`cpu`-gated so npm installs only the one matching the machine (the esbuild / @biomejs pattern). A tiny `bin/swapdex.js` launcher `require.resolve`s that binary and execs it - no install script, no prompt. cargo/homebrew unchanged.

### Fixed
- **`gemini` apply is now crash-atomic via a roll-back journal.** Applying a Gemini login swaps two files together (`oauth_creds.json` + `google_accounts.json`); a SIGKILL/power loss between the two writes left one account's token with another's identity, which a later `use` would silently apply. apply now writes+fsyncs a WAL of both files' prior bytes BEFORE the first write and removes it once the state is consistent; a surviving WAL means the run was interrupted and is rolled back to the pre-switch state on the next apply/capture (`recover_interrupted_gemini_apply`). This brings the gemini adapter to parity with the claude adapter's crash-atomicity WAL (#1) - both are credential-file paths with no Keychain, fully covered by tests.
- **`swapdex use` now guards the non-slotted tools (codex, gemini, antigravity) against switching while the tool is running.** These tools all rotate their OAuth token on refresh, and swapdex swaps their shared credential files - so switching a codex/gemini/antigravity account while that tool runs (a terminal session, or a `codex` MCP server used from Claude) lets the session's next refresh revoke the account being switched, logging it out (the "use a codex MCP from Claude, get logged out" report). `use` now refuses the switch while a matching process is running (the running-session analog of Claude's 0.25.0 guard - Claude itself is slot-isolated and already guarded); `--force` overrides. Detection is by process name (these tools are not slot-isolated). Note: this protects the SWITCH; two sessions run concurrently on the SAME account can still rotate each other out, which is the tool's own OAuth behaviour.

## [0.29.0] - 2026-07-20

The last three findings from the 0.24.3 adversarial review - crash-atomicity and
concurrency hardening for the classic snapshot path. Completes issue #4 (all five
findings #1-#5 now fixed). Narrow by nature (each needs a crash / I/O failure /
concurrency-timing window), and on the classic path the slot model does not use.

### Fixed
- **`apply` is now crash-atomic via a roll-back journal (#1, the last of the 0.24.3 review).** Applying a Claude login mutates three resources in order (the credential file, the macOS Keychain, `.claude.json`'s oauthAccount); a SIGKILL/power loss BETWEEN them left A's token with B's identity, which a later `use` would silently apply. `apply` now writes+fsyncs a WAL of each resource's prior state BEFORE the first mutation and removes it once the state is consistent again; a surviving WAL means the run was interrupted and is rolled back to the pre-switch state on the next apply/capture (`recover_interrupted_apply`). The credential-file + config path is covered by tests (it is the path that protects Linux/WSL, where there is no Keychain); the macOS Keychain slice is journaled and recovered the same way.
- **Interactive `login` no longer races a concurrent switch (#5).** A per-tool credential lock is held across the whole `login` flow - including the interactive sign-in, during which the store lock is deliberately released so unrelated ops proceed - and `use`/`restore` take it for the tools they switch. So a `swapdex use`/`restore` can no longer interleave with a sign-in on the SAME tool (pairing the wrong token with the new account). The final store writes re-acquire the store lock with a bounded retry, replacing the old best-effort that could write the timeline unlocked.
- **Profile save is now crash-transactional (#2, from the 0.24.3 review).** Overwriting a profile (a token refresh on `use`/`login`/`restore`) wrote its blobs one at a time, so a crash between the credential and identity writes could leave A's token paired with B's identity - a mismatch a later `use` would silently apply. `save` now builds the full generation in a `.<tool>.staging` dir, fsyncs, and swaps it in atomically; a crash in the swap window is healed on the next read/write (`reconcile_tool`) to a complete generation, never a mixed one.

## [0.28.0] - 2026-07-20

### Added
- **`swapdex login --tool codex` uses codex's device-code flow (`--device-auth`) by default**, so sign-in works over SSH / on a headless box - codex's default login is a localhost-redirect browser flow that needs a browser reaching localhost on the same machine, which fails remotely. Opt back into the browser flow with `SWAPDEX_CODEX_LOGIN=browser`. (Claude Code already falls back to its device-code URL+paste-code flow in headless/SSH environments, so `--tool claude` needs no change.)

### Fixed
- **Classic-path hardening from the 0.24.3 adversarial review (#4), the two findings reachable on a plain I/O failure (not only a crash):**
  - `claude` apply rollback no longer strands a freshly-created Keychain item. When a `.claude.json` write fails after the Keychain token was written AND no prior item existed, the rollback now deletes exactly the item it created instead of leaving A's token in the Keychain against B's file/config (a mismatch a later `use` would silently apply). The prior-token read is now tri-state: a read it cannot perform aborts before any mutation, so a rollback is always possible.
  - A `restore` whose apply fails no longer strands the requested backup. The outgoing login is backed up only AFTER apply succeeds; previously it was backed up first, becoming the newest backup, so a retry saw it as "already active" and never restored the target.

## [0.27.0] - 2026-07-15

### Added
- **`swapdex sync-mcp`** shares your MCP servers across slots. `settings.json`
  and global `CLAUDE.md` are symlinked into each new slot automatically, but MCP
  config lives in the per-account `.claude.json` (mixed with the account
  identity), so it is shared with an explicit merge: the `mcpServers` block from
  `~/.claude.json` is copied into every slot's own `.claude.json`, preserving
  each slot's `oauthAccount`. Run it after logging into your slots.

### Fixed
- **The `claude` shim never bakes a self-reference.** Re-running `swapdex shim`
  with the shim dir already on `PATH` could pick the shim itself as the "real"
  claude and create an exec loop. It now skips any `claude` that carries the
  shim marker, robust against `~`/symlink/relative `PATH` spellings.

### Docs
- README and `docs/COMMANDS.md` now document the permanent-slot model end to end
  (`run`, `use` repoint + the `claude` shim, `onboard`, `adopt`, `migrate`,
  `sync-mcp`), with the classic snapshot commands kept as the coexisting path.

## [0.26.1] - 2026-07-14

### Fixed
- **Guided onboarding now runs automatically on first launch.** A bare `swapdex`
  on an interactive terminal offers the guided setup when there is something to
  set up (existing `~/.claude-*` dirs to register, or legacy profiles to
  migrate), instead of requiring you to already know the `swapdex onboard`
  command. It is shown once (a marker prevents re-nagging), then a bare `swapdex`
  drops into the normal picker. Non-interactive shells (pipes, scripts) are never
  hijacked - they still print the banner.

## [0.26.0] - 2026-07-14

The **permanent-slot account model**: each account lives in its own
`CLAUDE_CONFIG_DIR`, so switching never copies a token - and therefore can never
log an account out when a running session refreshes it. This is the complete fix
for the rotation-logout class that the 0.25.0 guard only *prevented*. swapdex
never writes a credential in this model; each account's own login creates and
refreshes its token in place.

### Added
- **`swapdex run <name>`** - launch Claude in an account's own permanent slot.
  Concurrent-safe: each terminal picks its own account, and they never collide.
- **`swapdex use <name>`** now repoints a lightweight default-account pointer for
  slot accounts (no credential copy). **`swapdex shim`** installs a `claude`
  shortcut that follows it, so a plain `claude` uses your default account.
- **`swapdex onboard`** - guided setup that registers existing `~/.claude-*`
  config dirs, migrates legacy profiles, and offers the shim, one prompt at a
  time. It never mentions "slots" - just gets you to a safe state.
- **`swapdex adopt <name> <dir>`** - register an existing `CLAUDE_CONFIG_DIR`
  directory as an account, in place (not moved).
- **`swapdex migrate`** - give each legacy copy-model Claude profile its own slot.
- **`swapdex slots`** lists the account slots; `doctor` now reports the slots, the
  default account, and whether the shim is installed.
- New slots inherit shared config (`settings.json`, global `CLAUDE.md`) from
  `~/.claude` via symlink, so switching accounts keeps your tooling. Tokens and
  history stay per-slot.

### Notes
- The legacy copy-switch (`use` on a profile that is not a slot) still works,
  guarded by the 0.25.0 running-session check, until you migrate.
- MCP server config lives in the per-account `.claude.json` and is not yet shared
  across slots.

## [0.25.0] - 2026-07-14

Prevents a real logout: switching a Claude account while a `claude` session is
still running on that same login slot. Confirmed on a multi-`CLAUDE_CONFIG_DIR`
macOS setup, cross-checked by a two-model design review.

### Fixed
- **`swapdex use` now refuses to switch Claude while a session is running on
  that login slot.** Claude's OAuth refresh tokens rotate (each refresh revokes
  the previous one). If a `claude` session keeps running after a switch, its
  next refresh rewrites the slot's token and revokes the snapshot swapdex just
  saved for the outgoing account - so switching back later logged that account
  out. swapdex now detects a running `claude` and the login slot it uses (its
  `CLAUDE_CONFIG_DIR`), and refuses the switch when a session is on the very
  slot being swapped. A session on a *different* `CLAUDE_CONFIG_DIR` profile is
  correctly ignored. If the running session's slot can't be read, it fails
  closed (refuses) rather than risk the logout.

### Added
- **`swapdex use --force`** to switch anyway when you know the running session
  is on a different account, or you have quit it. The refusal message names the
  flag and the risk.

## [0.24.6] - 2026-07-14

Onboarding and TUI robustness across less-common conditions (empty sessionwiki
index, a corrupt single-tool login, a concurrent `swapdex rm`), from a
cross-model review of the onboarding and menu paths.

### Fixed
- **An empty sessionwiki index no longer hides your real sessions.** When
  sessionwiki was installed but never `sync`ed (or its index was genuinely
  empty), `swapdex ui`'s "open a conversation" menu stopped at the empty
  sessionwiki result and showed a blank list. It now falls through to the
  native on-disk reader, so the sessions you can see on disk still appear.
- **The post-switch "open" prompt only offers tools the profile holds.** A
  Codex-only profile used to offer "c new claude"; pressing it opened your
  unrelated live Claude account in a new conversation. The plain menu now
  filters the new-conversation keys to the switched profile's own tools, the
  same rule the full TUI already applied.
- **`swapdex setup` no longer aborts when one tool's login is unreadable.** A
  corrupt or hand-edited credentials file for a single tool aborted the whole
  wizard before the other, valid tools were saved. It now warns for that tool
  and continues, just as it does for a tool you are simply not logged into.
- **The TUI no longer panics if a `swapdex rm` shrinks the list mid-session.**
  Switch, open, rename, and delete now use bounds-checked row access and clamp
  the selection after the row count changes, so a stale highlight can never
  index past the end.
- **Mouse clicks below the list no longer switch an account.** A click on the
  footer/help rows under the list box mapped to a hidden entry and could
  synthesize a switch; clicks are now confined to the list's inner area.
- **Mouse clicks on a scrolled session list open the row you clicked.** The
  click-to-row math ignored the list's scroll offset, so on a long, scrolled
  "open a conversation" menu a click opened an earlier, hidden session -
  possibly from another account. It is now offset-aware.
- **`o` (open a profile's sessions) switches to that profile first.** Opening a
  new conversation launches under whichever account is live, so pressing `o` on
  a profile you had not switched to opened your *currently live* account
  instead. `o` now switches first, exactly like Enter (it still differs by
  always showing the full menu rather than shortcutting a single-tool profile).
- **The welcome screen after deleting your last profile no longer claims you
  are logged out.** Deleting the final profile drops to the onboarding screen,
  which had stale "logged-in tools" state and hid the "save these" shortcut for
  a login the delete never touched; it is now recomputed after a delete.

## [0.24.5] - 2026-07-14

### Added
- **The native session menu now nudges you toward sessionwiki.** When
  sessionwiki is not installed, `swapdex ui`'s "open a conversation" menu shows
  a one-line footer: "install sessionwiki to search these, trace a file to its
  session, and group by account" - so you know the switching works today and
  what installing sessionwiki would add on top.

## [0.24.4] - 2026-07-14

### Fixed
- **Without sessionwiki, Codex sessions no longer show "(no prompt)".** The
  native session menu (`swapdex ui` when sessionwiki is not installed) read
  only Codex's old transcript shape; current rollouts carry the first prompt as
  `event_msg`/`user_message`, so every Codex session titled as "(no prompt)".
  It now reads all three shapes (event_msg, response_item, and the 2025-era
  bare message) and skips AGENTS.md / environment boilerplate - the same drift
  fixed in sessionwiki 0.19.3, now in swapdex's own native reader. Claude
  titles were unaffected.
- `swapdex sessions` without sessionwiki now points at `swapdex ui`, which
  lists recent sessions natively, instead of only saying "install sessionwiki".

## [0.24.3] - 2026-07-14

Hardening from a cross-model adversarial review (GPT-5.6 code pass + ChatGPT
Pro invariant pass). Both independently flagged the stale-file/Keychain issue
as the one a normal macOS user actually hits.

### Fixed
- **macOS: the authoritative Keychain is now read first.** swapdex leaves a
  `~/.claude/.credentials.json` behind on switch, but Claude refreshes its
  token in the *Keychain* (rotating the refresh token) without rewriting that
  file. Reading the file first handed back a stale, possibly-revoked token -
  and the switch-away writeback then persisted it into the profile, losing the
  live login. When the Keychain is in play it is now the source of truth; the
  file is only a fallback.
- **`swapdex quota` no longer leaks the account token to a curl trace.** curl
  reads `~/.curlrc` even with `--config -`; a user `curlrc` with `verbose` or
  `trace-ascii` could log the `Authorization: Bearer` header. curl is now run
  with `-q` first (disables curlrc), and the `SWAPDEX_CURL` test hook is
  honored only under `SWAPDEX_ROOT`, never in production.
- **macOS: switching/sign-out never touches another profile's Keychain item.**
  Keychain writes and deletes now target only the item this environment
  *derives* (never a discovered one), so a plain `swapdex` with a single
  aliased `CLAUDE_CONFIG_DIR` login can no longer overwrite or delete that
  alias profile.
- **A switch never overwrites a recoverable login it could not back up.** If
  the live login is valid (identity resolves) but a sibling file is corrupt
  (a hand-edited `~/.claude.json`, a broken Gemini `google_accounts.json`),
  `use`/`restore` now refuse for that tool and point at the repair instead of
  overwriting it with no backup. A genuinely broken login (unparseable) still
  gets replaced, as before.
- **`rm` and permission-tightening never follow a symlink out of the store.**
  A symlink planted inside the 0700 store could make secure-overwrite or chmod
  escape to an external file; the traversal now uses `lstat` and skips symlinks.
- **`swapdex quota` shows a sane reset countdown even if the endpoint returns
  milliseconds.** A 13-digit `resets_at` is normalized to seconds instead of
  rendering "resets in 21970092d".

## [0.24.2] - 2026-07-14

### Fixed
- **`swapdex setup` no longer lets you name a profile `-`.** The name `-` is
  reserved for `swapdex use -` (toggle to the previous profile), and `add` /
  `rename` already reject it - but setup's interactive save bypassed that
  guard, so answering `-` at the name prompt created a `-` profile that then
  broke `use -` ("can't tell which profile '-' means"). The shared name
  prompt (`ask_name`, used by setup, `login`, and interactive `add`) now
  rejects `-` and re-asks, matching the non-interactive commands.

## [0.24.1] - 2026-07-14

Two real-use bugs from an adversarial scenario sweep (12 sandboxed
workflows + live checks on a real multi-profile Mac).

### Fixed
- **`status` no longer cries "access token expired".** An OAuth access token
  lapses about hourly and the tool refreshes it silently; `status` still
  flagged every just-lapsed token with "access token expired, may re-prompt".
  This was the `status` twin of the 0.20.0 ls/marker fix - the same line, in a
  code path that fix missed. Now only a login older than 30 days (whose
  refresh token may actually be dead) gets a soft note. This is the daily
  false alarm behind "it keeps saying expired".
- **`add` on a corrupt `~/.claude.json` no longer claims you are "not logged
  in".** With a valid credential but a hand-edited config that has a JSON
  syntax error, `add` printed "not logged in to any selected tool" and exited
  3 - sending you to re-log-in when the real fix is to repair the file. It now
  exits 1 and points at the corrupt file (the detailed per-tool error was
  already correct; only the summary + exit code were wrong).

## [0.24.0] - 2026-07-13

### Changed
- **macOS Keychain resolution now mirrors Claude Code exactly - parallel
  CLAUDE_CONFIG_DIR profiles are finally safe.** Ground truth from a real
  multi-profile Mac (three live profiles: plain `claude` plus two
  `CLAUDE_CONFIG_DIR` aliases, three Keychain items) showed the root cause of
  "my switch did not stick" and "my other logins got wiped": the old
  suffix-preferring Keychain scan grabbed an ALIASED profile's item while the
  user's plain `claude` read the bare one - so switches wrote the wrong item,
  and add-account's cleanup deleted the wrong profile's login (plus the bare
  one). The new contract: **swapdex manages the profile of the environment it
  runs in**, derived the same deterministic way `claude` derives it (no env ->
  the bare/default item; CLAUDE_CONFIG_DIR set -> that profile's suffixed
  item). The scan remains only as a fallback when the derived item does not
  exist AND exactly one Claude login exists (alias-only setups); with several
  items it refuses to guess instead of corrupting another profile.
- Add-account's Keychain cleanup now deletes ONLY the resolved item - the old
  "also clear the bare name" extra could kill a live default profile.
- A fresh Keychain write (first switch on a Mac, or right after a sign-out)
  now creates the slot the environment derives, never a discovered one.
- `doctor` describes coexisting profiles truthfully: other Claude items are
  reported as "other CLAUDE_CONFIG_DIR profiles (or leftovers) - swapdex never
  touches them", not as stale strays to delete; a refused ambiguous resolution
  comes with the exact way out.

## [0.23.1] - 2026-07-12

Hardening release from an adversarial review of 0.21.0-0.23.0 (one reviewer
agent + a manual pass; 11 findings, the ones that matter fixed here).

### Fixed
- **macOS: SWAPDEX_ROOT now really isolates.** SWAPDEX_ROOT redirected every
  FILE path into a sandbox but Keychain writes still hit the machine-global
  login Keychain - a sandboxed test switch on a Mac could overwrite the REAL
  Claude token. All Keychain operations are now disabled under SWAPDEX_ROOT
  (file-only, like Linux).
- **Keychain resolution: exact match first.** When swapdex sees the same
  CLAUDE_CONFIG_DIR that `claude` launches with, the computed suffixed service
  name is used directly; the dump-keychain scan (which can pick a stale sibling
  when several suffixed items linger) is now only the fallback.
- **`doctor` keychain check has real teeth now.** 0.23.0's mismatch check could
  effectively never fire (the switch path resolves its target from the same
  scan). It now flags the two detectable causes: several suffixed items with no
  CLAUDE_CONFIG_DIR to break the tie (the scan can only guess), and the
  env-computed name disagreeing with the resolved target - each with the exact
  cleanup command.
- **Add-account sign-out verification is stricter.** It now also requires the
  credential to actually be GONE (not just the identity changed), so a residual
  second Keychain item can never lead to a profile pairing the OLD token with
  the NEW account's identity. Aborts and restores instead.
- **`quota`: a corrupt saved token no longer masquerades as "network down".**
  It is reported per-account ("saved token unusable") and the other accounts
  are still fetched; previously it could abort the whole run with a false
  "could not reach api.anthropic.com".
- **`quota --json` names are clean.** `name` no longer carries the " (active)"
  display suffix (the `active` field already says so) - safe to feed back into
  `swapdex use`.
- **curl is pinned** to /usr/bin/curl when present (PATH fallback otherwise) -
  the same PATH-shadowing discipline as /usr/bin/security - and a non-zero curl
  exit is now always a transport error (a partial body can not be parsed as a
  response).
- TUI: mouse wheel scrolls the doctor/usage/quota panels; `%` (quota) works
  before any profile is saved; a failed quota shows its error in the panel
  instead of rendering blank; a Down-key on an empty open-menu no longer
  underflows.
- README: the "macOS Claude is issue #1" install note was stale (Keychain
  switching shipped in 0.17-0.19); countdown format examples now match the
  actual output ("2h 14m").

### Added
- E2E tests for `quota` against a fake curl (SWAPDEX_CURL fixture hook, like
  SWAPDEX_SESSIONWIKI_JSON): clean JSON names, expired snapshots, and the
  no-false-offline behavior are now regression-locked. Unit tests for the
  countdown/bar renderers and the new keychain verdicts.

## [0.23.0] - 2026-07-10

### Added
- **`swapdex doctor` now diagnoses why a macOS switch "does not stick".** The
  #1 durable cause of "I switched but the old Claude account is still active" is
  a Keychain service-name mismatch: swapdex writes one item while Claude Code
  reads another (the suffixed `Claude Code-credentials-<hash>` that appears when
  CLAUDE_CONFIG_DIR is set). Doctor now shows Claude's real Keychain item(s),
  the one swapdex targets, and - on a mismatch - the exact fix (launch swapdex
  with the same CLAUDE_CONFIG_DIR you launch `claude` with). Read-only, macOS
  only; reuses the existing keychain discovery, so nothing new touches a
  credential. This turns a silent failure into a self-serve, actionable finding.

## [0.22.0] - 2026-07-10

### Fixed
- **CRITICAL: add-account no longer signs you out of your other accounts.**
  0.19.0 made the add-a-new-account flow run `claude auth logout` to clear the
  macOS Keychain. That command REVOKES the OAuth token server-side, which killed
  the snapshot swapdex had just saved for the current account - and, because the
  refresh token is shared, could invalidate every saved profile for that
  account. The result was "all my logged-in accounts got signed out". Sign-out
  is now LOCAL only (clear the Keychain item + credential file, exactly what
  claude-swap and Symbioose do) - it never revokes, so a saved login is always
  restorable. A regression test asserts swapdex never invokes `claude auth
  logout` and that the previously-saved profile's token survives an add-account.

  If accounts were already signed out: re-login each once (`claude`, then
  `/login`), then `swapdex add <name> --update` to re-save the fresh token.
  Normal `swapdex use` between saved accounts never had this problem.

## [0.21.0] - 2026-07-10

### Added
- **`swapdex quota` - remaining balance per Claude account.** The one opt-in
  network command: it reads each account's remaining 5h/7d quota (and per-model
  weekly windows) from Anthropic's official OAuth usage endpoint, using that
  account's *own* access token. Read-only, and it spends zero message quota. The
  active account is always live; a saved account whose token has expired reports
  so rather than showing a stale number - swapdex still never refreshes tokens,
  which is the line between a switcher and a rotator. Also in `swapdex ui` under
  the `%` key, and `swapdex quota --json` (which includes the raw response for
  any unexpected shape).

### Changed
- The "no network, ever" claim is now stated precisely: the switcher has no HTTP
  client in its dependency graph (still CI-asserted) and never touches the
  network; the new opt-in `quota` command shells out to `curl` to read your own
  balance and is the sole, hand-invoked exception. README and the network badge
  updated to say so honestly.

## [0.20.0] - 2026-07-10

### Fixed
- **No more constant "expired".** Claude access tokens live ~1h and Claude
  Code refreshes them silently, but swapdex flagged every saved Claude
  profile `(expired)` the moment the access token lapsed. The marker (and
  the switch-time warning) now fire only for a snapshot older than 30 days,
  whose refresh token may actually be dead - matching Codex/Gemini/Antigravity.
- **Opening a conversation offers only the tools the account has.** A
  Claude-only profile no longer shows Codex/Gemini/Antigravity; a single-tool
  switch goes straight to that tool's folder browser. The session list also
  falls back to any-account when none are attributed (so the menu isn't
  empty), and the sessionwiki lookup timeout is 2s -> 5s.

### Added
- **Usage in the UI** (press `u`): tokens used per account, read locally.
  Labelled honestly - swapdex is no-network, so this is tokens USED on this
  machine, not the vendor's remaining quota.

## [0.19.0] - 2026-07-08

### Fixed
- **Adding a new Claude account now works on macOS.** The flow tried to clear
  Claude's Keychain item with an external `security` call, which is not
  ACL-authorized to do so reliably - so Claude stayed signed in and dropped
  you back into the same session. swapdex now uses Claude Code's own
  non-interactive auth commands: `claude auth logout` to sign out (Claude
  holds the Keychain ACL, so it actually clears the token) and `claude auth
  login` to sign in (just the OAuth step, no workspace-trust detour). Direct
  file/Keychain cleanup stays as a fallback for older Claude builds. Same on
  Linux/WSL.

## [0.18.1] - 2026-07-08

### Fixed
- **macOS Claude add-account: target the real Keychain item, and verify the
  sign-out.** swapdex now discovers Claude's Keychain item first (preferring
  the hash-suffixed entry - the real credential - over a bare stray) rather
  than trusting a computed name, since swapdex may not see the same
  `CLAUDE_CONFIG_DIR` the user launches `claude` with. And after the local
  sign-out the add-account flow verifies the account is actually cleared; if
  swapdex couldn't clear the Keychain it aborts with guidance and restores,
  instead of opening Claude straight back into the same session.

## [0.18.0] - 2026-07-08

### Changed
- **macOS Claude Keychain, done right** (from decompiling Claude Code's own
  bundle and reading the mature switchers). The Keychain service name is now
  COMPUTED exactly as Claude Code computes it - `Claude Code-credentials`
  plus a `-sha256(CLAUDE_CONFIG_DIR)[..8]` suffix when that env var is set -
  so swapdex targets the right item even when `CLAUDE_CONFIG_DIR` is set (the
  case that hardcoding tools get wrong), with runtime discovery as a
  fallback. All Keychain calls go through `/usr/bin/security` (the same
  binary Claude used to create the item, so its ACL already trusts it - no
  "Always Allow" prompt), target the item by account (`$USER`), and pass the
  token as hex over stdin so it never appears in `ps`. Linux/WSL unchanged.

## [0.17.2] - 2026-07-08

### Fixed
- **macOS Claude Keychain: target the item by account, not service alone.**
  Reading/deleting Claude's Keychain credential matched by service name
  only, so a stray bare `Claude Code-credentials` item (an older swapdex may
  have written one) could be hit instead of Claude's real item, leaving
  Claude logged in. Read and delete now pass `-a <account>` (the item's own
  account, else `$USER`) to target exactly Claude's credential, and delete
  also clears a distinct stray. Confirmed against Anthropic's auth docs and
  the community switchers: the macOS credential is the Keychain item plus
  the `oauthAccount` block in `~/.claude.json`, and `CLAUDE_CONFIG_DIR` does
  not isolate it on macOS - a Keychain swap (what swapdex does) is correct.

## [0.17.1] - 2026-07-08

### Fixed
- **macOS Claude Keychain: use the REAL service name.** 0.17.0 assumed the
  Keychain service was exactly `Claude Code-credentials`, but Claude's item
  has a per-install hash suffix (e.g. `Claude Code-credentials-5953ba74`), so
  swapdex operated on the wrong item and Claude stayed signed in. The service
  name is now discovered at runtime from the login keychain's attributes
  (no password prompt) and read/write/delete target it. On first access
  macOS will ask to allow swapdex to read the item - choose "Always Allow".

## [0.17.0] - 2026-07-08

### Added
- **Claude Code account switching on macOS** (issue #1). Claude on macOS keeps
  its login in the login Keychain rather than a file, so swapdex previously
  refused to switch it there. The Claude adapter now reads and writes the
  Keychain (via `security`): `capture` reads the token from the file or the
  Keychain, `apply` writes it to both plus the `.claude.json` identity with
  all-or-nothing rollback, and the add-a-new-account flow deletes the Keychain
  item so Claude prompts a fresh sign-in. Linux and WSL are unchanged
  (file-based); the Keychain code is a no-op off macOS.

## [0.16.3] - 2026-07-08

### Fixed
- **A left-open `swapdex login` no longer locks the whole store.** The
  add-a-new-account flow held the store lock across the interactive tool
  sign-in (which can take minutes or be left open), so while it was open
  every other operation - rename, use, restore - failed with "another
  swapdex is mid-switch". This was the macOS "rename doesn't work" report:
  a half-finished login had permanently locked the store. The lock now
  covers only the store writes and is released during the sign-in. The busy
  message also names the likely cause.

## [0.16.2] - 2026-07-08

### Fixed
- **Renaming in the UI now mutates the store directly** instead of shelling
  out to a `swapdex rename` subprocess. The subprocess resolved the binary
  via `current_exe()`, which can misbehave under some installs/wrappers and
  make the rename a silent no-op while the UI still refreshed and looked
  fine. It now renames in-process with the same validation, lock, and
  collision check as the CLI.

## [0.16.1] - 2026-07-08

### Fixed
- **Adding a new account that signs you back into the SAME one.** swapdex
  removes the local login and opens the tool, but it cannot make the tool's
  OAuth show an account picker - with a live browser session, the tool signs
  you straight back into the same account. The old flow printed a note but
  still saved that account under the new name, leaving a duplicate profile
  and no actual new account. Now it saves nothing under the new name,
  restores the login as it was, and explains per-tool how to reach the other
  account (sign out at claude.ai / chatgpt.com, or pick the other Google
  account) - printed both up front and if it happens.

## [0.16.0] - 2026-07-08

### Changed
- **Opening a new conversation is now a folder BROWSER, not a text field.**
  You no longer type or memorize a path: each level lists its
  subdirectories, Enter/Right descends, Left/Backspace (or the `..` row)
  goes up, a `~ (home)` row jumps home, and `> open here` launches the
  conversation in the current directory. Fully mouse-driven too - scroll,
  click a folder to enter it, click "open here" to launch. Dotfiles are
  hidden and the current path is shown in the title.

## [0.15.0] - 2026-07-08

A full UI overhaul, by user request: the picker is now a designed interface,
not a plain list.

### Added
- **A logo header.** The two-tone `swapdex` wordmark (violet SWAP + dimmed
  dex - the same mark the CLI prints) crowns a rounded, violet-titled panel.
  The active profile shows a filled dot, plan tier and warnings are
  colour-coded, and the key hints render the keys in violet. The logo drops
  automatically on short terminals so the list keeps its room.
- **Every feature is reachable in the UI now**: `n` renames the selected
  profile, `?` opens a read-only `doctor` health panel (with a "checking..."
  frame so it never looks frozen), alongside the existing switch / open /
  add / restore / delete.
- **Onboarding.** An empty store opens a welcome screen that detects the
  tools you're already logged into and offers to save them as your first
  profile with one key (`s`). A bare `swapdex` opens this for a
  fresh-but-logged-in user too.
- **Mouse.** Scroll to move the selection, click a menu item to choose it,
  click a profile row to select it (Enter still performs the switch, so a
  stray click never switches by surprise).

Every UI action runs the same subprocess command path as the CLI, so there
is still exactly one implementation of each.

## [0.14.0] - 2026-07-08

Three more lenses (a threat-model security audit, a model-based random-walk
soak, and a distribution-surface pass) plus a direct user report.

### Changed
- **A bare `swapdex` on an interactive terminal now opens the picker** when
  you have saved accounts, instead of printing a banner that flashes and
  returns (which read as "it opened and closed"). Pipes, dumb terminals,
  and fresh machines still get the banner + hints, and a bare run never
  creates the store.

### Fixed
- **Security (symlink escape):** a symlinked `accounts/<name>` or store
  directory could redirect a credential write OUTSIDE the 0700 store - the
  symlink refusal only checked the final path component. Every store
  read/write now verifies each component under the store root.
- **Security (MCP):** the read-only MCP server no longer reflects an
  attacker-controlled tool/method name back into its JSON-RPC error text.
- Declining the `add --update` repoint prompt printed "not logged in to any
  selected tool" and exited 3 - a lie; it now says nothing was saved
  because you declined, and exits 0. (Found by the soak.)
- The CI "no network" guard is broadened from 5 HTTP-client names to also
  fail on tokio/rustls/native-tls/openssl/socket2/hickory/quinn/h2, so a
  future socket-capable dependency can't slip the "100% local" promise.

### Verified by the security audit (no changes needed)
- The usage cache holds no token text (only ids/timestamps/counts); error
  messages are secret-free even when the token itself is malformed; the MCP
  server is strictly read-only and exposes no token, uuid, or path; the
  atomic temp file is created 0600 with no widening window; `ensure_not_root`
  guards every credential-mutating entry point.

## [0.13.0] - 2026-07-08

Four new audit lenses (upgrade compatibility, environment torture, parser
fuzzing, docs-vs-behavior contracts) plus real-machine profiling.

### Performance
- **`usage` on a heavy machine: ~20s -> ~0.5s.** A heavy week holds ~1GB of
  transcripts inside the 7-day window; usage reparsed all of it every run.
  Files are now parsed once into a per-file events cache (keyed by
  mtime+size, pruned to the window, atomic 0600) and cache misses parse
  across up to 8 threads. Cached and uncached outputs are byte-identical.

### Fixed
- **A future-stamped backup no longer hijacks `restore`.** One switch under
  clock skew (NTP jump, VM resume) wrote a backup stamp that shadowed every
  real backup forever - restore could silently no-op or restore a stale
  THIRD account, and the ghost survived pruning. Stamps more than an hour
  in the future now sort as the oldest everywhere.
- An unwritable store says so ("store is not writable: ...") instead of the
  unwinnable "another swapdex is mid-switch; try again"; doctor-adjacent
  lock errors are distinguished from real contention.
- A legacy all-whitespace profile (0.2.x allowed creating them) is
  manageable again - the whitespace rule moved to creation time, like the
  `-` reservation, so `rm`/`rename`/`use` still work on it after upgrade.
- Two separate invocations inside one wall-clock second no longer collide
  in `restore`'s last-switch scoping: timeline events carry a
  per-invocation discriminator (legacy events fall back to ts grouping).
- `TERM=dumb` (or empty) on a real terminal gets the plain numbered prompt
  instead of raw ANSI escapes.
- The MCP server's oversized-line resync is constant-memory - a 200MB
  no-newline request used to allocate 200MB just to skip it.
- Seven doc/string drifts from the contract audit (76 contracts verified
  OK): the ui pipe-fallback claim, exit-code rows 2 and 3, the backup
  guarantee's unreadable-live exception, the ui --help text, the two-tool
  top help/banner, and the status sample's missing tier.

### Verified (no changes needed)
- Upgrade compatibility is fully clean: stores created by 0.2.1 / 0.5.0 /
  0.9.2 read perfectly (and 0.12-created stores read back on old binaries);
  timeline compaction stays bounded through 2,200 events; backups stay at
  2 per tool.
- Fuzzing: 890 mutants / ~3,000 invocations across all four credential
  parsers, store snapshots, timeline, native session files, MCP JSON-RPC,
  and every --json output - zero panics, zero hangs, zero secret leaks,
  zero wrong-account results.

## [0.12.1] - 2026-07-08

A delta audit on the bug-sweep itself (fixes breed bugs) plus the last
"observation" items.

### Fixed
- **The login repoint guard could be bypassed** when the target profile's
  saved snapshot was unreadable - corrupt and absent were conflated, so a
  corrupt snapshot let the new sign-in silently overwrite the profile. An
  unreadable snapshot now counts as "different" and asks.
- **Refusing a repoint no longer discards your completed sign-in.** You get
  to save the NEW account under a different name; only skipping that
  explicitly discards it, and the message now says so honestly (the old one
  claimed "keep both accounts" while destroying one).
- The interactive sign-in also rides out **SIGQUIT** (Ctrl+backslash), not
  just Ctrl+C.
- Ghost profile dirs (no known tools; hidden from `ls`) are treated
  consistently by `rename`: not a valid source (exit 5), and colliding with
  one as target is a clean "already exists" (exit 6, was a hard error).
- `usage` prints an honest note when gemini/antigravity are logged in -
  those CLIs keep no local token transcripts, and silence must not read as
  zero usage.
- setup skips a tool whose login cannot be read instead of aborting the
  whole wizard; the login flow's keep-name suggestion falls back to `main`
  when no email exists on disk; the ui shows what to do after the last
  profile is deleted instead of an empty box.

## [0.12.0] - 2026-07-07

The bug-sweep release: three independent adversarial audits (a fresh-user
walkthrough of every command, a logic review of the newest code, and the
add-a-second-account journey run for each tool) plus the unified login flow.
24 defects fixed, each with a regression test.

### The big ones
- **Adding a second account now truly works for ALL four tools.** The
  save-current / sign-out / fresh-sign-in / capture flow existed only for
  Claude; gemini and antigravity dead-ended in guidance whose instruction
  saved the WRONG account under the new name, and codex's "already logged
  in" no-op did the same silently. One tool-generic flow now, with
  automatic restore on any failure - including a shell Ctrl+C mid-sign-in,
  which used to leave you signed out of everything.
- **A corrupt live ~/.claude.json is diagnosed as such** - previously every
  switch blamed the profile snapshot, both suggested remedies failed, and
  doctor said everything was ok.
- **Multi-tool switches no longer abort on the first failing tool** - the
  others proceed and a summary names what failed (exit 1).
- **Enter-through setup saves all four tools** - the "replace it?" prompt
  silently skipped every tool after the first.
- **The ui no longer panics after deleting the last profile.**

### Also fixed
- login guards repointing an existing profile to a different account, and
  rejects the reserved name `-`; non-TTY login-while-logged-in exits 3.
- rename rewrites timeline attribution (usage/sessions no longer report a
  dead profile name forever).
- Multi-tool ls/ui prefer Claude's real plan tier over antigravity's
  auth_method; Antigravity saves print an honest "cannot confirm WHICH
  Google account" note (no identity exists on disk).
- doctor checks live credential file permissions for all four tools, its
  backups/tools lines cover all four, and it diagnoses corrupt
  .claude.json by name.
- A `use` typo prints one line; ls hides crash-debris dirs and unknown
  tool subdirs; whitespace-only names are rejected; the invalid-name
  message states the real rules; fresh-install apply failures clean up
  the half-written file; bare `~` expands in folder prompts; native
  session titles no longer drop real prompts starting with `<`.

## [0.11.0] - 2026-07-07

Deep account dig, round 2: the rotation invariant ("a profile always holds
this account's newest known login") now holds on EVERY path that touches the
live login, and a profile's identity can no longer change silently.

### Fixed
- **`restore` refreshes the outgoing account's profile** with its latest
  (possibly rotated) tokens before undoing a switch - the same stale-token
  fix 0.10.0 gave `use`.
- **A no-op `use` is now a sync point**: switching to the already-active
  profile refreshes its snapshot from the live login (tokens rotate while
  you work). No backup and no timeline event - nothing is switching.
- **`add --update` no longer silently repoints a profile** to a different
  account. Logged into B while updating a profile that holds A: on a
  terminal it asks; non-interactively it refuses with exit 7 and shows both
  the keep-both and the explicit-repoint commands. Same-account updates
  (the documented stale-token refresh) pass through unchanged.

## [0.10.0] - 2026-07-07

A deep dig into account handling itself.

### Fixed
- **Stale-profile token rotation** - the deepest account bug a switcher can
  have. Providers ROTATE refresh tokens while an account is in use, so a
  profile snapshot goes stale the moment you work on that account; switching
  away and back later could restore a refresh token the provider had already
  revoked, forcing a re-login and making the switch look broken. Now `use`
  (and the `login` flow's stash) write the outgoing live capture - the
  freshest known tokens - back into EVERY profile holding that account
  before switching. A profile now always means "this account's newest known
  login", not "the login as of the day you saved it".
- **Store permissions self-tighten.** Snapshots are tokens, and doctor's
  store check only looked at the top-level directory - `cp -r`, backup
  tools, or a loose umask could leave a world-readable token file inside
  unnoticed. Opening the store now walks it and re-tightens every dir to
  0700 and every file to 0600, best-effort, on every command.

### Verified in the same dig (no changes needed)
- Symlinked credential files are refused with a non-zero exit.
- Two profiles holding the same account both stay fresh under the new
  rotation rule; the active marker points at the first match.

## [0.9.2] - 2026-07-07

Another angle-testing round as a user (tiny terminals, Unicode names, wrong
keys, error paths, full journeys through a pty). Four fixes.

### Fixed
- **Ctrl+C now quits the ui** from any screen. Raw mode swallows the signal,
  so the key was silently ignored - and it is the first key a user in
  trouble reaches for.
- **setup's "add another account" step asks WHICH tool** (all four) and runs
  the same one-flow login. The old block was Codex-only - the root of "it
  keeps asking about Codex accounts" in real use.
- setup's intro line names all four tools, not "Claude Code / Codex".
- `login` without `--tool`: a wrong number at the tool question re-prompts
  instead of silently cancelling.

### Verified in the same round (no changes needed)
- 4-line terminals render without panicking; Unicode/CJK profile names align;
  `--open`/`--dir` error paths exit non-zero with clear messages; the full
  ui add-account journey returns to the picker with the new profile active.

## [0.9.1] - 2026-07-07

### Fixed
- Esc in the folder prompt goes back ONE step (to the conversation menu),
  not two - a double-tapped Esc could accidentally quit the whole ui.
  Found by driving the ui end-to-end as a user through a pty.

## [0.9.0] - 2026-07-07

Two more real-use asks, same day.

### Added
- **Sessions without sessionwiki.** The post-switch menu now reads recent
  sessions STRAIGHT from each tool's own store (`~/.claude/projects`,
  `~/.codex/sessions`) when sessionwiki is absent - titles from the first
  user message, resume via the tool's native mechanism (`claude --resume
  <id>` in the session's own folder, `codex resume <id>`). A session's
  recorded cwd is only trusted when it exists as a real local directory.
  sessionwiki, when installed, still provides the richer cross-tool view.
- **The ui stays up** (ccusage-style): one persistent full-screen session.
  Switching shows its condensed result in the status line and refreshes the
  list in place; `o` opens the conversation menu for the selected profile
  (recent sessions + new-conversation entries with an in-UI folder prompt);
  Esc returns to the list. Opening a conversation is the one action that
  leaves - that is the point of a switch. Internally a switch runs this same
  binary as a subprocess, so there is still exactly one switching
  implementation.

## [0.8.0] - 2026-07-07

### Added
- **Switch, land in a conversation.** The post-switch menu now opens the tool
  itself: pick a recent session by number to resume it in its own folder
  (via sessionwiki), or `c`/`x`/`g`/`a` to open a NEW claude/codex/gemini/agy
  conversation - it asks which project folder (Enter keeps the current one,
  `~` expands). And `swapdex use <name> --tool claude --open [--dir <path>]`
  does switch-and-launch in one command. Real-use feedback: switching is not
  done until the conversation is open.

## [0.7.0] - 2026-07-07

Real-use feedback release: the three things that actually hurt.

### Added
- **Add a NEW account in one flow**: `swapdex login <name> --tool claude`
  while already logged in now does the whole thing - saves your current
  login (profile + store backup), signs you out locally, opens Claude Code
  for the fresh sign-in, and captures the new account. If the sign-in does
  not complete, your previous login is restored automatically; it can never
  be lost. (Previously this case printed instructions and stopped - the
  single most-hit wall in real use.)
- **Full-screen `ui`** on a real terminal: arrow keys, Enter to switch, `a`
  add a new account, `r` restore, `d` delete (with confirm), `q` quit -
  the llmux-style experience, by direct request. Every action runs the
  exact same command path as the CLI; piped stdin falls back to the plain
  numbered prompt. (ratatui with the crossterm backend only; the "no HTTP
  client in the dependency graph" guarantee is unchanged.)

### Changed
- `login` without `--tool` ASKS which tool instead of silently preferring
  Codex when it is installed - the old guess kept steering Claude users to
  the wrong tool.
- Tool ordering everywhere (setup, ls, status, doctor) leads with Claude
  Code, then Codex, Gemini, Antigravity.

## [0.6.0] - 2026-07-07

### Added
- **Antigravity support** (Google's agentic CLI, binary `agy`): its token at
  `~/.gemini/antigravity-cli/antigravity-oauth-token` is a fourth switchable
  tool - one profile can hold Claude Code + Codex + Gemini + Antigravity and
  a single `use` switches all four. No email or account id is stored on disk,
  so the profile match uses a one-way fingerprint of the refresh token (a
  fresh re-login honestly degrades to "not saved" until you re-add).

### Changed
- Gemini's `ls` marker is `stale` (snapshot refreshed >30 days ago, like
  Codex) instead of `expired`: Gemini access tokens live about an hour and
  the CLI refreshes them silently, so "expired right now" was pure noise.

## [0.5.0] - 2026-07-07

Two headline features: a third tool, and per-account usage.

### Added
- **Gemini CLI support**: `~/.gemini/oauth_creds.json` +
  `~/.gemini/google_accounts.json` are switched together with the same
  both-or-neither rollback the Claude adapter uses. One profile can now hold
  Claude Code + Codex + Gemini and a single `use` switches all three;
  `--tool gemini` scopes any command; `ls`/`status`/`ui`/`doctor`/`restore`
  cover it like the others; sessionwiki's account badges pick Gemini sessions
  up automatically (the timeline join is tool-generic). `--tool all` is the
  explicit everything-selector (alias `both` kept for scripts).
- **`usage` is per-account once a switch history exists**: every token event
  is attributed to the profile active at its timestamp - the same honest join
  `sessions` uses - so "how much have I used on EACH account" finally has an
  answer. What predates your first switch shows as untagged; no history, no
  guessing. JSON grows an `accounts` object per tool.

## [0.4.2] - 2026-07-07

Ecosystem-walkthrough fixes: the integrated flows, from a fresh user's chair.

### Fixed
- The `ui` resume handoff passes `--no-sync`: on a large store the exec used
  to kick off a full index sync - minutes of progress spam that looked like a
  hang in the flagship flow. sessionwiki still self-syncs when the id is not
  yet indexed.
- A present-but-never-synced sessionwiki no longer reads as "0 sessions":
  `sessions` and `status` say "index empty - run `sessionwiki sync` once".
- The sessionwiki read cap rose 1000 -> 50000, so the status summary cannot
  silently understate a large store.

### Added
- `sessions --json`: {"available", "accounts", "total"} for scripting
  (available=false distinguishes "no sessionwiki" from "zero sessions").

## [0.4.1] - 2026-07-07

Fixes from an adversarial audit of the 0.4.0 delta.

### Fixed
- `ui` no longer panics on a session id with multibyte characters (the id
  prefix was a byte slice; now char-based).
- The "any account" continuity fallback now fires on the FIRST real switch -
  the very case it was written for. (The empty-timeline check ran after the
  switch had already written its own event, so it only ever fired on a no-op
  pick.)
- `exec` handoff passes the session id after a `--` separator, so an id that
  begins with `-` can never be parsed as a flag.
- The `SWAPDEX_SESSIONWIKI_JSON` test fixture hook is only honored together
  with `SWAPDEX_ROOT` - a stray env var can no longer redirect a production
  run.

### Docs
- The README demo now shows the full integrated loop: `ui` -> switch ->
  recent sessions -> resume handoff -> `status --short`.

## [0.4.0] - 2026-07-07

### Added
- `ui` completes the loop: pick a recent session by number after the switch
  and swapdex hands off to `sessionwiki resume <id>` directly (a one-shot
  `exec` of the official reopen flow - the session's own tool takes over the
  terminal). Enter skips; nothing ever launches unasked. This is the same
  precedent as `login` driving the official sign-in: an explicit hand-off is
  not a wrapper - swapdex `exec`s and is gone.

## [0.3.1] - 2026-07-07

### Added
- `ui` shows a continuity hint after the switch: the picked account's recent
  sessions (id, relative age, tool, title) with the one command to reopen one
  (`sessionwiki resume <id>`) - switch, land back in the work you switched
  for. Before the first recorded switch, when nothing can be attributed yet,
  it honestly falls back to the most recent sessions of any account and says
  so. Requires sessionwiki; silently absent otherwise.

## [0.3.0] - 2026-07-07

### Added
- `swapdex ui`: an interactive picker - every profile with its account,
  active marker, and the session summary; type a number to switch, Enter or
  `q` cancels. The selection runs the exact same safe `use` path (backup,
  validate, atomic apply), so a human picking a number IS the explicit
  switch - the no-auto-rotation bright line is untouched. Deliberately
  stdin-only: no raw-mode TUI crate, nothing socket-shaped enters the
  dependency graph.

## [0.2.2] - 2026-07-07

### Fixed
- `ls` aligns by display width, so a CJK profile name (two columns per
  character) no longer shears the table.

### Docs
- The `status --short` line drops straight into Claude Code's own status line
  (`statusLine` snippet in the README) - the active account stays visible
  inside the tool you are switching.
- An honest Alternatives section (claude-swap, aisw, caam) with each
  project's trade-offs and when to pick them over swapdex.

## [0.2.1] - 2026-07-06

Fixes from an adversarial audit of the 0.2.0 delta, plus scripting/completion
polish.

### Fixed
- `use ""` (an unset shell variable) matched a single profile as a "unique
  prefix" and performed a real switch; an empty name is now rejected (exit 2)
  with the live login untouched.
- `use -` can no longer re-pick the profile you are already on when the live
  identity is unreadable (the newest switch's destination is excluded); the
  refusal message says the real reason when both profiles are active; and
  `--tool` now scopes the `-` resolution.
- macOS Keychain-mode installs: a bare `use`/`restore` skips claude-code with
  a note and keeps switching Codex, instead of aborting the whole command.
- `doctor`: the store-permission check could never fire (the store self-heals
  its mode on open) - it now reports what it found; the expired/stale remedy
  says "log in to that account" first, so following it verbatim can no longer
  overwrite the profile with whatever account happened to be live.
- `rm` checks the profile exists before asking y/N.
- `manpage` failures exit 1 instead of printing nothing successfully.
- A legacy profile literally named `-` stays manageable after the upgrade
  (`-` is rejected only when creating/renaming).
- A bare `swapdex` no longer creates the store directory as a side effect.

### Added
- `ls --names`: bare profile names one per line; the docs gain a verified
  bash/zsh snippet that tab-completes profile names for `use`/`rm`/`rename`.
- `add` with no name asks on a terminal (name suggested from the live
  account); non-interactively it errors with the fix.
- `doctor` verdicts are colored on a TTY; NO_COLOR is respected everywhere.
- The demo GIF shows `use -`, `status --short`, and the colored doctor.

## [0.2.0] - 2026-07-06

Daily-driver ergonomics: the goal is a switch in two keystrokes and zero
guessing about where you stand.

### Added
- `swapdex use -`: toggle to the previous/other profile, like `cd -` /
  `git switch -`. With two profiles it is simply the other one; with more it
  is the profile you were on before (from the switch timeline). `-` is now a
  reserved name.
- Unique-prefix matching on `use`: `swapdex use w` resolves to `work` and says
  so; an ambiguous prefix refuses and lists the candidates (switching is a
  write - it never guesses).
- `swapdex status --short`: one compact `claude:work codex:personal` line for
  shell prompts and statuslines (starship/PS1 snippet in the README).
- A bare `swapdex` now shows the active accounts under the banner, so the
  naked command answers "where am I?".

### Changed
- `rm` asks y/N on a terminal instead of demanding `--yes`; scripts keep the
  explicit `--yes` requirement (exit 7 when stdin is not a tty).

## [0.1.9] - 2026-07-06

### Added
- `swapdex manpage`: prints the man page (roff) to stdout. Homebrew installs
  it - and shell completions - automatically.
- A demo GIF of the core loop (ls -> use -> status -> restore -> doctor) at
  the top of the README.

### Fixed
- `use`/`restore` no longer print the running-session warning under
  `SWAPDEX_ROOT`: an isolated root's credentials are not the ones a live
  session uses, so the warning was a false positive there.

## [0.1.8] - 2026-07-06

### Added
- `swapdex doctor`: local health check - store permissions, every saved
  snapshot (unreadable/expired/stale), both live logins (including the
  corrupt-file case), backups, `.claude.json` permissions, and the CLIs on
  PATH. Each finding ends with its remedy. Exit 0 healthy, 9 when problems
  were found. Local only, never the network.

### Changed
- The switch timeline file is bounded (compacts to the newest 1000 events)
  instead of growing forever.
- `add` hints about quoting when a profile name contains spaces.

## [0.1.7] - 2026-07-06

Findings from a two-track review (adversarial code audit + a new-user
walkthrough), all fixed and regression-tested.

### Added
- `swapdex restore [--tool ...] [--dry-run]`: put back the login that was live
  before the last switch. `use` has always backed up the outgoing login (even
  one never saved as a profile), but there was no command to bring it back - a
  bad switch meant hand-copying files. `restore` backs up the current login
  first, so running it again toggles between the two. A bare `restore` scopes
  itself to the tool(s) the last switch touched, and it skips a torn backup in
  favor of an older intact one.
- `use` warns when the OUTGOING login is not saved as any profile (only the
  last 2 backups remember it), and when a live session of the switched tool is
  running.

### Fixed
- `usage` was wrong in both directions: Codex was undercounted 10-100x (it
  read the per-request `last_token_usage` as if it were cumulative; now it
  windows the deltas of the monotonic `total_token_usage` by event time) and
  Claude was overcounted ~2.5x (one line per content block repeats the same
  `message.id`, and resumed sessions copy messages into new files; now deduped
  by message id). Also ~9x faster (streaming + pre-filter instead of
  whole-file reads; 12.2s -> 1.3s on a 927MB transcript set).
- A corrupt live credential file no longer blocks recovery: `use <profile>`
  warns and replaces it (previously it aborted - the one command that could
  fix the file refused to run), `status` reports "login file unreadable" per
  tool instead of dying mid-output, and `restore` tolerates it too.
- macOS: `use`/`add`/`restore` on a Keychain-mode Claude Code install now
  refuse with an explanation instead of half-switching (writing a credentials
  file the CLI ignores while flipping the reported identity). `status` and
  `add` explain the Keychain situation. Codex switching works on macOS.
- `rename` to an existing name exits 6 ("already exists", like `add`) instead
  of a generic hard error, and takes the store lock like every other mutation.
- `login <name> --tool claude` with no CLI on PATH exits 3 on stderr (was:
  exit 0 on stdout - scripts saw success where nothing was saved).
- A corrupt saved snapshot is now visible as `(unreadable)` in `ls` with a
  remedy footer, and a failing `use` names the profile and the fix.
- Claude apply: if the rollback after a failed config write ALSO fails (e.g.
  disk full), the error now says so instead of claiming a clean rollback.
- `restore` attributes its timeline event to the restored profile's name, so
  `sessions` no longer blames an account literally named "(backup)".
- setup: Ctrl-D (EOF) at a prompt exits cleanly instead of spinning forever.
- Timestamps with fractional seconds AND a numeric timezone offset
  ("...00.123+09:00") now parse the offset instead of ignoring it.

### Changed
- `use` prints "{tool}: profile 'x' has no {tool} login - left unchanged" when
  a logged-in tool is skipped, instead of silently half-switching; `--dry-run`
  shows the target account's email.
- `ls` aligns by characters (not bytes) and truncates over-long names/emails
  with an ellipsis so one long row cannot shear the table (full values in
  `--json`).
- `status --json` has a stable shape: every key present on every row (null
  when unknown) plus an `unreadable` flag.
- Parent directories swapdex creates for credential files (e.g. a fresh
  `~/.codex`) are 0700, not umask-default.

## [0.1.6] - 2026-07-06

### Added
- `swapdex usage [--json]`: recent local token usage per tool over the last 5h
  and 7d, summed from `~/.claude` and `~/.codex` session logs. A machine-wide
  activity gauge (not tagged by account, not the billed quota) so you can tell
  when to switch. Reads local files only - never the network, keeping the
  switcher-not-rotator stance intact.
- `use` now warns (best-effort) when the tool being switched has a live session
  running: a running session holds the old token and can overwrite the login
  you just switched to on its next refresh, so it prints a note to restart it.
  Detection is an exact process-name match (never a false alarm from a stray
  path), local only.

## [0.1.5] - 2026-07-06

### Changed
- `swapdex login --tool claude` now drives the flow instead of only printing
  guidance: if you are not logged in it opens Claude Code so you can sign in and
  auto-captures the result; if you already are, it guides the add/switch step.
  (Claude Code has no login subcommand, so a re-login to a different account is
  done inside the app.)

## [0.1.4] - 2026-07-06

### Added
- Guided onboarding: `swapdex setup` (interactive wizard - saves the accounts
  you are logged into, offers to add more, shows how to switch) and
  `swapdex login <name>` (log in and save in one step). The empty state and the
  no-argument banner now point new users to `swapdex setup`.

### Fixed
- `login`/`setup` back up the current Codex login before running `codex login`
  (which deletes `~/.codex/auth.json`), so an interrupted login is never lost.

## [0.1.3] - 2026-07-04

### Fixed
- `ls`, `status`, and the MCP `list_accounts` track the active account per tool
  (`active_tools`), fixing a mixed cross-tool state that marked both profiles
  active with a bare `*`.
- Removed the dead `active.json` hint - the live login drives every marker, so
  this dropped per-switch fsync churn and a corrupt-file surface.
- `ls` uses two-pass column widths and falls back to the tier when an email is
  missing (no stray leading-space `[tier]`).
- `session_link` skips the sessionwiki shell-out under `SWAPDEX_ROOT` so an
  isolated run never reads the host's real sessions.

## [0.1.2] - 2026-07-03

### Fixed
- `--tool` is a strict value set: a typo (`--tool cluade`) is rejected with the
  possible values instead of silently falling through to both tools.
- `use` no longer reports "already active" when the account id is empty, which
  could have kept the wrong account.
- `ls`/`status` inspect all of a profile's tools; Codex identity and the
  stale/expired marker were previously hidden behind the alphabetically-first
  `claude-code`.
- `add` (default) attaches a newly-available tool without forcing `--update`.

### Added
- The npm package ships its README (was blank on npmjs).

## [0.1.1] - 2026-07-03

### Added
- Shell completions: `swapdex completions <bash|zsh|fish|...>`.
- `status --json` for scripting.
- `ls` marks a saved login `(expired)` (Claude) or `(stale)` (Codex) so you know
  to re-capture it.

## [0.1.0] - 2026-07-03

### Added
- Initial release. Switch between multiple Claude Code and Codex login accounts
  locally: `add`, `use`, `ls`, `status`, `rm`, `rename`, `sessions`, and a
  read-only `mcp` server. In-place credential file swap, hardened for safety
  (0600 files, back-up-then-apply, symlink/root refusal, atomic writes, and a
  build-enforced no-network guarantee). Distributed via crates.io, Homebrew,
  npm, and prebuilt release binaries.
