//! Held actions after the gate's decision (approvals spec §4.6, §4.9): the owner's `/ok`, `/no`,
//! `/cancel` and `/held`, the delays that come due and the expiry sweep on the router's tick, and
//! the restart rule. `gate::decide` holds an action and sends its card; this carries it out
//! exactly as it was stored, once, or tells the agent that it will not run. The router only
//! hands commands and its tick here.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde_json::Value;
use tokio::sync::oneshot;
use zephyr_policy::policy::{Decision, UNKNOWN};

use crate::approvals::{self, Mode};
use crate::config::Config;
use crate::procs::Procs;
use crate::store::{Action, ActionKind, Store};
use crate::tools::not_waiting;

/// How long an approved connector call's one-time pass is good for (spec §4.6).
pub const PASS_SECS: i64 = 30 * 60;

/// What `/ok` answers while the gateway is paused (spec §4.9).
pub const PAUSED: &str = "[zephyr] paused; /resume first";

/// The browser actions waiting in place for the owner (spec §4.4). The gate socket adds one for
/// each `Wait` it holds and blocks the wrapper on its receiver; `/ok` answers `true`, and
/// whatever ends the action otherwise (`/no`, `/cancel`, expiry, `/stop`) answers `false`.
#[derive(Clone, Default)]
pub struct Waits(Arc<Mutex<HashMap<i64, oneshot::Sender<bool>>>>);

impl Waits {
    /// Waits for action `id`'s verdict: `true` lets the browser go on.
    pub fn add(&self, id: i64) -> oneshot::Receiver<bool> {
        let (tx, rx) = oneshot::channel();
        self.lock().insert(id, tx);
        rx
    }

    /// Answers action `id`'s waiting wrapper. Returns whether one was still waiting.
    pub fn answer(&self, id: i64, allow: bool) -> bool {
        let waiting = self.lock().remove(&id);
        waiting.is_some_and(|tx| tx.send(allow).is_ok())
    }

    /// `/stop`: denies every browser action waiting. Returns how many.
    pub fn deny_all(&self) -> usize {
        let all: Vec<_> = self.lock().drain().collect();
        all.into_iter()
            .map(|(_, tx)| tx.send(false).is_ok())
            .filter(|sent| *sent)
            .count()
    }

