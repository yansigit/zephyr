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
use crate::scheduler::{Note, Steers};
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

    /// Forgets action `id`'s wait without answering it: its wrapper is gone.
    pub fn remove(&self, id: i64) {
        self.lock().remove(&id);
    }

    /// `/stop`: denies every browser action waiting. Returns the actions whose wrapper was told.
    pub fn deny_all(&self) -> Vec<i64> {
        let all: Vec<_> = self.lock().drain().collect();
        (all.into_iter())
            .filter_map(|(id, tx)| tx.send(false).is_ok().then_some(id))
            .collect()
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
    /// The task and job runs in progress, which a pass's note is steered into.
    pub steers: &'a Steers,
    /// The phone calls in progress (DESIGN §4.26).
    pub calls: &'a crate::calls::live::Calls,
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
        let is_strong = strong.iter().any(|c| c == channel);
        if a.class == "money" && money_strong && !is_strong {
            return Err(format!(
                "[zephyr] approving a payment needs {}",
                strong.join(" or ")
            ));
        }
        // The payment window, whatever `strong_only` says (DESIGN §4.25).
        crate::gate::window_check(ctx.cfg, store, &a, is_strong, now)?;
    }
    // A phone call is checked again as things are now (DESIGN §4.26): approved out of the
    // calling hours where it rings, it stays waiting for an `/ok` inside them.
    if a.kind == ActionKind::Call {
        let args: Value = serde_json::from_str(&a.payload).unwrap_or_default();
        if let Err(why) = crate::calls::check(ctx.cfg, &args, ctx.cfg.calls_clock()) {
            let why = why.trim_start_matches("not run: ");
            return Err(format!("[zephyr] {why}; /ok {id} again then"));
        }
    }
    // Before it is taken, so a full process table leaves the approval waiting (R44).
    if a.kind == ActionKind::Run && !store.proc_slot_free().map_err(failed)? {
        return Err(format!(
            "[zephyr] {} commands are running; /ok {id} again when one ends",
            crate::store::MAX_RUNNING_PROCS
        ));
    }
    // R62: the owner's own `/ok` marks it approved, so it no longer counts toward the day.
    let taken = match by {
        By::Owner { .. } => store.take_approved(id),
        By::Delay => store.take_action(id, "running"),
    };
    let Some(a) = taken.map_err(failed)? else {
        return Err(not_waiting(id));
    };
    let decision = match (by, once) {
        (By::Delay, _) => "due",
        (_, true) => "once",
        _ => "approved",
    };
    // What the window counts: every payment the owner approved or that came due, recorded
    // with the count that binds. One that no longer fits waits again, for a strong channel.
    let (source, strong) = match by {
        By::Owner { channel } => {
            let strong = ctx
                .cfg
                .settings
                .strong_channels
                .iter()
                .any(|c| c == channel);
            ("ok", strong)
        }
        By::Delay => ("due", false),
    };
    crate::gate::paid(ctx.cfg, store, &a, source, strong, now)?;
    audit(ctx.cfg, store, &a, decision);
    let started = match a.kind {
        ActionKind::Run => run(ctx, &a),
        ActionKind::Pass => pass(ctx, &a, by, now),
        ActionKind::Wait => wait(ctx, &a),
        ActionKind::Card => card(ctx, &a),
        ActionKind::Call => call(ctx, &a),
    };
    let (n, s) = (a.id, summary(ctx.cfg, &a));
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
        (Err(why), by) => not_run(ctx.cfg, store, &a, by, &why),
    })
}

/// Under `ask-new` the approved action's targets are known from now on (spec §4.3): what the
/// owner's answer adds, the targets that were new.
fn remember(cfg: &Config, store: &Store, a: &Action) -> String {
    // A number is never remembered: every call waits for the owner.
    if a.kind == ActionKind::Call {
        return String::new();
    }
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
        false => format!(
            "; from now on {} goes without asking",
            crate::facts::scrubbed(cfg, new.join(", "))
        ),
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
    let mut env = match call.secrets.is_empty() {
        true => Vec::new(),
        false => crate::vault::exec_env(ctx.cfg, ctx.store, &call.command, &call.secrets)?,
    };
    // As an `exec` command: what it runs asks the gate as the session that asked.
    env.extend(crate::gate::agent_env(ctx.cfg, &a.origin.session));
    let to = origin(ctx.store, a).ok_or("there is no owner to report to")?;
    let p = (ctx.procs).start_action(ctx.cfg, ctx.store, a.id, call, to, env)?;
    Ok(format!(
        "running it as process #{p}; the agent gets its report"
    ))
}

/// A connector action: a one-time pass for exactly that call (server, tool, arguments), good
/// for 30 minutes; the agent is told to make it again: steered into the task or job run that
/// asked while it still runs, else as main's note.
fn pass(ctx: &Ctx, a: &Action, by: By, now: i64) -> Result<String, String> {
    let store = ctx.store;
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
        summary(ctx.cfg, a)
    );
    let steered = Note {
        action: a.id,
        text: crate::store::note_text(a.id, word, &text),
        fallback: Some((word.into(), text)),
        queued: None,
    };
    if let Err(Note {
        fallback: Some((word, text)),
        ..
    }) = ctx.steers.send(&a.origin.session, steered)
    {
        note(store, a, &word, &text);
    }
    Ok("the agent may make exactly that call once in the next 30 min".into())
}

/// A card request (DESIGN §4.25): read again as the settings are now, then issued off the
/// router's loop; the owner is told what to do next, and the agent when the card can be filled.
fn card(ctx: &Ctx, a: &Action) -> Result<String, String> {
    let (checked, ask) = crate::gate::approved_card(ctx.cfg, ctx.store, a)?;
    if crate::cards::caps(ctx.cfg).is_none() {
        return Err(crate::cards::NOT_SET_UP
            .trim_start_matches("not run: ")
            .into());
    }
    crate::cards::issue_approved(ctx.cfg, checked, ask, a);
    Ok("issuing the card; what to do next follows".into())
}

/// A phone call: the daemon dials it now (`calls::live`). It stays `running` until the call
/// ends, which closes it and hands main the transcript.
fn call(ctx: &Ctx, a: &Action) -> Result<String, String> {
    let to = origin(ctx.store, a).ok_or("there is no owner to report to")?;
    ctx.calls.start(ctx.cfg, a, to)
}

/// A browser action: the wrapper waiting on it goes on, if it still waits. It stays `running`
/// until the wrapper says whether it ran, once it read the page again (I4): the gate socket
/// records how it ended then (M2).
fn wait(ctx: &Ctx, a: &Action) -> Result<String, String> {
    match ctx.waits.answer(a.id, true) {
        true => Ok("the browser goes on".into()),
        false => Err("the browser is no longer waiting, so nothing was done".into()),
    }
}

/// A taken action that did not start: failed, the agent told why, and the answer for whoever
/// set it in motion.
fn not_run(cfg: &Config, store: &Store, a: &Action, by: By, why: &str) -> String {
    // R70: why can carry the agent's words (a program's name), so it is scrubbed too.
    let why = crate::facts::scrubbed(cfg, why.to_string());
    finish(store, a, "failed", &why);
    let (n, s) = (a.id, summary(cfg, a));
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

/// R70: `a`'s summary as it may be shown now. It was scrubbed of the private facts when it was
/// held; a fact saved since is scrubbed here. When the facts can't be read only the summary is
/// withheld, so whatever shows it still names the action and how to answer it.
fn summary(cfg: &Config, a: &Action) -> String {
    crate::facts::scrubbed(cfg, a.summary.clone())
}

/// The owner's decision on `a` as an audit row (spec §4.9), beside the gate's own: the action's
/// number and summary, the daemon's words, never its payload.
fn audit(cfg: &Config, store: &Store, a: &Action, outcome: &str) {
    let tool = match a.kind {
        ActionKind::Run if a.origin.session.starts_with("task-") => "bash",
        ActionKind::Run if a.origin.session.starts_with("job-") => "bash",
        ActionKind::Run => "exec",
        ActionKind::Pass => "mcp_call",
        ActionKind::Wait => "browser",
        ActionKind::Card => "card",
        ActionKind::Call => "call",
    };
    let args = format!("#{} {}", a.id, summary(cfg, a));
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

/// A browser action whose wrapper went away before the owner answered (killed with its
/// child): dropped, as the gate socket finds it, with nothing left to tell.
pub fn abandon(cfg: &Config, store: &Store, id: i64) {
    match store.take_action(id, "dropped") {
        Ok(Some(a)) => audit(cfg, store, &a, "dropped"),
        Ok(None) => {}
        Err(e) => eprintln!("zephyr: dropping #{id} failed: {e}"),
    }
}

/// `/no N` on a held action: dropped, and the agent told not to try it again (spec §4.6). The
/// router denies a waiting browser action afterwards (`Waits::deny_settled`).
pub fn decline(cfg: &Config, store: &Store, id: i64) -> String {
    match store.take_action(id, "dropped") {
        Ok(Some(a)) => {
            audit(cfg, store, &a, "dropped");
            let s = summary(cfg, &a);
            let text = format!("the owner declined #{id} ({s}); don't try it again");
            note(store, &a, "declined", &text);
            format!("[zephyr] dropped #{id} ({s})")
        }
        Ok(None) => not_waiting(id),
        Err(e) => format!("[zephyr] reading proposal #{id} failed: {e}"),
    }
}

/// `/cancel N`: a delayed action is cancelled before it goes, and the agent is told; a delayed
/// browser action is denied. `Ok` is the answer once it was cancelled, `Err` the answer when
/// nothing was (as `approve` answers).
pub fn cancel(cfg: &Config, store: &Store, waits: &Waits, arg: &str) -> Result<String, String> {
    let Ok(id) = arg.trim().trim_start_matches('#').parse::<i64>() else {
        return Err("[zephyr] usage: /cancel N, for an action waiting out its delay".into());
    };
    match store.action(id) {
        Ok(Some(a)) if a.status == "scheduled" => {}
        Ok(Some(a)) if a.status == "pending" => {
            return Err(format!(
                "[zephyr] #{id} is not waiting out a delay; /no {id} drops it"
            ))
        }
        Ok(_) => return Err(not_waiting(id)),
        Err(e) => return Err(format!("[zephyr] reading proposal #{id} failed: {e}")),
    }
    match store.take_action(id, "cancelled") {
        Ok(Some(a)) => {
            audit(cfg, store, &a, "cancelled");
            waits.answer(id, false);
            let s = summary(cfg, &a);
            let text = format!("the owner cancelled #{id} ({s}); don't try it again");
            note(store, &a, "cancelled", &text);
            Ok(format!("[zephyr] cancelled #{id} ({s})"))
        }
        Ok(None) => Err(not_waiting(id)),
        Err(e) => Err(format!("[zephyr] reading proposal #{id} failed: {e}")),
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
                summary(cfg, a),
                crate::settings::clock(at, &tz),
                a.id
            ),
            _ => format!(
                "#{} {} · /ok {} · /no {}",
                a.id,
                summary(cfg, a),
                a.id,
                a.id
            ),
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
                audit(ctx.cfg, store, &a, "expired");
                ctx.waits.answer(a.id, false);
                let within = match crate::gate::lifetime_secs(&a) < 3600 {
                    true => "10 min",
                    false => "a day",
                };
                let text = format!(
                    "the owner did not answer #{} ({}) within {within}, so it was not run; ask \
                     again if it is still wanted",
                    a.id,
                    summary(ctx.cfg, &a)
                );
                note(store, &a, "expired", &text);
            }
        }
        Err(e) => eprintln!("zephyr: expiring held actions failed: {e}"),
    }
    ctx.waits.deny_settled(store);
}