    /// Denies each wait whose action no longer waits for the owner, whatever ended it (`/no`
    /// settles in `tools::decide`, which has no `Waits`). A store error denies too.
    pub fn deny_settled(&self, store: &Store) {
        let ids: Vec<i64> = self.lock().keys().copied().collect();
        for id in ids {
            let open = matches!(
                store.action(id),
                Ok(Some(a)) if matches!(a.status.as_str(), "pending" | "scheduled" | "running")
            );
            if !open {
                self.answer(id, false);
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, oneshot::Sender<bool>>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// What carrying out an action needs from the router.
pub struct Ctx<'a> {
    pub cfg: &'a Arc<Config>,
    pub store: &'a Store,
    pub procs: &'a Procs,
    pub waits: &'a Waits,
}

/// Who approves an action: the owner on a channel, or its delay running out (the tick).
#[derive(Clone, Copy)]
pub enum By<'a> {
    Owner { channel: &'a str },
    Delay,
}

/// `/ok N [once]`, and a delay that came due: carries out held action `id` exactly as it was
/// stored, once (`take_action` settles it in one statement, so of two `/ok` only one runs it).
/// `Ok` is the answer once it was taken (the owner's reply, or for a delay the notice its owner
/// gets), whether it started or not; `Err` the answer when it still waits: the gateway is
/// paused, a payment needs a strong channel, or every process slot is taken (R44). The owner
/// approving under `ask-new` remembers its targets once it started, unless `once` (R46).
pub fn approve(ctx: &Ctx, id: i64, once: bool, by: By, now: i64) -> Result<String, String> {
    let store = ctx.store;
    let failed = |e: rusqlite::Error| format!("[zephyr] reading proposal #{id} failed: {e}");
    let waiting = |a: &Action| matches!(a.status.as_str(), "pending" | "scheduled");
    let Some(a) = store.action(id).map_err(failed)?.filter(waiting) else {
        return Err(not_waiting(id));
    };
    if store.pause_reason().map_err(failed)?.is_some() {
        return Err(PAUSED.into());
    }
    if let By::Owner { channel } = by {
        let strong = &ctx.cfg.settings.strong_channels;
        let money_strong = approvals::strong_only(ctx.cfg).iter().any(|s| s == "money");
        if a.class == "money" && money_strong && !strong.iter().any(|c| c == channel) {
            return Err(format!(
                "[zephyr] approving a payment needs {}",
                strong.join(" or ")
            ));
        }
    }
    // Before it is taken, so a full process table leaves the approval waiting (R44).
    if a.kind == ActionKind::Run && !store.proc_slot_free().map_err(failed)? {
        return Err(format!(
            "[zephyr] {} commands are running; /ok {id} again when one ends",
            crate::store::MAX_RUNNING_PROCS
        ));
    }
    let Some(a) = store.take_action(id, "running").map_err(failed)? else {
        return Err(not_waiting(id));
    };
    let decision = match (by, once) {
        (By::Delay, _) => "due",
        (_, true) => "once",
        _ => "approved",
    };
    audit(store, &a, decision);
    let started = match a.kind {
        ActionKind::Run => run(ctx, &a),
        ActionKind::Pass => pass(store, &a, by, now),
        ActionKind::Wait => wait(ctx, &a),
    };
    let (n, s) = (a.id, &a.summary);
    Ok(match (started, by) {
        (Ok(what), By::Owner { .. }) => {
            // Only the owner's own approval of what started makes a target known.
            let remembered = match once {
                true => String::new(),
                false => remember(ctx.cfg, store, &a),
            };
            format!("[zephyr] approved #{n} ({s}): {what}{remembered}")
        }
        (Ok(what), By::Delay) => {
            format!("[zephyr] #{n} ({s}) went at the end of its delay: {what}")
        }
        (Err(why), by) => not_run(store, &a, by, &why),
    })
}

/// Under `ask-new` the approved action's targets are known from now on (spec §4.3): what the
/// owner's answer adds, the targets that were new.
fn remember(cfg: &Config, store: &Store, a: &Action) -> String {
    let e = approvals::effective(cfg);
    let mode = match a.class.as_str() {
        "messages" => e.messages,
        "posts" => e.posts,
        _ => return String::new(),
    };
    if mode != Mode::AskNew {
        return String::new();
    }
    let new: Vec<&str> = (a.targets.iter())
        .filter(|t| *t != UNKNOWN)
        .filter(|t| {
            !store
                .all_known(&a.class, std::slice::from_ref(t))
                .unwrap_or(false)
        })
        .map(String::as_str)
        .collect();
    if let Err(e) = store.remember_targets(&a.class, &a.targets) {
        eprintln!("zephyr: remembering #{}'s targets failed: {e}", a.id);
        return String::new();
    }
    match new.is_empty() {
        true => String::new(),
        false => format!("; from now on {} goes without asking", new.join(", ")),
    }
}

/// A shell action: its command again, as the agent, through `procs`, as `exec` runs one (the
/// same log, timeout and vault tokens, the grants checked again now); its end is reported to
/// main on the run's channel. `Ok` says what started, `Err` why it did not.
fn run(ctx: &Ctx, a: &Action) -> Result<String, String> {
    let mut args: Value = serde_json::from_str(&a.payload).unwrap_or_default();
    if !args.is_object() {
        args = serde_json::json!({ "command": a.command() });
    }
    args["secrets"] = serde_json::json!(a.secrets);
    let call = zephyr_policy::tools::exec_call(&args)?;
    if let Decision::Deny(why) = zephyr_policy::policy::check("exec", &args) {
        return Err(format!("the policy refuses it: {why}"));
    }
    let env = match call.secrets.is_empty() {
        true => Vec::new(),
        false => crate::vault::exec_env(ctx.cfg, ctx.store, &call.command, &call.secrets)?,
    };
    let to = origin(ctx.store, a).ok_or("there is no owner to report to")?;
    let p = (ctx.procs).start_action(ctx.cfg, ctx.store, a.id, call, to, env)?;
    Ok(format!(
        "running it as process #{p}; the agent gets its report"
    ))
}

/// A connector action: a one-time pass for exactly that call (server, tool, arguments), good
/// for 30 minutes; the agent is told to make it again.
fn pass(store: &Store, a: &Action, by: By, now: i64) -> Result<String, String> {
    let args: Value = serde_json::from_str(&a.payload).unwrap_or_default();
    let digest = crate::gate::pass_digest(&args).ok_or("the call names no server or tool")?;
    (store.add_pass(&digest, a.id, now + PASS_SECS))
        .map_err(|e| format!("its pass could not be stored: {e}"))?;
    finish(store, a, "done", "a pass for 30 min");
    let (word, what) = match by {
        By::Owner { .. } => ("approved", format!("the owner approved #{}", a.id)),
        By::Delay => ("due", format!("#{}'s delay ended", a.id)),
    };
    let text = format!(
        "{what} ({}): make exactly that connector call again within 30 min and it goes \
         through, once",
        a.summary
    );
    note(store, a, word, &text);
    Ok("the agent may make exactly that call once in the next 30 min".into())
}

/// A browser action: the wrapper waiting on it goes on, if it still waits.
fn wait(ctx: &Ctx, a: &Action) -> Result<String, String> {
    match ctx.waits.answer(a.id, true) {
        true => {
            finish(ctx.store, a, "done", "the browser went on");
            Ok("the browser goes on".into())
        }
        false => Err("the browser is no longer waiting, so nothing was done".into()),
    }
}

/// A taken action that did not start: failed, the agent told why, and the answer for whoever
/// set it in motion.
fn not_run(store: &Store, a: &Action, by: By, why: &str) -> String {
    finish(store, a, "failed", why);
    let (n, s) = (a.id, &a.summary);
    let (agent, owner) = match by {
        By::Owner { .. } => (
            format!("the owner approved #{n} ({s}), but it did not start: {why}"),
            format!("[zephyr] approved #{n} ({s}), but it did not start: {why}"),
        ),
        By::Delay => (
            format!("#{n} ({s}) came due, but it did not start: {why}"),
            format!("[zephyr] #{n} ({s}) came due, but it did not start: {why}"),
        ),
    };
    note(store, a, "not run", &agent);
    owner
}

/// The owner's decision on `a` as an audit row (spec §4.9), beside the gate's own: the action's
/// number and summary, the daemon's words, never its payload.
fn audit(store: &Store, a: &Action, outcome: &str) {
    let tool = match a.kind {
        ActionKind::Run if a.origin.session.starts_with("task-") => "bash",
        ActionKind::Run if a.origin.session.starts_with("job-") => "bash",
        ActionKind::Run => "exec",
        ActionKind::Pass => "mcp_call",
        ActionKind::Wait => "browser",
    };
    let args = format!("#{} {}", a.id, a.summary);
    if let Err(e) = store.audit(&a.origin.session, tool, &args, outcome) {
        eprintln!("zephyr: writing the audit failed: {e}");
    }
}

fn finish(store: &Store, a: &Action, status: &str, result: &str) {
    if let Err(e) = store.finish_action(a.id, status, result) {
        eprintln!("zephyr: recording how #{} ended failed: {e}", a.id);
    }
}

fn note(store: &Store, a: &Action, word: &str, text: &str) {
    if let Err(e) = store.action_note(a.id, word, text) {
        eprintln!("zephyr: telling the agent about #{} failed: {e}", a.id);
    }
}

/// Where action `a` reports: the run that asked, else the owner last answered.
fn origin(store: &Store, a: &Action) -> Option<(String, String)> {
    match a.origin.channel.is_empty() {
        false => Some((a.origin.channel.clone(), a.origin.sender.clone())),
        true => store.last_owner().ok().flatten(),
    }
}

/// `/no N` on a held action: dropped, and the agent told not to try it again (spec §4.6). The
/// router denies a waiting browser action afterwards (`Waits::deny_settled`).
pub fn decline(store: &Store, id: i64) -> String {
    match store.take_action(id, "dropped") {
        Ok(Some(a)) => {
            audit(store, &a, "dropped");
            let text = format!(
                "the owner declined #{id} ({}); don't try it again",
                a.summary
            );
            note(store, &a, "declined", &text);
            format!("[zephyr] dropped #{id} ({})", a.summary)
        }
        Ok(None) => not_waiting(id),
        Err(e) => format!("[zephyr] reading proposal #{id} failed: {e}"),
    }
}

/// `/cancel N`: a delayed action is cancelled before it goes, and the agent is told; a delayed
/// browser action is denied.
pub fn cancel(store: &Store, waits: &Waits, arg: &str) -> String {
    let Ok(id) = arg.trim().trim_start_matches('#').parse::<i64>() else {
        return "[zephyr] usage: /cancel N, for an action waiting out its delay".into();
    };
    match store.action(id) {
        Ok(Some(a)) if a.status == "scheduled" => {}
        Ok(Some(a)) if a.status == "pending" => {
            return format!("[zephyr] #{id} is not waiting out a delay; /no {id} drops it")
        }
        Ok(_) => return not_waiting(id),
        Err(e) => return format!("[zephyr] reading proposal #{id} failed: {e}"),
    }
    match store.take_action(id, "cancelled") {
        Ok(Some(a)) => {
            audit(store, &a, "cancelled");
            waits.answer(id, false);
            let text = format!(
                "the owner cancelled #{id} ({}); don't try it again",
                a.summary
            );
            note(store, &a, "cancelled", &text);
            format!("[zephyr] cancelled #{id} ({})", a.summary)
        }
        Ok(None) => not_waiting(id),
        Err(e) => format!("[zephyr] reading proposal #{id} failed: {e}"),
    }
}

/// `/held`: the actions waiting for the owner or for their delay, newest first.
pub fn held(cfg: &Config, store: &Store) -> String {
    let list = match store.held_actions() {
        Ok(list) => list,
        Err(e) => return format!("[zephyr] reading the held actions failed: {e}"),
    };
    if list.is_empty() {
        return "[zephyr] nothing is held for you".into();
    }
    let tz = crate::scheduler::zone(cfg).unwrap_or(jiff::tz::TimeZone::UTC);
    let lines: Vec<String> = (list.iter())
        .map(|a| match (a.status.as_str(), a.due_at) {
            ("scheduled", Some(at)) => format!(
                "#{} {} · sending at {} unless /cancel {}",
                a.id,
                a.summary,
                crate::settings::clock(at, &tz),
                a.id
            ),
            _ => format!("#{} {} · /ok {} · /no {}", a.id, a.summary, a.id, a.id),
        })
        .collect();
    format!("[zephyr] held for you, newest first:\n{}", lines.join("\n"))
}

/// The router's tick (spec §4.6). Delays that came due go as an `/ok` would, and whoever set
/// each in motion is told how it ended (R45). While the gateway is paused, and on the first tick
/// after a pause lifted by itself (a budget pause at midnight), `held` is set: what came due
/// waits for the owner again instead, never going late on its own (I1). A due command with no
/// free process slot waits for the owner too. Actions left unanswered expire, and the agent is
/// told; a browser wait whose action ended is denied.
pub fn tick(ctx: &Ctx, now: i64, held: bool) {
    let store = ctx.store;
    // A store error counts as paused: nothing goes on a guess.
    let paused = store.pause_reason().map_or(true, |p| p.is_some());
    if held || paused {
        ask_again(ctx.cfg, store, now);
    } else {
        match store.due_actions(now) {
            Ok(due) => due.iter().for_each(|a| due_now(ctx, a, now)),
            Err(e) => eprintln!("zephyr: reading delayed actions failed: {e}"),
        }
    }
    match store.expire_actions(now) {
        Ok(expired) => {
            for a in expired {
                audit(store, &a, "expired");
                ctx.waits.answer(a.id, false);
                let within = match a.kind {
                    ActionKind::Wait => "10 min",
                    _ => "a day",
                };
                let text = format!(
                    "the owner did not answer #{} ({}) within {within}, so it was not run; ask \
                     again if it is still wanted",
                    a.id, a.summary
                );
                note(store, &a, "expired", &text);
            }
        }
        Err(e) => eprintln!("zephyr: expiring held actions failed: {e}"),
    }
    ctx.waits.deny_settled(store);
}

/// Delayed action `a` came due: it goes, and its owner is told how it ended. With every process
/// slot taken it waits for the owner instead, who is told so.
fn due_now(ctx: &Ctx, a: &Action, now: i64) {
    let store = ctx.store;
    let full = a.kind == ActionKind::Run && !store.proc_slot_free().unwrap_or(false);
    if full {
        match store.unschedule(a.id) {
            Ok(true) => {
                let (n, s) = (a.id, &a.summary);
                let text = format!(
                    "[zephyr] #{n} ({s}) came due, but {} commands are running, so it waits for \
                     you: /ok {n} when one ends, /no {n} drops it",
                    crate::store::MAX_RUNNING_PROCS
                );
                tell(store, a, &text);
            }
            Ok(false) => {}
            Err(e) => eprintln!("zephyr: holding delayed #{} failed: {e}", a.id),
        }
        return;
    }
    match approve(ctx, a.id, false, By::Delay, now) {
        Ok(text) => tell(store, a, &text),
        Err(why) => eprintln!("zephyr: delayed #{} did not go: {why}", a.id),
    }
}

/// Delays due by `due_by` wait for the owner again, each card shown once more: at start, while
/// paused, at `/resume`, and at `/stop` (`i64::MAX`, every delay).
pub fn ask_again(cfg: &Config, store: &Store, due_by: i64) {
    match store.ask_again(due_by) {
        Ok(back) => ask(cfg, store, &back),
        Err(e) => eprintln!("zephyr: holding delayed actions failed: {e}"),
    }
}

/// `/resume` (I1): what came due during the pause waits for the owner, before any tick runs it.
pub fn resume(cfg: &Config, store: &Store, now: i64) {
    ask_again(cfg, store, now);
}

/// `/stop` (R47): every delay waits for the owner again, so nothing set to go by itself goes
/// after the owner said stop; every browser action waiting is denied. Held actions stay held.
pub fn stop(cfg: &Config, store: &Store, waits: &Waits) {
    ask_again(cfg, store, i64::MAX);
    waits.deny_all();
}

/// At start (spec §4.9), after `procs` adopted what a re-exec handed over: a delay that came due
/// while the daemon was down waits for the owner again, who is shown its card once more, and an
/// action that was running is `unknown` unless its command is still running.
pub fn restore(cfg: &Config, store: &Store, now: i64) {
    match store.restore_actions(now) {
        Ok(back) => ask(cfg, store, &back),
        Err(e) => eprintln!("zephyr: restoring held actions failed: {e}"),
    }
}

/// Shows each action's card to whoever it waits for.
fn ask(cfg: &Config, store: &Store, actions: &[Action]) {
    for a in actions {
        tell(store, a, &crate::gate::card(a, cfg, store));
    }
}

/// A notice about `a` for the owner of the run that asked (else the owner last answered).
fn tell(store: &Store, a: &Action, text: &str) {
    let told = match a.origin.channel.is_empty() {
        true => store.notify_last_owner(text),
        false => store.notify(&a.origin.channel, &a.origin.sender, text),
    };
    if let Err(e) = told {
        eprintln!("zephyr: telling the owner about #{} failed: {e}", a.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{NewAction, Origin};

    fn setup(name: &str, extra: &str) -> (Arc<Config>, Store) {
        let home = crate::test_dir(name);
        let text = format!("rpi = '/r'\nhome = {home:?}\ntimezone = 'UTC'\n{extra}");
        let cfg: Config = toml::from_str(&text).unwrap();
        std::fs::create_dir_all(cfg.state_dir()).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        (Arc::new(cfg), store)
    }

    /// A message held from main on `chat`, as the gate stores one.
    fn hold(
        store: &Store,
        kind: ActionKind,
        payload: &str,
        secrets: &[&str],
        due: Option<i64>,
    ) -> i64 {
        store
            .hold_action(&NewAction {
                kind,
                class: "messages".into(),
                targets: vec!["a@x.org".into()],
                summary: "email to a@x.org".into(),
                reason: None,
                payload: payload.into(),
                secrets: secrets.iter().map(|s| s.to_string()).collect(),
                origin: Origin {
                    channel: "chat".into(),
                    sender: "o".into(),
                    session: "main".into(),
                    run: "main:1".into(),
                    call_id: Some("c1".into()),
                },
                due_at: due,
            })
            .unwrap()
    }

    fn told(store: &Store) -> Vec<String> {
        (store.pending_outbox().unwrap().into_iter())
            .map(|o| o.text)
            .collect()
    }

    const SEND: &str = r#"{"command": "himalaya message send < m.eml"}"#;

    /// R46: a target becomes known only once its action started.
    #[test]
    fn a_target_is_remembered_only_when_its_action_starts() {
        let (cfg, store) = setup("actions-remember", "[approvals]\npreset = \"semi\"");
        let (procs, waits) = (Procs::default(), Waits::default());
        let ctx = Ctx {
            cfg: &cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
        };
        // Its token is not in the vault: it cannot start.
        let id = hold(&store, ActionKind::Run, SEND, &["NOPE"], None);
        let by = By::Owner { channel: "chat" };
        let reply = approve(&ctx, id, false, by, crate::now()).unwrap();
        assert!(
            reply.starts_with(&format!(
                "[zephyr] approved #{id} (email to a@x.org), but it did not start: not run: no \
                 token named NOPE"
            )),
            "{reply}"
        );
        assert!(!reply.contains("without asking"), "{reply}");
        assert!(!store.all_known("messages", &["a@x.org".into()]).unwrap());
        assert_eq!(store.action(id).unwrap().unwrap().status, "failed");
    }

    /// R45: whoever set a delay in motion is told how it ended: gone, or not started and why.
    #[tokio::test]
    async fn the_owner_is_told_how_a_delay_ended() {
        let (cfg, store) = setup("actions-delay-told", "");
        let (procs, waits) = (Procs::default(), Waits::default());
        let ctx = Ctx {
            cfg: &cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
        };
        let now = crate::now();
        let pass = r#"{"server": "mail", "tool": "send", "arguments": {"to": "a@x.org"}}"#;
        let p = hold(&store, ActionKind::Pass, pass, &[], Some(now - 1));
        let w = hold(&store, ActionKind::Wait, "{}", &[], Some(now - 1));
        tick(&ctx, now, false);
        let status = |id| store.action(id).unwrap().unwrap().status;
        assert_eq!((status(p).as_str(), status(w).as_str()), ("done", "failed"));
        let told = told(&store);
        assert!(
            told.contains(&format!(
                "[zephyr] #{p} (email to a@x.org) went at the end of its delay: the agent may make \
                 exactly that call once in the next 30 min"
            )),
            "{told:?}"
        );
        assert!(
            told.contains(&format!(
                "[zephyr] #{w} (email to a@x.org) came due, but it did not start: the browser is \
                 no longer waiting, so nothing was done"
            )),
            "{told:?}"
        );
        // With every process slot taken, a due command waits for the owner instead.
        for _ in 0..crate::store::MAX_RUNNING_PROCS {
            store.add_proc("sleep 9", "chat", "o").unwrap().unwrap();
        }
        let r = hold(&store, ActionKind::Run, SEND, &[], Some(now - 1));
        tick(&ctx, now, false);
        assert_eq!(status(r), "pending");
        let told = super::tests::told(&store);
        assert!(
            told.contains(&format!(
                "[zephyr] #{r} (email to a@x.org) came due, but 8 commands are running, so it \
                 waits for you: /ok {r} when one ends, /no {r} drops it"
            )),
            "{told:?}"
        );
    }

    #[tokio::test]
    async fn a_wait_is_answered_once_and_denied_when_its_action_ends() {
        let store = Store::open(&crate::test_dir("actions-waits").join("state.db")).unwrap();
        let waits = Waits::default();
        let rx = waits.add(7);
        assert!(waits.answer(7, true));
        assert_eq!(rx.await, Ok(true));
        assert!(!waits.answer(7, true), "answered once");
        // No action #8 waits in the store: its wait is denied.
        let rx = waits.add(8);
        waits.deny_settled(&store);
        assert_eq!(rx.await, Ok(false));
        let _rx = waits.add(9);
        assert_eq!(waits.deny_all(), 1);
    }
}