/// Delayed action `a` came due: it goes, and its owner is told how it ended. R63: when the facts
/// or its class's mode would hold it now (the owner turned approvals on, or saved a fact it
/// holds), it waits for the owner again, who is told why and shown its card. With every process
/// slot taken it waits for the owner too, who is told so.
fn due_now(ctx: &Ctx, a: &Action, now: i64) {
    let store = ctx.store;
    if let Some(why) = crate::gate::due_check(ctx.cfg, store, a) {
        match store.unschedule(a.id) {
            Ok(true) => {
                audit(ctx.cfg, store, a, "re-asked");
                let (n, s) = (a.id, summary(ctx.cfg, a));
                let text =
                    format!("[zephyr] #{n} ({s}) came due, but {why}, so it waits for you again");
                tell(store, a, &text);
                let mut again = a.clone();
                again.status = "pending".into();
                tell(store, a, &crate::gate::card(&again, ctx.cfg, store));
            }
            Ok(false) => {}
            Err(e) => eprintln!("zephyr: holding delayed #{} failed: {e}", a.id),
        }
        return;
    }
    let full = a.kind == ActionKind::Run && !store.proc_slot_free().unwrap_or(false);
    if full {
        match store.unschedule(a.id) {
            Ok(true) => {
                audit(ctx.cfg, store, a, "re-asked");
                let (n, s) = (a.id, summary(ctx.cfg, a));
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
        // One that no longer fits the payment window waits for the owner again, who is told.
        Err(why) if store.action_strong(a.id).unwrap_or(false) => tell(store, a, &why),
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

/// `/stop` (R47): every browser action waiting is denied, and settled as `dropped` (D10), so no
/// card is left that can only fail; then every other delay waits for the owner again, so nothing
/// set to go by itself goes after the owner said stop. Held actions stay held.
pub fn stop(cfg: &Config, store: &Store, waits: &Waits) {
    for id in waits.deny_all() {
        match store.end_action(
            id,
            "dropped",
            "the owner's /stop denied the waiting browser",
        ) {
            Ok(Some(a)) => audit(cfg, store, &a, "stopped"),
            Ok(None) => {}
            Err(e) => eprintln!("zephyr: settling #{id} at /stop failed: {e}"),
        }
    }
    ask_again(cfg, store, i64::MAX);
}

/// At start (spec §4.9), after `procs` adopted what a re-exec handed over: a browser action
/// still waiting ended with the daemon that held its wrapper's connection (D10), so it is
/// `failed` and its owner told; a delay that came due while the daemon was down waits for the
/// owner again, who is shown its card once more; and an action that was running is `unknown`
/// unless its command is still running.
pub fn restore(cfg: &Config, store: &Store, now: i64) {
    match store.end_waits(now) {
        Ok(ended) => {
            for a in ended {
                audit(cfg, store, &a, "failed");
                let text = format!(
                    "[zephyr] #{} ({}) did not run: {RESTARTED}",
                    a.id,
                    summary(cfg, &a)
                );
                tell(store, &a, &text);
            }
        }
        Err(e) => eprintln!("zephyr: ending the browser waits failed: {e}"),
    }
    match store.restore_actions(now) {
        Ok(back) => ask(cfg, store, &back),
        Err(e) => eprintln!("zephyr: restoring held actions failed: {e}"),
    }
}

/// Why a browser action waiting at start did not run.
const RESTARTED: &str = "zephyr restarted while the browser waited";

/// Shows each action's card to whoever it waits for, again, and audits that it was asked again.
fn ask(cfg: &Config, store: &Store, actions: &[Action]) {
    for a in actions {
        audit(cfg, store, a, "re-asked");
        tell(store, a, &crate::gate::card(a, cfg, store));
    }
}

/// A notice about `a` for the owner of the run that asked (else the owner last answered). The
/// gate socket tells this way of a browser action the owner let go that did not run. What it
/// says shows `a`'s summary as [`summary`] gives it (R70).
pub(crate) fn tell(store: &Store, a: &Action, text: &str) {
    if let Err(e) = told(store, a, text) {
        eprintln!("zephyr: telling the owner about #{} failed: {e}", a.id);
    }
}

/// [`tell`], its failure the caller's.
pub(crate) fn told(store: &Store, a: &Action, text: &str) -> rusqlite::Result<()> {
    match a.origin.channel.is_empty() {
        true => store.notify_last_owner(text),
        false => store.notify(&a.origin.channel, &a.origin.sender, text),
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

    /// `cfg`'s home with other settings, as a reload makes it.
    fn again(cfg: &Config, extra: &str) -> Arc<Config> {
        let text = format!(
            "rpi = '/r'\nhome = {:?}\ntimezone = 'UTC'\n{extra}",
            cfg.home
        );
        Arc::new(toml::from_str(&text).unwrap())
    }

    /// A message held from main on `chat`, as the gate stores one.
    fn hold(
        store: &Store,
        kind: ActionKind,
        payload: &str,
        secrets: &[&str],
        due: Option<i64>,
    ) -> i64 {
        let id = store
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
            .unwrap();
        // A delay of 5 minutes, as the gate holds one, however soon it is due.
        if let Some(due) = due {
            store
                .db_for_test()
                .execute(
                    "UPDATE proposals SET created_at = ?2 WHERE id = ?1",
                    rusqlite::params![id, due - 300],
                )
                .unwrap();
        }
        id
    }

    /// A browser click on Send waiting in place, as the wrapper asks for one.
    const CLICK: &str = r#"{"args": {"argv": ["click", "@e3"]}, "browser": {"action": "click",
        "url": "https://mail.example.com/c", "origin": "https://mail.example.com",
        "role": "button", "name": "Send"}}"#;

    fn told(store: &Store) -> Vec<String> {
        (store.pending_outbox().unwrap().into_iter())
            .map(|o| o.text)
            .collect()
    }

    const SEND: &str = r#"{"command": "himalaya message send < m.eml"}"#;

    /// `/cancel N` says whether it cancelled, so `zephyr.decide` need not read its words.
    #[test]
    fn cancel_says_whether_it_cancelled() {
        let (cfg, store) = setup("actions-cancel", "");
        let waits = Waits::default();
        let now = hold(&store, ActionKind::Run, SEND, &[], None);
        let later = hold(&store, ActionKind::Run, SEND, &[], Some(crate::now() + 300));
        let refused = cancel(&cfg, &store, &waits, &now.to_string()).unwrap_err();
        assert!(refused.contains("is not waiting out a delay"), "{refused}");
        assert_eq!(
            cancel(&cfg, &store, &waits, &later.to_string()),
            Ok(format!("[zephyr] cancelled #{later} (email to a@x.org)"))
        );
        assert!(cancel(&cfg, &store, &waits, &later.to_string()).is_err());
        assert!(cancel(&cfg, &store, &waits, "x").is_err());
    }

    /// DESIGN §4.26: a call approved outside the calling hours where it rings stays waiting.
    #[test]
    fn an_ok_out_of_hours_leaves_the_call_pending() {
        use crate::calls::tests::{noon, toronto};
        let (cfg, store) = setup(
            "actions-call",
            "[calls]\nprovider = 'telnyx'\ncaller_name = 'A'\ncontact_phone = '416-555-0199'\n\
             contact_email = 'a@example.com'\n",
        );
        let mut at_noon = (*cfg).clone();
        at_noon.voice_dir = cfg.home.join("voice");
        std::fs::create_dir_all(at_noon.voice_dir.join("voices")).unwrap();
        std::fs::write(
            at_noon.voice_dir.join("voices/en_US-lessac-medium.onnx"),
            "",
        )
        .unwrap();
        at_noon.calls_now = Some(noon());
        let req = crate::gate::Request {
            session: "main".into(),
            call_id: Some("c1".into()),
            tool: "call".into(),
            args: serde_json::json!({"number": "416-555-0123", "purpose": "book a table"}),
            browser: None,
            run: "main:1".into(),
            reason: None,
        };
        let owner = ("chat".to_string(), "o".to_string());
        let held = crate::gate::decide(&at_noon, &store, &req, owner, crate::now());
        let crate::gate::Verdict::Hold { id, .. } = held else {
            panic!("{held:?}")
        };
        let (procs, waits, steers) = (Procs::default(), Waits::default(), Steers::default());
        let calls = crate::calls::live::Calls::idle();
        let ctx = |cfg| Ctx {
            cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
            steers: &steers,
            calls: &calls,
        };
        let by = By::Owner { channel: "chat" };
        // 21:45 on a Tuesday in Toronto.
        let late = Arc::new(Config {
            calls_now: Some(toronto("2026-11-10", "21:45")),
            ..at_noon.clone()
        });
        assert_eq!(
            approve(&ctx(&late), id, false, by, crate::now()).unwrap_err(),
            format!(
                "[zephyr] not after 21:30 their time (calls only 9:00–21:30 weekdays, \
                 10:00–18:00 weekends their time); /ok {id} again then"
            )
        );
        assert_eq!(store.action(id).unwrap().unwrap().status, "pending");
        // Inside the hours it is taken; with nowhere for Telnyx to reach zephyr it does not
        // start, and says why. Its number is never remembered.
        let at_noon = Arc::new(at_noon);
        let said = approve(&ctx(&at_noon), id, false, by, crate::now()).unwrap();
        assert!(
            said.contains("but it did not start: set [calls] port and media_url"),
            "{said}"
        );
        assert_eq!(store.action(id).unwrap().unwrap().status, "failed");
        let number = ["tel:+14165550123".to_string()];
        assert!(!store.all_known("messages", &number).unwrap());
    }

    /// R46: a target becomes known only once its action started.
    #[test]
    fn a_target_is_remembered_only_when_its_action_starts() {
        let (cfg, store) = setup("actions-remember", "[approvals]\npreset = \"semi\"");
        let (procs, waits, steers) = (Procs::default(), Waits::default(), Steers::default());
        let ctx = Ctx {
            cfg: &cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
            steers: &steers,
            calls: &crate::calls::live::Calls::idle(),
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
        let (cfg, store) = setup("actions-delay-told", "[approvals]\nmessages = 'delay 5'\n");
        let (procs, waits, steers) = (Procs::default(), Waits::default(), Steers::default());
        let ctx = Ctx {
            cfg: &cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
            steers: &steers,
            calls: &crate::calls::live::Calls::idle(),
        };
        let now = crate::now();
        let pass = r#"{"server": "mail", "tool": "send", "arguments": {"to": "a@x.org"}}"#;
        let p = hold(&store, ActionKind::Pass, pass, &[], Some(now - 1));
        let w = hold(&store, ActionKind::Wait, CLICK, &[], Some(now - 1));
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

    /// Spec §4.6: the session that asked is told to make the call again, steered in while its
    /// run goes on, else through main.
    #[test]
    fn a_pass_is_steered_into_the_run_that_asked_else_told_to_main() {
        let (cfg, store) = setup("actions-pass-steer", "");
        let (procs, waits, steers) = (Procs::default(), Waits::default(), Steers::default());
        let ctx = Ctx {
            cfg: &cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
            steers: &steers,
            calls: &crate::calls::live::Calls::idle(),
        };
        let pass = r#"{"server": "mail", "tool": "send", "arguments": {"to": "a@x.org"}}"#;
        let from = |session: &str| {
            let id = hold(&store, ActionKind::Pass, pass, &[], None);
            let db = rusqlite::Connection::open(cfg.db_path()).unwrap();
            let origin = format!(
                r#"{{"channel":"chat","sender":"o","session":"{session}","run":"{session}"}}"#
            );
            db.execute(
                "UPDATE proposals SET origin = ?2 WHERE id = ?1",
                rusqlite::params![id, origin],
            )
            .unwrap();
            id
        };
        let by = By::Owner { channel: "chat" };
        // Task 3 still runs: the note goes into its run, and main is handed nothing.
        let mut inbox = steers.open("task-3");
        let id = from("task-3");
        approve(&ctx, id, false, by, crate::now()).unwrap();
        let note = inbox.try_recv().expect("steered in");
        assert_eq!(note.action, id);
        assert!(note.text.starts_with(&format!(
            "[action #{id} approved; zephyr's note, not the owner's words] the owner approved \
             #{id} (email to a@x.org): make exactly that connector call again"
        )));
        let row = |id: i64| {
            let db = rusqlite::Connection::open(cfg.db_path()).unwrap();
            db.query_row(
                "SELECT text FROM inbox WHERE platform_msg_id = ?1",
                [format!("action-{id}")],
                |r| r.get::<_, String>(0),
            )
            .ok()
        };
        assert_eq!(row(id), None);
        // Its run ended: main is told instead.
        drop(inbox);
        let id = from("task-3");
        approve(&ctx, id, false, by, crate::now()).unwrap();
        assert!(row(id).is_some_and(|t| t.contains("make exactly that connector call")));
    }

    /// R63: a delay re-checks the facts and the class's mode when it comes due: stricter now, it
    /// waits for the owner again, with its card; the same or looser, it goes.
    #[test]
    fn a_delay_comes_due_as_the_owner_has_it_now() {
        let (held_cfg, store) = setup(
            "actions-due-recheck",
            "[approvals]\nposts = 'delay 5'\nmessages = 'delay 5'\n",
        );
        let now = crate::now();
        let owner = ("chat".to_string(), "o".to_string());
        let hold = |tool: &str, arguments: Value, run: &str| {
            let req = crate::gate::Request {
                session: "task-3".into(),
                call_id: None,
                tool: "mcp_call".into(),
                args: serde_json::json!({"server": "mail", "tool": tool, "arguments": arguments}),
                browser: None,
                run: run.into(),
                reason: None,
            };
            match crate::gate::decide(&held_cfg, &store, &req, owner.clone(), now) {
                crate::gate::Verdict::Hold { id, .. } => id,
                v => panic!("{v:?}"),
            }
        };
        let post = hold("create_issue", serde_json::json!({"to": "o/r"}), "due:1");
        let fact = hold(
            "send_email",
            serde_json::json!({"to": "a@x.org", "body": "call 555 010 2345"}),
            "due:2",
        );
        let same = hold("send_email", serde_json::json!({"to": "b@x.org"}), "due:3");
        let status = |id| store.action(id).unwrap().unwrap().status;
        assert_eq!(status(post), "scheduled");
        for o in store.pending_outbox().unwrap() {
            store.set_outbox_status(o.id, "sent").unwrap();
        }
        // The owner made posts `ask` and saved a fact the payload holds since.
        let now_cfg = again(
            &held_cfg,
            "[approvals]\nposts = 'ask'\nmessages = 'delay 5'\n",
        );
        std::fs::write(
            crate::facts::path(&now_cfg),
            "[[fact]]\nname = \"phone\"\nkind = \"digits\"\nvalue = \"5550102345\"\n",
        )
        .unwrap();
        let (procs, waits, steers) = (Procs::default(), Waits::default(), Steers::default());
        let ctx = Ctx {
            cfg: &now_cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
            steers: &steers,
            calls: &crate::calls::live::Calls::idle(),
        };
        tick(&ctx, now + 301, false);
        assert_eq!(
            (
                status(post).as_str(),
                status(fact).as_str(),
                status(same).as_str()
            ),
            ("pending", "pending", "done")
        );
        let told = told(&store);
        let summary = |id| store.action(id).unwrap().unwrap().summary;
        assert!(
            told.contains(&format!(
                "[zephyr] #{post} ({}) came due, but approvals.posts is now ask, so it waits \
                 for you again",
                summary(post)
            )),
            "{told:#?}"
        );
        assert!(
            told.contains(&format!(
                "[zephyr] #{fact} ({}) came due, but it contains your private fact phone, so \
                 it waits for you again",
                summary(fact)
            )),
            "{told:#?}"
        );
        for id in [post, fact] {
            let card = told
                .iter()
                .find(|t| t.starts_with(&format!("[zephyr] approve #{id}:")));
            let card = card.unwrap_or_else(|| panic!("no card for #{id}: {told:#?}"));
            assert!(
                card.ends_with(&format!("/no {id} · expires in 24 h")),
                "{card}"
            );
        }
        assert!(
            told.iter()
                .any(|t| t.starts_with(&format!("[zephyr] #{same} ("))
                    && t.contains("went at the end of its delay")),
            "{told:#?}"
        );
        assert!(!told
            .iter()
            .any(|t| t.contains("5550102345") || t.contains("010 2345")));
    }

    /// R70: a held action's summary was scrubbed of the facts when it was held. A fact saved
    /// since is scrubbed too, from every notice, agent note (the store's own reports among them),
    /// reply and audit row that shows it.
    #[test]
    fn a_summary_is_scrubbed_with_the_facts_as_they_are_when_shown() {
        let (held_cfg, mut store) = setup(
            "actions-scrub-summary",
            "[approvals]\nmessages = 'delay 5'\n",
        );
        let now = crate::now();
        let due = hold(&store, ActionKind::Run, SEND, &[], Some(now + 300));
        let declined = hold(&store, ActionKind::Run, SEND, &[], None);
        let cancelled = hold(&store, ActionKind::Run, SEND, &[], Some(now + 300));
        let ran = hold(&store, ActionKind::Run, SEND, &[], None);
        let lost = hold(&store, ActionKind::Run, SEND, &[], None);
        for id in [ran, lost] {
            store.take_action(id, "running").unwrap();
        }
        // The owner saved the recipient as a private fact since, and made messages `ask`.
        let cfg = again(&held_cfg, "[approvals]\nmessages = 'ask'\n");
        std::fs::write(
            crate::facts::path(&cfg),
            "[[fact]]\nname = \"mine\"\nkind = \"email\"\nvalue = \"a@x.org\"\n",
        )
        .unwrap();
        let (procs, waits, steers) = (Procs::default(), Waits::default(), Steers::default());
        let mut shown = vec![held(&cfg, &store)];
        shown.push(decline(&cfg, &store, declined));
        shown.push(cancel(&cfg, &store, &waits, &cancelled.to_string()).unwrap());
        // The store's own notes to main: an approved command's report, and one a restart lost.
        store
            .action_report(ran, "done", "exited 0 after 1s", "ok")
            .unwrap();
        restore(&cfg, &store, now);
        assert_eq!(store.action(lost).unwrap().unwrap().status, "unknown");
        let ctx = Ctx {
            cfg: &cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
            steers: &steers,
            calls: &crate::calls::live::Calls::idle(),
        };
        tick(&ctx, now + 301, false);
        assert_eq!(store.action(due).unwrap().unwrap().status, "pending");
        shown.extend(told(&store));
        let notes: Vec<String> = (store.db_for_test())
            .prepare("SELECT text FROM inbox WHERE platform_msg_id LIKE 'action-%'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(notes.len(), 4, "{notes:#?}");
        shown.extend(notes);
        let audit = store.audit_rows().unwrap();
        shown.extend(audit.iter().map(|(_, _, args, _)| args.clone()));
        assert!(
            shown.iter().any(|t| t.starts_with(&format!(
                "[zephyr] #{due} (email to [fact:mine]) came due"
            ))),
            "{shown:#?}"
        );
        assert!(
            !shown.iter().any(|t| t.to_lowercase().contains("a@x.org")),
            "{shown:#?}"
        );
        // NB6: asking again is in the audit, as every other outcome on the tick is.
        assert!(
            audit.iter().any(
                |(_, _, args, outcome)| args.starts_with(&format!("#{due} "))
                    && outcome == "re-asked"
            ),
            "{audit:#?}"
        );
    }

    /// Main's notes about held actions, as the store wrote them.
    fn notes(store: &Store) -> Vec<String> {
        (store.db_for_test())
            .prepare("SELECT text FROM inbox WHERE platform_msg_id LIKE 'action-%'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    const PHONE: &str = "[[fact]]\nname = \"phone\"\nkind = \"digits\"\nvalue = \"5550102345\"\n";

    /// R70: why an approved action did not start can carry the agent's words (a program's name
    /// from the vault's check); it is scrubbed of the facts too.
    #[test]
    fn why_an_action_did_not_start_is_scrubbed() {
        let (cfg, store) = setup("actions-not-run-scrub", "");
        std::fs::write(crate::facts::path(&cfg), PHONE).unwrap();
        let id = hold(&store, ActionKind::Run, SEND, &[], None);
        let a = store.action(id).unwrap().unwrap();
        let why = "it needs the owner's OK to use GH_TOKEN with notify-5550102345";
        let owner = not_run(&cfg, &store, &a, By::Delay, why);
        let notes = notes(&store);
        assert!(owner.contains("notify-[fact:phone]"), "{owner}");
        assert!(
            !owner.contains("5550102345") && !notes.iter().any(|t| t.contains("5550102345")),
            "{owner} {notes:?}"
        );
    }

    /// R70: the store reads the facts beside the path it was opened at, even when state.db is
    /// a link to a file in another folder (SQLite opens the file the link names).
    #[test]
    fn the_store_scrubs_with_the_facts_beside_the_path_it_was_opened_at() {
        let (cfg, first) = setup("actions-scrub-linked", "");
        drop(first);
        let elsewhere = cfg.state_dir().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::rename(cfg.db_path(), elsewhere.join("state.db")).unwrap();
        std::os::unix::fs::symlink(elsewhere.join("state.db"), cfg.db_path()).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        std::fs::write(
            crate::facts::path(&cfg),
            "[[fact]]\nname = \"mine\"\nkind = \"email\"\nvalue = \"a@x.org\"\n",
        )
        .unwrap();
        let id = hold(&store, ActionKind::Run, SEND, &[], None);
        store.take_action(id, "running").unwrap();
        store.restore_actions(crate::now()).unwrap();
        let notes = notes(&store);
        assert!(
            notes.iter().any(|t| t.contains("[fact:mine]"))
                && !notes.iter().any(|t| t.contains("a@x.org")),
            "{notes:?}"
        );
    }

    /// R70: with a facts file that can't be read, only what could hold a fact is withheld:
    /// `/held`, the notices, the card shown again and the audit still name the action and how to
    /// answer it.
    #[test]
    fn an_unreadable_facts_file_withholds_only_the_summary() {
        let (held_cfg, store) = setup(
            "actions-scrub-unreadable",
            "[approvals]\nmessages = 'delay 5'\n",
        );
        let now = crate::now();
        let due = hold(&store, ActionKind::Run, SEND, &[], Some(now + 300));
        let cfg = again(&held_cfg, "[approvals]\nmessages = 'ask'\n");
        std::fs::write(crate::facts::path(&cfg), "not toml [").unwrap();
        let listed = held(&cfg, &store);
        assert!(
            listed.contains(&format!("#{due} ")) && listed.contains(&format!("/cancel {due}")),
            "{listed}"
        );
        let (procs, waits, steers) = (Procs::default(), Waits::default(), Steers::default());
        let ctx = Ctx {
            cfg: &cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
            steers: &steers,
            calls: &crate::calls::live::Calls::idle(),
        };
        tick(&ctx, now + 301, false);
        let told = told(&store);
        assert!(
            told.iter()
                .any(|t| t.starts_with(&format!("[zephyr] #{due} (")) && t.contains("came due")),
            "{told:#?}"
        );
        assert!(
            told.iter()
                .any(|t| t.starts_with(&format!("[zephyr] approve #{due}:"))
                    && t.contains(&format!("/ok {due}"))),
            "{told:#?}"
        );
        let mut all = told;
        all.push(listed);
        assert!(!all.iter().any(|t| t.contains("a@x.org")), "{all:#?}");
        let audit = store.audit_rows().unwrap();
        assert!(
            audit.iter().any(
                |(_, _, args, outcome)| args.starts_with(&format!("#{due} "))
                    && outcome == "re-asked"
            ),
            "{audit:#?}"
        );
    }

    /// R63 under `ask-new` and a longer delay: a known recipient goes, a new one asks, and a
    /// delay made longer since asks too.
    #[test]
    fn a_delay_due_under_ask_new_goes_only_to_known_recipients() {
        let (held_cfg, store) = setup("actions-due-ask-new", "[approvals]\nmessages = 'delay 5'\n");
        let now = crate::now();
        let hold = |to: &str, run: &str| {
            let req = crate::gate::Request {
                session: "task-3".into(),
                call_id: None,
                tool: "mcp_call".into(),
                args: serde_json::json!({"server": "mail", "tool": "send_email",
                                         "arguments": {"to": to}}),
                browser: None,
                run: run.into(),
                reason: None,
            };
            let origin = ("chat".to_string(), "o".to_string());
            match crate::gate::decide(&held_cfg, &store, &req, origin, now) {
                crate::gate::Verdict::Hold { id, .. } => id,
                v => panic!("{v:?}"),
            }
        };
        let (known, new) = (hold("c@x.org", "an:1"), hold("d@x.org", "an:2"));
        let longer = hold("e@x.org", "an:3");
        store
            .remember_targets("messages", &["mail/send_email".into(), "c@x.org".into()])
            .unwrap();
        let ask_new = again(&held_cfg, "[approvals]\nmessages = 'ask-new'\n");
        let (procs, waits, steers) = (Procs::default(), Waits::default(), Steers::default());
        let calls = crate::calls::live::Calls::idle();
        let ctx = |cfg| Ctx {
            cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
            steers: &steers,
            calls: &calls,
        };
        // Only the first two are due by now + 301 if the third is pushed back: tick them alone.
        let db = rusqlite::Connection::open(ask_new.db_path()).unwrap();
        db.execute(
            "UPDATE proposals SET due_at = due_at + 3600 WHERE id = ?1",
            [longer],
        )
        .unwrap();
        tick(&ctx(&ask_new), now + 301, false);
        let status = |id| store.action(id).unwrap().unwrap().status;
        assert_eq!(
            (status(known).as_str(), status(new).as_str()),
            ("done", "pending")
        );
        let delay30 = again(&held_cfg, "[approvals]\nmessages = 'delay 30'\n");
        db.execute(
            "UPDATE proposals SET due_at = due_at - 3600 WHERE id = ?1",
            [longer],
        )
        .unwrap();
        tick(&ctx(&delay30), now + 301, false);
        assert_eq!(status(longer), "pending");
        let told = told(&store);
        assert!(
            told.iter().any(|t| {
                t.starts_with(&format!("[zephyr] #{longer} (")) && t.contains(
                "came due, but approvals.messages is now delay 30, so it waits for you again"
            )
            }),
            "{told:#?}"
        );
    }

    /// D10: a restart ends every browser action still waiting (its wrapper saw the socket close),
    /// and `/stop` settles each one it denied, so no card is left that can only fail.
    #[test]
    fn waits_a_restart_or_a_stop_ended_are_settled() {
        let (cfg, store) = setup("actions-waits-restart", "");
        let now = crate::now();
        let pending = hold(&store, ActionKind::Wait, "{}", &[], None);
        let scheduled = hold(&store, ActionKind::Wait, "{}", &[], Some(now + 300));
        let run = hold(&store, ActionKind::Run, SEND, &[], None);
        restore(&cfg, &store, now);
        let status = |id| store.action(id).unwrap().unwrap().status;
        assert_eq!(
            (
                status(pending).as_str(),
                status(scheduled).as_str(),
                status(run).as_str()
            ),
            ("failed", "failed", "pending")
        );
        let told = told(&store);
        for id in [pending, scheduled] {
            assert!(
                told.contains(&format!(
                    "[zephyr] #{id} (email to a@x.org) did not run: zephyr restarted while the \
                     browser waited"
                )),
                "{told:#?}"
            );
        }
        assert!(!told
            .iter()
            .any(|t| t.starts_with(&format!("[zephyr] approve #{scheduled}"))));
        // `/stop`: the waiting wrapper is denied, and its action is settled with it.
        let waits = Waits::default();
        let stopped = hold(&store, ActionKind::Wait, "{}", &[], None);
        let mut rx = waits.add(stopped);
        let delayed = hold(&store, ActionKind::Wait, "{}", &[], Some(now + 300));
        let mut rx2 = waits.add(delayed);
        stop(&cfg, &store, &waits);
        assert_eq!(rx.try_recv(), Ok(false));
        assert_eq!(rx2.try_recv(), Ok(false));
        assert_eq!(
            (status(stopped).as_str(), status(delayed).as_str()),
            ("dropped", "dropped")
        );
        let told = super::tests::told(&store);
        assert!(!told
            .iter()
            .any(|t| t.starts_with(&format!("[zephyr] approve #{delayed}"))));
    }

    /// R62: a delay that went by itself still counts toward the day; the owner's `/ok` does not.
    #[test]
    fn only_the_owners_ok_takes_an_action_off_the_day() {
        let (cfg, store) = setup("actions-day", "[approvals]\nmessages = 'delay 5'\n");
        let (procs, waits, steers) = (Procs::default(), Waits::default(), Steers::default());
        let ctx = Ctx {
            cfg: &cfg,
            store: &store,
            procs: &procs,
            waits: &waits,
            steers: &steers,
            calls: &crate::calls::live::Calls::idle(),
        };
        let now = crate::now();
        let pass = r#"{"server": "mail", "tool": "send", "arguments": {"to": "a@x.org"}}"#;
        let delayed = hold(&store, ActionKind::Pass, pass, &[], Some(now - 1));
        let asked = hold(&store, ActionKind::Pass, pass, &[], None);
        assert_eq!(store.held_today(now).unwrap(), 2);
        tick(&ctx, now, false);
        assert_eq!(store.action(delayed).unwrap().unwrap().status, "done");
        assert_eq!(store.held_today(now).unwrap(), 2);
        approve(&ctx, asked, false, By::Owner { channel: "chat" }, now).unwrap();
        assert_eq!(store.held_today(now).unwrap(), 1);
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
        assert_eq!(waits.deny_all(), [9]);
    }
}
