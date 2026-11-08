//! `zephyr tui`: Zephyr's own terminal client on the attach port, built from rpi-tui
//! components. The loop follows upstream's `rpi-cli/src/tui_shell.rs::run_driver_tui`
//! (pi-rust v0.12.0, MIT): an input reader thread, a submit channel and the render scheduler.

mod chrome;
mod conn;
mod files;
mod history;
mod overlays;
mod settings_panel;
mod shell;
mod theme;
mod view;

use crate::config::Config;
use anyhow::Context;
use chrome::{Link, Region, Retry, Search, State};
use conn::{Ask, Login, Msg};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use overlays::{Action, Kind, Overlay};
use rpi_tui::{
    Component, Container, Editor, EditorOptions, EditorStyle, Focusable, OverlayAnchor,
    OverlayMargin, OverlayOptions, ProcessTerminal, ScrollView, StackChild, StackEntry,
    TuiAltScreen, VStack, TUI,
};
use serde_json::Value;
use std::any::Any;
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Two Ctrl+C within this window detach.
const DOUBLE_PRESS_MS: i64 = 1500;

/// Two Esc within this window, on an empty and idle composer, open the rewind picker.
const REWIND_MS: i64 = 500;

/// How often the running sign-in is asked about: often enough to feel live, and only while
/// one runs, so an idle TUI sends nothing.
const LOGIN_POLL_MS: i64 = 500;

/// How long the sign-in may go without an answered poll before the panel gives up on it. A
/// lost answer used to wedge the panel: it stopped asking, showed a phase that had stopped
/// being true, and never timed out.
const LOGIN_SILENCE_MS: i64 = 30_000;

/// What the panel says when it gives up, above the provider list, which is where the user is
/// looking: a sign-in that ended has no reply card in the transcript to explain itself.
const GIVE_UP: &str = "the gateway stopped answering; try /login again";

/// How long `copied …` stays in the footer.
const COPIED_MS: i64 = 2000;

/// Clicks on one cell this close together make a double or triple click.
const CLICK_MS: i64 = 400;

/// At most one notification in this long.
const NOTIFY_MS: i64 = 10_000;

/// OSC 9;4 state 3, the indeterminate tab progress bar, and state 0, none. rpi-tui's
/// `set_progress` sends state 1, which ConEmu, Windows Terminal and Ghostty draw as a 0% bar.
const PROGRESS_ON: &str = "\x1b]9;4;3\x07";
const PROGRESS_OFF: &str = "\x1b]9;4;0\x07";

pub async fn run(cfg: &Config) -> anyhow::Result<()> {
    let a = cfg
        .attach
        .as_ref()
        .context("zephyr tui needs [attach] in zephyr.toml")?;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        anyhow::bail!("zephyr tui needs a terminal; `zephyr chat` reads lines from a pipe");
    }
    // `zephyr attach` passes the token by env; inside the container the config has it too.
    let token = match std::env::var("RPI_SERVER_TOKEN") {
        Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
        _ => crate::attach::token(a)?,
    };
    let addr = format!("127.0.0.1:{}", a.port);
    let tz = crate::scheduler::zone(cfg).unwrap_or_else(|_| jiff::tz::TimeZone::system());
    let palette = theme::install(probe_background());

    let state = Arc::new(Mutex::new(State::default()));
    let region = |draw| -> Arc<dyn Component> {
        Arc::new(Region {
            state: state.clone(),
            p: palette,
            draw,
        })
    };
    let panel: Arc<dyn Component> = Arc::new(Panel {
        state: state.clone(),
        p: palette,
        tz: tz.clone(),
    });
    let mut view = view::View::new(palette, tz.clone());
    let document = Arc::new(Container::new());
    document.add_child(view.root.clone());
    document.add_child(Arc::new(rpi_tui::Spacer::new(1)));
    let marked = Arc::new(view::Marked {
        inner: document,
        row: usize::MAX.into(),
    });
    let scroll = Arc::new(ScrollView::primary(marked.clone()));
    let transcript = scroll.clone();
    let editor = Arc::new(Editor::new(
        EditorOptions {
            padding_x: 0,
            placeholder: Some("Message zephyr".into()),
            ..Default::default()
        },
        EditorStyle::default(),
        Arc::new(rpi_tui::Keybindings::new()),
    ));
    let input = Arc::new(InputBox {
        editor: editor.clone(),
        border: palette.dim,
    });
    let root = VStack::from_children(vec![
        StackChild::Entry(StackEntry::new(region(chrome::header))),
        StackChild::Entry(
            StackEntry::new(scroll)
                .basis(0)
                .grow(1)
                .shrink(1)
                .min_size(1),
        ),
        StackChild::Entry(StackEntry::new(region(chrome::search_bar))),
        StackChild::Entry(StackEntry::new(region(chrome::run_line))),
        StackChild::Entry(StackEntry::new(region(chrome::queue_strip))),
        StackChild::Entry(StackEntry::new(region(overlays::completions))),
        StackChild::Entry(StackEntry::new(input).shrink(0).min_size(3)),
        StackChild::Entry(StackEntry::new(region(chrome::footer))),
    ]);
    let tui = Arc::new(TuiAltScreen::new(
        Box::new(ProcessTerminal::new()),
        // The editor marks the caret; the terminal's own cursor draws it.
        true,
        None,
    ));
    tui.set_layout_root(Some(Arc::new(root)));
    tui.set_focus(Some(editor.clone()));
    editor.set_focused(true);
    tui.start_render_scheduler();
    tui.start_readerless();
    focus_reports(&tui, true);
    // The title and progress bar on screen, set from the state after each change.
    let mut look = ("", false);
    attend(&tui, &state.lock().unwrap(), &mut look);

    let (submit_tx, mut submits) = mpsc::unbounded_channel::<(String, bool)>();
    // Set by Alt+Enter around the Enter it hands the editor: that submit is a follow-up.
    let follow = Arc::new(AtomicBool::new(false));
    let following = follow.clone();
    editor.on_submit(Arc::new(move |text: &str| {
        let _ = submit_tx.send((text.to_string(), following.swap(false, Ordering::Relaxed)));
    }));
    let (input_tx, mut inputs) = mpsc::unbounded_channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    // While set, the reader leaves the terminal alone (`/shell` has it); `reading` is held around
    // each poll and read, so taking it after setting `paused` waits out the one in flight.
    let paused = Arc::new(AtomicBool::new(false));
    let reading = Arc::new(Mutex::new(()));
    let reader = {
        let stop = stop.clone();
        let (paused, reading) = (paused.clone(), reading.clone());
        tokio::task::spawn_blocking(move || {
            while !stop.load(Ordering::Relaxed) {
                if paused.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                let _turn = reading.lock().unwrap();
                if paused.load(Ordering::Relaxed) {
                    continue;
                }
                match crossterm::event::poll(Duration::from_millis(50)) {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(_) => break,
                }
                let Ok(event) = crossterm::event::read() else {
                    break;
                };
                if input_tx.send(event).is_err() {
                    break;
                }
            }
        })
    };
    let (msg_tx, mut msgs) = mpsc::unbounded_channel();
    let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
    tokio::spawn(conn::run(addr, token, msg_tx, prompt_rx));

    let mut ticker = tokio::time::interval(Duration::from_millis(166));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut ctrl_c_at = 0;
    let mut esc_at = 0;
    // The mascot frame on screen: a tick redraws when it changes, idle blinks included.
    let mut face = [String::new(), String::new(), String::new()];
    let mut was_up = false;
    // The composer text whose autocomplete Esc dismissed.
    let mut dismissed: Option<String> = None;
    // When the running sign-in was last polled, and when the daemon last answered.
    let mut poll = Poller::default();
    // `!cmd` results, shown and held for the owner's next message.
    let (ran_tx, mut rans) = mpsc::unbounded_channel::<shell::Ran>();
    let mut held: Vec<shell::Ran> = vec![];
    // Where the left button went down, whether with Shift, and which click of a burst it was,
    // to tell a click from a drag.
    let mut press: Option<((u16, u16), bool, u8)> = None;
    let mut clicks = Clicks::default();
    // The last plain click, in transcript rows: Shift+click selects from here.
    let mut anchor: Option<(u16, usize)> = None;
    let mut attention = Attention::default();
    // The code block the last ^X copied, so the next one copies the block before it.
    let mut walk: Option<usize> = None;
    // What you sent, the draft ^C cleared, and the drafts Alt+S put aside.
    let mut history = history::History::default();
    let (home, projects) = (cfg.home.clone(), cfg.projects.clone());
    // The way a Ctrl+P waiting for `zephyr.models`' answer goes. It uses `s.models`, the last
    // answer. shortcut: kept until the /model panel asks again, so a new /login account's list
    // shows up there first; ask on every Ctrl+P if that bites.
    let mut cycle: Option<bool> = None;
    // The vault entry just saved, and whether its name was already there: its card is shown
    // when the daemon's answer lists it.
    let mut saving: Option<(String, bool)> = None;
    let outcome: anyhow::Result<()> = loop {
        tokio::select! {
            msg = msgs.recv() => {
                let Some(msg) = msg else { break Ok(()) };
                let mut s = state.lock().unwrap();
                match msg {
                    Msg::Up { snapshot, messages } => {
                        if !was_up {
                            s.greeted = Some(crate::unix_millis() as i64);
                            history.fill(&messages);
                        }
                        // The names `/vault rm` and the like complete from.
                        let _ = prompt_tx.send(Ask::VaultList);
                        s.link = Link::Up;
                        s.snapshot = *snapshot;
                        s.retry = None;
                        s.stopping = false;
                        view.replay(&messages);
                        s.context = messages.iter().rev().find_map(context_tokens);
                        if was_up {
                            view.notice("reconnected");
                        }
                        was_up = true;
                        for text in s.unsent.drain(..) {
                            let _ = prompt_tx.send(Ask::Prompt(text));
                        }
                        let _ = prompt_tx.send(Ask::Sessions);
                    }
                    Msg::Event(ev) => {
                        let waited = s.snapshot.proposals;
                        let ping = event(&mut s, &mut view, &ev)
                            .and_then(|what| attention.ping(what, crate::unix_millis() as i64));
                        // A proposal came or went while the settings panel is open: list them again.
                        let settings = s.overlay.as_ref().is_some_and(|o| o.kind == Kind::Settings);
                        if settings && s.snapshot.proposals != waited {
                            let _ = prompt_tx.send(Ask::Proposals);
                        }
                        if let Some(ping) = ping {
                            tui.terminal().write(&ping);
                            tui.terminal().flush();
                        }
                        // These change which sessions there are, or their names.
                        let command = ev["command"].as_str().unwrap_or_default();
                        let renamed = matches!(
                            command.split_whitespace().next(),
                            Some("/rename" | "/fork" | "/switch" | "/new" | "/clear")
                        );
                        if ev["type"] == "zephyr_reply" && renamed {
                            let _ = prompt_tx.send(Ask::Sessions);
                        }
                    }
                    Msg::Sessions(list) => s.sessions = list,
                    Msg::Turns(list) => {
                        if let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Rewind) {
                            o.turns = Some(list);
                        }
                    }
                    Msg::Models(list) => {
                        s.models = list.clone();
                        if let Some(back) = cycle.take() {
                            if let Some(m) = next_model(&s.snapshot, &list, back) {
                                send(&mut s, &prompt_tx, format!("/model {m}"));
                            }
                        }
                        // The settings panel's model editor is the same picker.
                        let picking = |o: &&mut Overlay| matches!(o.kind, Kind::Model | Kind::Settings);
                        if let Some(o) = s.overlay.as_mut().filter(picking) {
                            o.models = Some(list.clone());
                            o.placed = false;
                        }
                    }
                    Msg::Dequeued(texts) => {
                        let n = texts.len();
                        let said = match n {
                            0 => "nothing waiting to take back".to_string(),
                            1 => "took back 1 waiting message".to_string(),
                            n => format!("took back {n} waiting messages"),
                        };
                        if n > 0 {
                            // Oldest first, ahead of whatever was being typed.
                            let mut all = texts;
                            all.extend(Some(editor.get_expanded_text()).filter(|d| !d.is_empty()));
                            editor.set_text(&all.join("\n\n"));
                        }
                        s.hint = Some((said, crate::unix_millis() as i64 + COPIED_MS));
                    }
                    Msg::Providers(list) => {
                        if let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Login) {
                            o.login.providers = Some(list);
                            o.placed = false;
                        }
                    }
                    Msg::LoginStarted(login) => {
                        take_start(&mut s, &prompt_tx, &login, &mut poll);
                    }
                    Msg::Login(login_id, status) => {
                        let now = crate::unix_millis() as i64;
                        let mut ended = None;
                        let mut mine = false;
                        let open = s.overlay.as_mut().filter(|o| o.kind == Kind::Login);
                        // The answer is read only if it is about the sign-in on screen: one
                        // for a login the panel has given up on (cancelled, restarted, or
                        // answered slowly) says nothing about the one running now.
                        if let Some(o) = open
                            .filter(|o| o.login.running() == Some(login_id.as_str()))
                        {
                            mine = true;
                            ended = match status.phase.as_str() {
                                // `done` and `failed` end it; every other phase keeps polling.
                                "done" => Some(Ok((status.account.clone(), status.model.clone()))),
                                "failed" => Some(Err(status.error.clone().unwrap_or_default())),
                                _ => None,
                            };
                            o.login.status = Some(*status);
                        }
                        if mine {
                            poll.answered(&s, now);
                        }
                        if let Some(ended) = ended {
                            close(&tui, &editor, &mut s);
                            match ended {
                                Ok((account, model)) => {
                                    let mut said = String::from("signed in");
                                    if let Some(account) = account {
                                        said.push_str(&format!(" as {account}"));
                                    }
                                    if let Some(model) = model {
                                        said.push_str(&format!(" ({model})"));
                                    }
                                    view.notice(&said);
                                }
                                Err(error) if error.is_empty() => view.error("the sign-in failed"),
                                Err(error) => view.error(&error),
                            }
                        }
                    }
                    Msg::LoginGone(login_id) => {
                        // The daemon answered `unknown login`: the login was dropped by its idle
                        // deadline, or refused by the eight-pending cap, so there is nothing
                        // left to poll, type into or cancel. The panel ends without a word —
                        // the user asked to stop or typed into nothing — and only a daemon that
                        // stops answering at all is worth `GIVE_UP`.
                        if login_gone(&s, &login_id, &mut poll) {
                            close(&tui, &editor, &mut s);
                        }
                    }
                    Msg::LoginError(e) => {
                        // Starting was refused (already signed in, unknown provider), the
                        // daemon lost the login, or it refused the token just typed: the panel
                        // cannot carry on, so say it in the transcript, where it stays readable.
                        if login_over(&s) {
                            close(&tui, &editor, &mut s);
                            // The panel is gone, so the poll that was counting for it goes too:
                            // a lost answer must not wedge the next sign-in.
                            poll.forget();
                            view.error(&e);
                        }
                    }
                    Msg::Vault(list) => {
                        if let Some((name, updated)) = saving.take() {
                            if let Some(e) = list.iter().find(|e| e.name == name) {
                                view.reply(Some("/vault"), &e.saved(updated));
                            }
                        }
                        s.vault = list.iter().map(|e| e.name.clone()).collect();
                        if let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Vault) {
                            o.vault.entries = Some(list);
                            o.vault.problem = None;
                        }
                    }
                    Msg::VaultError(e) => {
                        saving = None;
                        match s.overlay.as_mut().filter(|o| o.kind == Kind::Vault) {
                            Some(o) => o.vault.problem = Some(e),
                            None => view.error(&e),
                        }
                    }
                    Msg::Settings(v) => {
                        if let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Settings) {
                            o.settings.load(&v);
                        }
                    }
                    Msg::Proposals(v) => {
                        if let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Settings) {
                            o.settings.load_waiting(&v);
                        }
                    }
                    Msg::SettingResult(v) => {
                        match s.overlay.as_mut().filter(|o| o.kind == Kind::Settings) {
                            Some(o) => {
                                o.settings.answered(&v, &tz);
                                // The values and their sources moved on.
                                let _ = prompt_tx.send(Ask::Settings);
                            }
                            // The panel closed before the answer came: say it in the transcript.
                            None => {
                                let mut panel = settings_panel::SettingsPanel::default();
                                panel.answered(&v, &tz);
                                match panel.note {
                                    Some((true, said)) => view.notice(&said),
                                    Some((false, why)) => view.error(&why),
                                    None => {}
                                }
                            }
                        }
                    }
                    Msg::Decided(v) => match s.overlay.as_mut().filter(|o| o.kind == Kind::Settings) {
                        Some(o) => {
                            o.settings.decided(&v);
                            let _ = prompt_tx.send(Ask::Settings);
                            let _ = prompt_tx.send(Ask::Proposals);
                        }
                        // As `/ok N` would have answered it.
                        None => view.reply(None, v["text"].as_str().unwrap_or_default()),
                    },
                    Msg::Facts(list) => {
                        if let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Settings) {
                            // The Approvals section's row names the facts.
                            if o.settings.load_facts(list) {
                                let _ = prompt_tx.send(Ask::Settings);
                            }
                        }
                    }
                    Msg::FactsError(why) => {
                        match s.overlay.as_mut().filter(|o| o.kind == Kind::Settings) {
                            Some(o) => o.settings.facts_refused(&why),
                            None => view.error(&why),
                        }
                    }
                    Msg::Down(why) if was_up => {
                        // The answer to a poll in flight is lost with the connection; the sign-in
                        // is asked about again once the link is back, not waited on.
                        poll.forget();
                        s.link = Link::Down { why };
                    }
                    Msg::Down(why) => s.link = Link::Connecting(Some(why)),
                    Msg::Fatal(e) => break Err(anyhow::anyhow!(e)),
                }
                attend(&tui, &s, &mut look);
                drop(s);
                tui.request_render(false);
            }
            input = inputs.recv() => {
                let Some(input) = input else { break Ok(()) };
                let mut edit_draft = false;
                match input {
                    Event::Resize(..) => tui.note_resize(),
                    // Selection as upstream's `interactive_tui` wires it: rpi-tui tracks and
                    // highlights it, and a drag is copied when the button comes up.
                    Event::Mouse(m) if !tui.handle_scroll_to_end_indicator_mouse_event(&m) => {
                        match m.kind {
                            MouseEventKind::ScrollUp => tui.scroll_by(-3),
                            MouseEventKind::ScrollDown => tui.scroll_by(3),
                            MouseEventKind::Down(MouseButton::Left) => {
                                tui.handle_selection_mouse_event(&m);
                                let cell = (m.column, m.row);
                                let n = clicks.down(cell, crate::unix_millis() as i64);
                                press = Some((cell, m.modifiers.contains(KeyModifiers::SHIFT), n));
                                walk = None;
                            }
                            MouseEventKind::Drag(MouseButton::Left) => {
                                tui.handle_selection_mouse_event(&m);
                            }
                            MouseEventKind::Up(MouseButton::Left) => {
                                tui.handle_selection_mouse_event(&m);
                                let clicked = press.take().filter(|p| p.0 == (m.column, m.row));
                                let mut s = state.lock().unwrap();
                                let at = clicked
                                    .filter(|_| s.overlay.is_none())
                                    .and_then(|_| placed(&transcript, &s, &palette));
                                if let (Some((cell, shift, n)), Some(at)) = (clicked, &at) {
                                    if let Some((from, to)) = click_span(at, anchor, cell, shift, n) {
                                        drag(&tui, from, to);
                                    }
                                }
                                if let Some(text) = tui.selection_text() {
                                    copy(&tui, &mut s, &text, None);
                                } else if let (Some((cell, ..)), Some(at)) = (clicked, at) {
                                    let row = at.row(cell.1);
                                    anchor = row.map(|r| (cell.0, r));
                                    let n = row.and_then(|r| view::code_at(&at.lines, r));
                                    if let Some((n, text)) = n.and_then(|n| Some((n, view.code(n)?))) {
                                        copy(&tui, &mut s, text, Some(&format!("#{n}")));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                    Event::Mouse(_) => tui.clear_selection(),
                    Event::FocusLost => attention.away = true,
                    Event::FocusGained => attention.away = false,
                    Event::Paste(text) if state.lock().unwrap().overlay.is_none() => {
                        match files::dragged(&text, &home, projects.as_deref()) {
                            Some(Ok(reference)) => editor.insert(&reference),
                            Some(Err(hint)) => {
                                editor.handle_paste(&text);
                                view.notice(&hint);
                            }
                            None => editor.handle_paste(&text),
                        }
                    }
                    Event::Paste(text) => {
                        if let Some(o) = state.lock().unwrap().overlay.as_mut() {
                            overlays::paste(o, &text);
                        }
                    }
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        let now = crate::unix_millis() as i64;
                        let walked = walk.take();
                        let (busy, panel_open, completing) = {
                            let s = state.lock().unwrap();
                            (s.snapshot.run.busy, s.overlay.is_some(), s.complete.is_some())
                        };
                        let text = editor.get_text();
                        let empty = text.is_empty();
                        let plain = key.modifiers.is_empty();
                        let mut s = state.lock().unwrap();
                        let find = s.search.as_ref().and_then(|_| find_key(&key));
                        match key {
                            k if ctrl(&k, 'd') && empty => break Ok(()),
                            k if panel_open => {
                                let snap = s.snapshot.clone();
                                let o = s.overlay.as_mut().expect("open");
                                let kind = o.kind;
                                let cols = crossterm::terminal::size().map_or(80, |(w, _)| w as usize);
                                let rows = overlays::rows(o, &snap, &palette, &tz, now, cols);
                                let running = o.login.running().map(str::to_string);
                                let action = if ctrl(&k, 'c') {
                                    match running {
                                        // Ctrl+C gives up on the sign-in, not just on the panel:
                                        // leaving a login process waiting for nothing helps no one.
                                        Some(id) if kind == Kind::Login => Action::Cancel(id),
                                        _ => Action::Close,
                                    }
                                } else {
                                    overlays::key(o, k, &rows)
                                };
                                match action {
                                    Action::None => {}
                                    Action::Close => close(&tui, &editor, &mut s),
                                    Action::Pick(cmd) => {
                                        let back = s.overlay.as_ref().and_then(|o| overlays::rewound(o, &cmd));
                                        close(&tui, &editor, &mut s);
                                        match kind {
                                            Kind::Model => send(&mut s, &prompt_tx, cmd),
                                            Kind::History => recall(&editor, &cmd),
                                            // The message taken back, to edit and send again.
                                            Kind::Rewind => {
                                                send(&mut s, &prompt_tx, cmd);
                                                editor.set_text(&back.unwrap_or_default());
                                            }
                                            _ if cmd.starts_with(['/', '!']) => editor.set_text(&cmd),
                                            _ => {}
                                        }
                                    }
                                    Action::Login(id) => start(&mut s, &prompt_tx, id),
                                    Action::Input(text) => send_token(&mut s, &prompt_tx, text),
                                    Action::Cancel(id) => {
                                        // Esc gives up on the sign-in with the panel: the cancel
                                        // tells the daemon to kill its child, and closing leaves
                                        // no prompt behind waiting for a token.
                                        let _ = prompt_tx.send(Ask::LoginCancel { login_id: id });
                                        close(&tui, &editor, &mut s);
                                    }
                                    Action::VaultPut(entry) => {
                                        let had = s.vault.contains(&entry.name);
                                        saving = Some((entry.name.clone(), had));
                                        let _ = prompt_tx.send(Ask::VaultPut(entry));
                                    }
                                    Action::VaultRm(name) => {
                                        let _ = prompt_tx.send(Ask::VaultRm(name));
                                    }
                                    Action::VaultRevoke(name) => {
                                        let _ = prompt_tx.send(Ask::VaultRevoke(name));
                                    }
                                    Action::SettingsSet { key, value, until } => {
                                        let _ = prompt_tx.send(Ask::SettingsSet { key, value, until });
                                    }
                                    Action::Decide { id, verb } => {
                                        let _ = prompt_tx.send(Ask::Decide { id, verb });
                                    }
                                    Action::FactsPut(fact) => {
                                        let _ = prompt_tx.send(Ask::FactsPut(fact));
                                    }
                                    Action::FactsRm(name) => {
                                        let _ = prompt_tx.send(Ask::FactsRm(name));
                                    }
                                }
                            }
                            _ if find.is_some() => {
                                let key = find.expect("searching");
                                search(&mut s, key, &transcript, &marked, &palette);
                            }
                            k if ctrl(&k, 'f') => match s.search.take() {
                                Some(_) => marked.row.store(usize::MAX, Ordering::Relaxed),
                                None => s.search = Some(Search::default()),
                            },
                            KeyEvent {
                                code: KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Enter | KeyCode::Esc,
                                ..
                            } if completing
                                && plain
                                // A recalled `/command` keeps Up and Down for the history walk.
                                && !(matches!(key.code, KeyCode::Up | KeyCode::Down) && history.walking(&text)) =>
                            {
                                let c = s.complete.as_mut().expect("completing");
                                let n = c.items.len();
                                let choice = c.items[c.selected].clone();
                                let before = before_cursor(&editor);
                                // Something left to complete, not a command's arguments being typed.
                                let partial = choice.insert != before;
                                match key.code {
                                    KeyCode::Up => c.selected = (c.selected + n - 1) % n,
                                    KeyCode::Down => c.selected = (c.selected + 1) % n,
                                    KeyCode::Esc => {
                                        s.complete = None;
                                        dismissed = Some(text.clone());
                                    }
                                    KeyCode::Tab if partial => take(&editor, &before, &choice.insert),
                                    KeyCode::Tab => {}
                                    // Enter takes the choice, and runs it when it needs nothing more.
                                    _ if partial && !choice.then_run => {
                                        take(&editor, &before, &choice.insert)
                                    }
                                    _ => {
                                        if partial {
                                            take(&editor, &before, &choice.insert);
                                        }
                                        drop(s);
                                        editor.handle_key(key);
                                    }
                                }
                            }
                            k if ctrl(&k, 'l') => {
                                open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Model)
                            }
                            k if ctrl(&k, 's') => {
                                open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Status)
                            }
                            KeyEvent { code: KeyCode::Char('?'), modifiers, .. }
                                if empty && !modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Help)
                            }
                            k if ctrl(&k, 'c') => {
                                if !empty {
                                    history.cleared = Some(editor.get_expanded_text());
                                    editor.clear();
                                } else if now - ctrl_c_at < DOUBLE_PRESS_MS {
                                    break Ok(());
                                } else {
                                    let hint = "press ^C again to detach".to_string();
                                    s.hint = Some((hint, now + DOUBLE_PRESS_MS));
                                }
                                ctrl_c_at = now;
                            }
                            k if ctrl(&k, 'o') => {
                                view.toggle_expanded();
                            }
                            k if ctrl(&k, 't') => {
                                view.toggle_thinking();
                            }
                            // Upstream's copy key; a selection in the composer keeps it for cut.
                            k if ctrl(&k, 'x') && !editor.has_selection() => {
                                if let Some(text) = tui.selection_text() {
                                    copy(&tui, &mut s, &text, None);
                                } else {
                                    // The newest block, then each press the one before it.
                                    let n = walked.filter(|&n| n > 1).map_or(view.codes(), |n| n - 1);
                                    match view.code(n) {
                                        Some(text) => {
                                            copy(&tui, &mut s, text, Some(&format!("#{n}")));
                                            walk = Some(n);
                                        }
                                        None => s.hint = Some(("no code block to copy".into(), now + COPIED_MS)),
                                    }
                                }
                            }
                            KeyEvent { code: KeyCode::Esc, .. } if busy => {
                                if !s.stopping && s.link == Link::Up {
                                    s.stopping = true;
                                    let _ = prompt_tx.send(Ask::Prompt("/stop".into()));
                                }
                            }
                            KeyEvent { code: KeyCode::Esc, .. } if empty && !held.is_empty() => {
                                held.clear();
                                let until = now + DOUBLE_PRESS_MS;
                                s.hint = Some(("dropped the command output".into(), until));
                            }
                            KeyEvent { code: KeyCode::Up, modifiers: KeyModifiers::NONE, .. }
                                if editor.cursor_position().0 == 0 =>
                            {
                                match history.up(&editor.get_expanded_text()) {
                                    Some(earlier) => recall(&editor, &earlier),
                                    None => {
                                        drop(s);
                                        editor.handle_key(key);
                                    }
                                }
                            }
                            KeyEvent { code: KeyCode::Down, modifiers: KeyModifiers::NONE, .. }
                                if editor.cursor_position().0 + 1 == editor.line_count() =>
                            {
                                match history.down(&editor.get_expanded_text()) {
                                    Some(later) => recall(&editor, &later),
                                    None => {
                                        drop(s);
                                        editor.handle_key(key);
                                    }
                                }
                            }
                            k if ctrl(&k, 'r') => {
                                open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::History);
                                if let Some(o) = s.overlay.as_mut() {
                                    o.history = history.newest_first();
                                }
                            }
                            KeyEvent { code: KeyCode::Char('s'), modifiers: KeyModifiers::ALT, .. } => {
                                let said = match history.stash(&editor.get_expanded_text()) {
                                    Some(back) if !back.is_empty() => {
                                        recall(&editor, &back);
                                        format!("{} more put aside", history.stashed())
                                    }
                                    Some(_) => {
                                        editor.clear();
                                        "put aside; alt+s on an empty box brings it back".into()
                                    }
                                    None => "nothing put aside".into(),
                                };
                                s.hint = Some((said, now + COPIED_MS));
                            }
                            // Run below, once the state's lock is let go: it is awaited.
                            k if ctrl(&k, 'g') => edit_draft = true,
                            // A follow-up: it waits for the reply to end instead of steering in.
                            KeyEvent { code: KeyCode::Enter, modifiers: KeyModifiers::ALT, .. } => {
                                drop(s);
                                follow.store(true, Ordering::Relaxed);
                                editor.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
                                follow.store(false, Ordering::Relaxed);
                            }
                            KeyEvent { code: KeyCode::Up, modifiers: KeyModifiers::ALT, .. } if s.link == Link::Up => {
                                let _ = prompt_tx.send(Ask::Dequeue);
                            }
                            KeyEvent { code: KeyCode::BackTab, .. } => {
                                send(&mut s, &prompt_tx, "/thinking next".into())
                            }
                            KeyEvent { code: KeyCode::Char(c @ ('p' | 'P')), modifiers, .. }
                                if modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                let back = c == 'P' || modifiers.contains(KeyModifiers::SHIFT);
                                if s.models.is_empty() {
                                    cycle = Some(back);
                                    let _ = prompt_tx.send(Ask::Models);
                                } else if let Some(m) = next_model(&s.snapshot, &s.models, back) {
                                    send(&mut s, &prompt_tx, format!("/model {m}"));
                                }
                            }
                            KeyEvent { code: KeyCode::Esc, .. } if empty => {
                                if now - esc_at < REWIND_MS {
                                    esc_at = 0;
                                    open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Rewind);
                                } else {
                                    esc_at = now;
                                }
                            }
                            KeyEvent { code: KeyCode::PageUp, modifiers: KeyModifiers::NONE, .. } => {
                                tui.scroll_by(-page())
                            }
                            KeyEvent { code: KeyCode::PageDown, modifiers: KeyModifiers::NONE, .. } => {
                                tui.scroll_by(page())
                            }
                            KeyEvent {
                                code: code @ (KeyCode::Up | KeyCode::Down),
                                modifiers: KeyModifiers::CONTROL,
                                ..
                            } => {
                                let up = code == KeyCode::Up;
                                let at = placed(&transcript, &s, &palette);
                                match at.and_then(|at| jump(&view::prompts(&at.lines), at.top, up)) {
                                    Some(row) => transcript.scroll_to(row),
                                    None if !up => tui.scroll_to_bottom(),
                                    None => {}
                                }
                            }
                            KeyEvent { code: KeyCode::Home, modifiers: KeyModifiers::NONE, .. } if empty => {
                                tui.scroll_to_top()
                            }
                            KeyEvent { code: KeyCode::End, modifiers: KeyModifiers::NONE, .. } if empty => {
                                tui.scroll_to_bottom()
                            }
                            k => {
                                drop(s);
                                editor.handle_key(k);
                            }
                        }
                    }
                    _ => {}
                }
                if edit_draft {
                    let set = |k: &str| std::env::var(k).ok().filter(|e| !e.trim().is_empty());
                    let edit = set("VISUAL").or_else(|| set("EDITOR")).unwrap_or_else(|| "nano".into());
                    let (home, draft, ed) = (home.clone(), editor.get_expanded_text(), edit.clone());
                    let edited = hand_over(&tui, &paused, &reading, move || shell::edit(&home, &ed, &draft)).await;
                    match edited.map_err(std::io::Error::other).and_then(|r| r) {
                        Ok(Some(text)) => recall(&editor, &text),
                        Ok(None) => view.error(&format!("{edit} failed; the draft is as it was")),
                        Err(e) => view.error(&format!("{edit}: {e}")),
                    }
                    tui.request_render(true);
                }
                let mut s = state.lock().unwrap();
                complete(&mut s, &editor, view.codes(), &home, projects.as_deref(), &mut dismissed);
                drop(s);
                tui.request_render(false);
            }
            text = submits.recv() => {
                let Some((text, after)) = text else { break Ok(()) };
                // In a guard of its own: one borrowed on the way to `/shell` counts as held
                // across its await, `drop` or not.
                {
                    let mut s = state.lock().unwrap();
                    // shortcut: an OK given on another channel leaves the title waiting until the
                    // owner types here; read it from the snapshot if the daemon ever reports
                    // proposals.
                    s.asking &= text.trim().is_empty();
                    attend(&tui, &s, &mut look);
                }
                let mut s = state.lock().unwrap();
                walk = None;
                history.push(&text);
                // These open a panel here instead of a reply card, or never leave the TUI.
                match text.trim() {
                    "" => {}
                    "/focus" => s.focus = view.toggle_focus(),
                    "/quit" | "/exit" => break Ok(()),
                    "/p" => view.reply(Some("/p"), &files::listing(&home)),
                    // Into the composer, never sent: you read what a saved prompt says first.
                    t if t.starts_with("/p ") => {
                        let (name, args) = t[3..].trim_start().split_once(' ').unwrap_or((t[3..].trim(), ""));
                        match files::prompt(&home, name, args) {
                            Ok(text) => recall(&editor, &text),
                            Err(e) => view.error(&e),
                        }
                    }
                    t if t == "/copy" || t.starts_with("/copy ") => match to_copy(&view, &t[5..]) {
                        Ok((text, what)) => copy(&tui, &mut s, &text, Some(&what)),
                        Err(e) => s.hint = Some((e, crate::unix_millis() as i64 + COPIED_MS)),
                    },
                    "/help" => open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Help),
                    "/status" => open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Status),
                    "/model" => open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Model),
                    "/login" => open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Login),
                    "/vault" => open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Vault),
                    "/settings" => open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Settings),
                    // A value is typed into the vault's form, never sent as a message.
                    text if text.starts_with("/vault add") || text.starts_with("/vault edit") => {
                        match overlays::Form::from_command(text) {
                            Some(form) => {
                                open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Vault);
                                if let Some(o) = s.overlay.as_mut() {
                                    o.vault.form = Some(form);
                                }
                            }
                            None => view.error(
                                "not sent: /vault add login NAME SITE or /vault add token NAME \
                                 opens a form for the value; /vault edits a saved entry",
                            ),
                        }
                    }
                    "/shell" | "/shell bash" | "/shell zsh" => {
                        drop(s);
                        let sh = if text.trim().ends_with("zsh") { "zsh" } else { "bash" };
                        let home = home.clone();
                        let ran = hand_over(&tui, &paused, &reading, move || shell::interactive(&home, sh)).await;
                        // Back from the shell, the owner is looking.
                        attention.away = false;
                        match ran.map_err(std::io::Error::other).and_then(|r| r) {
                            Ok(_) => view.notice(&format!("back from {sh}")),
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                                view.error(&format!("{sh} is not installed in the container"))
                            }
                            Err(e) => view.error(&format!("{sh}: {e}")),
                        }
                        tui.request_render(true);
                    }
                    text => match shell::bang(text) {
                        Some(command) => {
                            let (home, command, tx) = (cfg.home.clone(), command.to_string(), ran_tx.clone());
                            tokio::task::spawn_blocking(move || {
                                let _ = tx.send(shell::run(&home, &command, shell::LIMIT));
                            });
                        }
                        // A `/` command is the gateway's, not a message: the output waits.
                        None if text.starts_with('/') => send(&mut s, &prompt_tx, text.to_string()),
                        None => {
                            let text = files::expand(text, &home, projects.as_deref());
                            let text = shell::prefix(&std::mem::take(&mut held), &text);
                            // shortcut: offline, a follow-up waits as a plain message; keep the
                            // flag in `unsent` if that matters.
                            match after && s.link == Link::Up {
                                true => drop(prompt_tx.send(Ask::FollowUp(text))),
                                false => send(&mut s, &prompt_tx, text),
                            }
                        }
                    },
                }
                tui.request_render(false);
            }
            Some(ran) = rans.recv() => {
                let (title, body) = ran.card();
                view.reply(Some(&title), &body);
                held.push(ran);
                tui.request_render(false);
            }
            _ = ticker.tick() => {
                tui.ensure_size();
                let mut s = state.lock().unwrap();
                let now = crate::unix_millis() as i64;
                let hint_over = s.hint.as_ref().is_some_and(|(_, until)| now >= *until);
                if hint_over {
                    s.hint = None;
                }
                match login_poll(&s, now, &mut poll) {
                    Poll::Ask(login_id) => {
                        let _ = prompt_tx.send(Ask::LoginPoll { login_id });
                    }
                    Poll::Silent(login_id) => give_up(&mut s, &prompt_tx, &login_id, &mut poll),
                    Poll::Idle => {}
                }
                let frame = chrome::mascot(chrome::Mood::of(&s, now), &palette, now);
                let new_face = frame != face;
                face = frame;
                if hint_over || new_face || s.animated() || view.running() {
                    tui.request_render(false);
                }
            }
        }
    };
    stop.store(true, Ordering::Relaxed);
    let _ = reader.await;
    tui.terminal().write(PROGRESS_OFF);
    focus_reports(&tui, false);
    tui.stop(Default::default());
    outcome?;
    println!("detached — zephyr keeps running (zephyr attach to return)");
    Ok(())
}

/// Routes one live event: zephyr's own to the chrome, the agent's to the transcript. Returns
/// what to notify the owner of, if they are away: a reply ready, or something needing their OK.
fn event(s: &mut State, view: &mut view::View, ev: &Value) -> Option<&'static str> {
    let mut ping = None;
    match ev["type"].as_str().unwrap_or_default() {
        "zephyr_status" => {
            let was_busy = s.snapshot.run.busy;
            s.snapshot = serde_json::from_value(ev["snapshot"].clone()).unwrap_or_default();
            if !s.snapshot.run.busy {
                // A run the user stopped gets no cheer.
                if was_busy && !s.stopping {
                    s.finished = Some(crate::unix_millis() as i64);
                    ping = Some("reply ready");
                }
                s.stopping = false;
            }
        }
        "zephyr_reply" => {
            let text = ev["text"].as_str().unwrap_or_default();
            // A notice, not an answer to a command typed here, naming the OK that approves it:
            // `/ok 3`, `/job ok 3`, `/skill ok tea`, `/memory ok`.
            if ev["command"].is_null()
                && text.starts_with("[zephyr]")
                && (text.contains("/ok ") || text.contains(" ok "))
            {
                s.asking = true;
                ping = Some("needs your OK");
            }
            view.reply(ev["command"].as_str(), text)
        }
        "retry_scheduled" => {
            s.retry = Some(Retry {
                attempt: ev["attempt"].as_u64().unwrap_or(0),
                max: ev["maxRetries"].as_u64().unwrap_or(0),
                until_ms: crate::unix_millis() as i64 + ev["delayMs"].as_i64().unwrap_or(0),
                error: ev["error"].as_str().unwrap_or_default().to_string(),
            });
        }
        kind => {
            if matches!(kind, "message_start" | "agent_settled" | "agent_end") {
                s.retry = None;
            }
            if matches!(kind, "agent_settled" | "agent_end") {
                s.stopping = false;
            }
            if kind == "message_end" {
                s.context = context_tokens(&ev["message"]).or(s.context);
            }
            view.apply(ev);
        }
    }
    ping
}

/// The tokens in the model's context after an assistant message: its usage, which counts the
/// whole request. `None` for other messages and for a reply that failed or was cut off.
fn context_tokens(m: &Value) -> Option<i64> {
    let u = &m["usage"];
    let used: i64 = ["input", "cacheRead", "cacheWrite", "output"]
        .iter()
        .filter_map(|k| u[*k].as_i64())
        .sum();
    let failed = matches!(m["stopReason"].as_str(), Some("aborted" | "error"));
    (m["role"] == "assistant" && used > 0 && !failed).then_some(used)
}

/// The model after (or, `back`, before) the current one in its account's list, for Ctrl+P.
fn next_model(snap: &conn::Snapshot, list: &[conn::AccountModels], back: bool) -> Option<String> {
    let models = &list.iter().find(|a| a.name == snap.account)?.models;
    let n = models.len();
    let next = match models.iter().position(|m| Some(m) == snap.model.as_ref()) {
        Some(i) if back => (i + n - 1) % n,
        Some(i) => (i + 1) % n,
        None => 0,
    };
    models.get(next).cloned()
}

/// Sends a prompt now, or keeps it for the next connection.
fn send(s: &mut State, asks: &mpsc::UnboundedSender<Ask>, text: String) {
    if s.link == Link::Up {
        let _ = asks.send(Ask::Prompt(text));
    } else {
        s.unsent.push(text);
    }
}

/// Puts text on the clipboard with OSC 52 and says so in the footer. A terminal without OSC 52
/// (macOS Terminal.app, tmux without `set-clipboard on`) drops it without a word.
fn copy(tui: &TuiAltScreen, s: &mut State, text: &str, what: Option<&str>) {
    tui.terminal().write(&osc52(text));
    tui.terminal().flush();
    let lines = match text.lines().count() {
        0 | 1 => "1 line".to_string(),
        n => format!("{n} lines"),
    };
    let said = match what {
        Some(what) => format!("copied {what} ({lines})"),
        None => format!("copied {lines}"),
    };
    s.hint = Some((said, crate::unix_millis() as i64 + COPIED_MS));
}

fn osc52(text: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text);
    format!("\x1b]52;c;{b64}\x07")
}

/// What `/copy [n]` copies, and its name in the footer: the last reply, or code block `n`.
fn to_copy(view: &view::View, arg: &str) -> Result<(String, String), String> {
    match arg.trim() {
        "" => (view.last_reply())
            .map(|t| (t.to_string(), "the last reply".to_string()))
            .ok_or_else(|| "no reply to copy yet".to_string()),
        n => (n.trim_start_matches('#').parse().ok())
            .and_then(|k| Some((view.code(k)?.to_string(), format!("#{k}"))))
            .ok_or_else(|| format!("no code block {n}")),
    }
}

/// The transcript on screen: below the header as `Panel::render` measures it, its rows as last
/// drawn. A click's code block, word and row, Shift+click's anchor, the search and the prompt
/// jumps all read transcript rows through it.
struct Placed {
    /// The screen row of its first row.
    header: usize,
    /// How many rows of it are on screen.
    height: usize,
    /// The transcript row at its top.
    top: usize,
    lines: Vec<String>,
}

impl Placed {
    /// The transcript row under screen row `row`.
    fn row(&self, row: u16) -> Option<usize> {
        let r = (row as usize)
            .checked_sub(self.header)
            .filter(|&r| r < self.height)?;
        Some(self.top + r)
    }

    /// The screen row of transcript row `row`, or of the edge it is scrolled past.
    fn screen(&self, row: usize) -> u16 {
        let last = self.top + self.height.saturating_sub(1);
        (self.header + row.clamp(self.top, last) - self.top) as u16
    }
}

fn placed(transcript: &ScrollView, s: &State, p: &theme::Palette) -> Option<Placed> {
    let cols = crossterm::terminal::size().map_or(80, |(w, _)| w as usize);
    let snap = transcript.snapshot(cols)?;
    Some(Placed {
        header: chrome::header(s, p, cols, crate::unix_millis() as i64).len(),
        height: transcript.viewport_height(),
        top: snap.scroll_top,
        lines: snap.document.lines(0, snap.document.len()),
    })
}

/// Counts clicks on one cell within [`CLICK_MS`] of each other: 1, 2, 3, then 1 again.
#[derive(Default)]
struct Clicks {
    cell: (u16, u16),
    at: i64,
    n: u8,
}

impl Clicks {
    fn down(&mut self, cell: (u16, u16), now: i64) -> u8 {
        let again = self.n > 0 && cell == self.cell && now - self.at <= CLICK_MS;
        self.n = if again { self.n % 3 + 1 } else { 1 };
        (self.cell, self.at) = (cell, now);
        self.n
    }
}

/// What a click selects, as the screen cells a drag would run between: from the last plain
/// click's anchor with Shift, the word under it on a second click, its row on a third. rpi-tui's
/// selection has no Shift-extend or multi-click of its own.
fn click_span(
    at: &Placed,
    anchor: Option<(u16, usize)>,
    cell: (u16, u16),
    shift: bool,
    n: u8,
) -> Option<((u16, u16), (u16, u16))> {
    if let Some((col, row)) = anchor.filter(|_| shift) {
        // shortcut: an anchor scrolled out of view starts at the edge row, not its own row;
        // feed rpi-tui content coordinates if it ever takes them.
        return Some(((col, at.screen(row)), cell));
    }
    let text = rpi_tui::strip_ansi(at.lines.get(at.row(cell.1)?)?);
    let (first, last) = match n {
        2 => word_at(&text, cell.0 as usize)?,
        3 => row_span(&text)?,
        _ => return None,
    };
    Some(((first as u16, cell.1), (last as u16, cell.1)))
}

/// Each character of a plain row with its first column and width.
fn cells(text: &str) -> Vec<(usize, usize, char)> {
    let mut col = 0;
    let mut out = vec![];
    for c in text.chars() {
        let w = rpi_tui::visible_width(c.encode_utf8(&mut [0; 4]));
        out.push((col, w, c));
        col += w;
    }
    out
}

/// The first and last column of the word under column `col`: letters, digits and the
/// characters of paths and addresses, as terminals pick a word.
fn word_at(text: &str, col: usize) -> Option<(usize, usize)> {
    let cells = cells(text);
    let word = |i: usize| cells[i].2.is_alphanumeric() || "_-./~:@+".contains(cells[i].2);
    let i = cells.iter().position(|c| (c.0..c.0 + c.1).contains(&col))?;
    if !word(i) {
        return None;
    }
    let first = (0..=i).rev().take_while(|&j| word(j)).last()?;
    let last = (i..cells.len()).take_while(|&j| word(j)).last()?;
    Some((cells[first].0, cells[last].0 + cells[last].1.max(1) - 1))
}

/// The first and last column of a row's text, past the gutter (or a band's bar).
fn row_span(text: &str) -> Option<(usize, usize)> {
    let cells = cells(text);
    let mut text = cells.iter().filter(|c| c.0 >= 3 && !c.2.is_whitespace());
    let first = text.next()?;
    let last = text.next_back().unwrap_or(first);
    Some((first.0, last.0 + last.1.max(1) - 1))
}

/// Feeds rpi-tui's selection a press at `from` dragged to `to`, as a mouse would, so what a
/// Shift+click or a double or triple click selects is highlighted and read like a drag.
fn drag(tui: &TuiAltScreen, from: (u16, u16), to: (u16, u16)) {
    let at = |kind, (column, row): (u16, u16)| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    for ev in [
        at(MouseEventKind::Down(MouseButton::Left), from),
        at(MouseEventKind::Drag(MouseButton::Left), to),
        at(MouseEventKind::Up(MouseButton::Left), to),
    ] {
        tui.handle_selection_mouse_event(&ev);
    }
}

/// A key while the search bar is open.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Find {
    Type(char),
    Back,
    Older,
    Newer,
    Close,
}

/// The search bar's keys; any other key does what it does without the bar.
fn find_key(k: &KeyEvent) -> Option<Find> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let shift = k.modifiers.contains(KeyModifiers::SHIFT);
    Some(match k.code {
        KeyCode::Esc => Find::Close,
        KeyCode::Enter if shift => Find::Newer,
        KeyCode::Enter => Find::Older,
        // Ctrl+Shift+G comes as `G`, or as `g` with Shift, depending on the terminal.
        KeyCode::Char('G') if ctrl => Find::Newer,
        KeyCode::Char('g') if ctrl && shift => Find::Newer,
        KeyCode::Char('g') if ctrl => Find::Older,
        KeyCode::Backspace => Find::Back,
        KeyCode::Char(c) if !ctrl && !k.modifiers.contains(KeyModifiers::ALT) => Find::Type(c),
        _ => return None,
    })
}

/// One search bar key: the query changes or the next match is picked, and the match is scrolled
/// a third of the way down the transcript and marked. rpi-tui's `AltScreenSearch` has no way
/// into the scroll view, so this searches the transcript's rows as drawn.
fn search(
    s: &mut State,
    key: Find,
    transcript: &ScrollView,
    marked: &view::Marked,
    p: &theme::Palette,
) {
    let at = placed(transcript, s, p);
    let lines = at.as_ref().map_or(&[][..], |at| &at.lines[..]);
    let Some(f) = s.search.as_mut().filter(|_| key != Find::Close) else {
        s.search = None;
        marked.row.store(usize::MAX, Ordering::Relaxed);
        return;
    };
    let row = match key {
        Find::Type(c) => {
            f.query.push(c);
            f.refresh(lines);
            f.current()
        }
        Find::Back => {
            f.query.pop();
            f.refresh(lines);
            f.current()
        }
        Find::Older => f.step(true),
        Find::Newer => f.step(false),
        Find::Close => None,
    };
    marked
        .row
        .store(row.unwrap_or(usize::MAX), Ordering::Relaxed);
    if let (Some(row), Some(at)) = (row, at) {
        transcript.scroll_to(row.saturating_sub(at.height / 3));
    }
}

/// Ctrl+Up: the last user turn above the transcript's top row; Ctrl+Down: the first below it.
fn jump(prompts: &[usize], top: usize, up: bool) -> Option<usize> {
    match up {
        true => prompts.iter().rev().find(|&&r| r < top),
        false => prompts.iter().find(|&&r| r > top),
    }
    .copied()
}

/// The terminal's focus, from its focus reports, and when the last notification went out.
#[derive(Default)]
struct Attention {
    /// The terminal said it lost focus. A terminal without focus reports never says so, and
    /// never gets a notification.
    away: bool,
    sent: Option<i64>,
}

impl Attention {
    /// A bell and an OSC 9 notification saying `what`, while the terminal is away and none went
    /// out in the last [`NOTIFY_MS`].
    fn ping(&mut self, what: &str, now: i64) -> Option<String> {
        if !self.away || self.sent.is_some_and(|t| now - t < NOTIFY_MS) {
            return None;
        }
        self.sent = Some(now);
        Some(format!("\x07\x1b]9;zephyr: {what}\x07"))
    }
}

/// Sets the window title and the tab's progress bar from the state, writing only what changed.
fn attend(tui: &TuiAltScreen, s: &State, look: &mut (&'static str, bool)) {
    let now = (chrome::title(s), s.snapshot.run.busy);
    if now.0 != look.0 {
        tui.set_title(now.0);
    }
    if now.1 != look.1 {
        tui.terminal()
            .write(if now.1 { PROGRESS_ON } else { PROGRESS_OFF });
        tui.terminal().flush();
    }
    *look = now;
}

/// Turns the terminal's focus reports on or off: off on the way out and while `/shell` has the
/// terminal, like the mouse.
fn focus_reports(tui: &TuiAltScreen, on: bool) {
    use crossterm::Command;
    let mut seq = String::new();
    let _ = match on {
        true => crossterm::event::EnableFocusChange.write_ansi(&mut seq),
        false => crossterm::event::DisableFocusChange.write_ansi(&mut seq),
    };
    tui.terminal().write(&seq);
    tui.terminal().flush();
}

/// The login picker's Enter: ask the daemon to sign in to that provider. One sign-in at a time,
/// because the panel shows one: a second would have nowhere to go.
fn start(s: &mut State, asks: &mpsc::UnboundedSender<Ask>, provider: String) {
    let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Login) else {
        return;
    };
    // A sign-in already running, or a start still unanswered, means the panel is taken: the
    // second request would open a login with nowhere to be shown.
    if !o.login.ask(&provider) {
        return;
    }
    let _ = asks.send(Ask::LoginStart {
        provider,
        account: None,
        model: None,
    });
}

/// Takes on the sign-in `zephyr.login_start` answered with, or cancels it. Only the start this
/// panel asked for is taken on, and only while none is running: a stale answer for a sign-in
/// from before the panel was reopened must not attach itself to what is on screen now. A start
/// with no panel left to show it is cancelled here — nothing polls it, nothing ever cancels it,
/// and it would hold one of the daemon's sixteen login slots until it finished on its own.
/// Silent on purpose: no notice and no error, the sign-in simply did not happen.
fn take_start(
    s: &mut State,
    asks: &mpsc::UnboundedSender<Ask>,
    login: &Login,
    poll: &mut Poller,
) -> bool {
    let open = s.overlay.as_mut().filter(|o| o.kind == Kind::Login);
    let adopted = open.is_some_and(|o| o.login.adopt(login));
    if adopted {
        // A fresh sign-in: asked about at once, and timed from here.
        poll.follow(s, crate::unix_millis() as i64);
        return true;
    }
    // A panel that is still waiting on a start of its own is not waiting on this one, so only
    // the request this answer belongs to is spent.
    if let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Login) {
        if o.login.requested.as_deref() == Some(login.provider.as_str()) {
            o.login.requested = None;
        }
    }
    let _ = asks.send(Ask::LoginCancel {
        login_id: login.id.clone(),
    });
    false
}

/// Whether the daemon saying this login is gone (`-32602` `unknown login`) ends the panel on
/// screen, and so whether the caller has to close it. An answer about a login the panel no
/// longer runs says nothing about the one it does. The poll of the login that goes is dropped
/// with it: nothing will answer it again, and the next sign-in starts from the clock.
fn login_gone(s: &State, login_id: &str, poll: &mut Poller) -> bool {
    let showing = s
        .overlay
        .as_ref()
        .filter(|o| o.kind == Kind::Login)
        .is_some_and(|o| o.login.running() == Some(login_id));
    if showing {
        poll.forget();
    }
    showing
}

/// The token typed at a `need_input` prompt. The panel cleared it when the key was taken, so
/// this is the only copy.
fn send_token(s: &mut State, asks: &mpsc::UnboundedSender<Ask>, text: String) {
    let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Login) else {
        return;
    };
    let Some(login_id) = o.login.running().map(str::to_string) else {
        return;
    };
    let _ = asks.send(Ask::LoginInput { login_id, text });
}

/// What the tick should do about the sign-in on screen.
enum Poll {
    /// Ask the daemon about this login: the last ask is due for an answer, or never got one.
    Ask(String),
    /// It has not answered a poll for half a minute: the sign-in is over as far as the panel
    /// is concerned, and it says so instead of waiting on a login that will never move again.
    Silent(String),
    /// Nothing is due: no panel, no running sign-in, or the last poll is not answered yet.
    Idle,
}

/// The poll the panel has outstanding: which login it is for, when the last ask went out, and
/// when the daemon last answered one. Both clocks restart with the login, so a late answer for
/// a login the panel gave up on can neither hold the next sign-in back nor age it.
#[derive(Default)]
struct Poller {
    login_id: Option<String>,
    sent: i64,
    answered: i64,
}

impl Poller {
    /// Follows the panel: a closed panel or a sign-in that ended forgets the poll, and a new
    /// one is polled at once rather than after the last sign-in's clock ran out.
    fn follow(&mut self, s: &State, now: i64) {
        let running = s
            .overlay
            .as_ref()
            .filter(|o| o.kind == Kind::Login)
            .and_then(|o| o.login.running());
        match running {
            Some(id) if self.login_id.as_deref() == Some(id) => {}
            Some(id) => {
                self.login_id = Some(id.to_string());
                self.sent = now - LOGIN_POLL_MS;
                self.answered = now;
            }
            None => self.login_id = None,
        }
    }

    /// Notes an answered poll for the running sign-in, which is what keeps it from being given
    /// up on. Only called for an answer about the login this poller is following.
    fn answered(&mut self, s: &State, now: i64) {
        self.follow(s, now);
        if self.login_id.is_some() {
            self.answered = now;
        }
    }

    /// Closes the window on the poll: the answer it waits for will never be read.
    fn forget(&mut self) {
        self.login_id = None;
    }
}

/// What the tick should do about the running sign-in, given the clock: ask again every half
/// second, and stop asking once it has gone unanswered for half a minute.
fn login_poll(s: &State, now: i64, poll: &mut Poller) -> Poll {
    poll.follow(s, now);
    let Some(login_id) = poll.login_id.clone() else {
        return Poll::Idle;
    };
    if now - poll.answered >= LOGIN_SILENCE_MS {
        return Poll::Silent(login_id);
    }
    if now - poll.sent < LOGIN_POLL_MS {
        return Poll::Idle;
    }
    poll.sent = now;
    Poll::Ask(login_id)
}

/// A sign-in the daemon stopped answering: the panel goes back to the provider list with the
/// reason on it, and the daemon is asked to drop the login the panel has given up on. Nothing
/// would read it again, and the daemon may be holding a child process for it.
fn give_up(s: &mut State, asks: &mpsc::UnboundedSender<Ask>, login_id: &str, poll: &mut Poller) {
    let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Login) else {
        return;
    };
    if o.login.running() != Some(login_id) {
        return;
    }
    o.login.reset();
    o.login.problem = Some(GIVE_UP.to_string());
    poll.forget();
    let _ = asks.send(Ask::LoginCancel {
        login_id: login_id.to_string(),
    });
}

/// Whether a `LoginError` takes the panel down. Any open sign-in panel does, and above all a
/// `need_input` prompt: the typed token was taken from the buffer when it was sent, so the panel
/// would wait for a code that can never arrive once the daemon has lost the login.
fn login_over(s: &State) -> bool {
    s.overlay.as_ref().is_some_and(|o| o.kind == Kind::Login)
}

/// Opens a panel over the bottom of the screen, between the header and the footer.
fn open(
    tui: &TuiAltScreen,
    panel: &Arc<dyn Component>,
    editor: &Editor,
    s: &mut State,
    asks: &mpsc::UnboundedSender<Ask>,
    kind: Kind,
) {
    close(tui, editor, s);
    // An unfocused editor draws no cursor marker under the panel (the panel has its own),
    // which would shift the composited row.
    editor.set_focused(false);
    s.overlay = Some(Overlay::new(kind));
    s.complete = None;
    match kind {
        Kind::Model => {
            let _ = asks.send(Ask::Models);
        }
        Kind::Login => {
            let _ = asks.send(Ask::LoginProviders);
        }
        Kind::Vault => {
            let _ = asks.send(Ask::VaultList);
        }
        Kind::Rewind => {
            let _ = asks.send(Ask::Turns);
        }
        // The models, for its model editor, which is the model picker.
        Kind::Settings => {
            let _ = asks.send(Ask::Settings);
            let _ = asks.send(Ask::Proposals);
            let _ = asks.send(Ask::Models);
            let _ = asks.send(Ask::FactsList);
        }
        Kind::Status | Kind::Help | Kind::History => {}
    }
    let margin = OverlayMargin {
        top: 2,
        right: 1,
        bottom: 1,
        left: 1,
    };
    let options = OverlayOptions {
        anchor: OverlayAnchor::BottomLeft,
        margin: Some(margin),
        ..Default::default()
    };
    tui.show_overlay(panel.clone(), Some(options));
}

fn close(tui: &TuiAltScreen, editor: &Editor, s: &mut State) {
    if s.overlay.take().is_some() {
        tui.hide_overlay();
    }
    editor.set_focused(true);
}

/// Keeps autocomplete in step with the composer: a command still being named, else a command's
/// argument, a saved prompt's name or an `@` file, else the usage of the command being typed.
fn complete(
    s: &mut State,
    editor: &Editor,
    codes: usize,
    home: &std::path::Path,
    projects: Option<&std::path::Path>,
    dismissed: &mut Option<String>,
) {
    let text = editor.get_text();
    if s.overlay.is_some() || dismissed.as_deref() == Some(text.as_str()) {
        s.complete = None;
        return;
    }
    *dismissed = None;
    let before = before_cursor(editor);
    let slash = overlays::slash(&before);
    let mut items = vec![];
    if !slash.iter().any(|c| c.insert != before) {
        items = overlays::args(&before, s, codes);
        if items.is_empty() && before.starts_with("/p ") {
            items = overlays::prompt_args(&before, &files::prompts(home));
        }
        if items.is_empty() {
            items = files::complete(&before, home, projects);
        }
    }
    if items.is_empty() {
        items = slash;
    }
    let selected = (s.complete.as_ref())
        .filter(|c| c.items == items)
        .map_or(0, |c| c.selected);
    s.complete = (!items.is_empty()).then_some(overlays::Complete { items, selected });
}

/// The composer's text up to the cursor (the editor's column is a byte offset).
fn before_cursor(editor: &Editor) -> String {
    let text = editor.get_text();
    let (row, col) = editor.cursor_position();
    let start: usize = text.split('\n').take(row).map(|l| l.len() + 1).sum();
    text[..(start + col).min(text.len())].to_string()
}

/// Takes a completion: `before` becomes `insert` by editing only where they differ, so the text
/// after the cursor, a folded paste and undo all stay.
fn take(editor: &Editor, before: &str, insert: &str) {
    let same = before
        .chars()
        .zip(insert.chars())
        .take_while(|(a, b)| a == b);
    let (chars, bytes) = same.fold((0, 0), |(n, b), (c, _)| (n + 1, b + c.len_utf8()));
    for _ in chars..before.chars().count() {
        editor.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE));
    }
    editor.insert(&insert[bytes..]);
}

/// Puts `text` in the composer with the cursor at its end.
fn recall(editor: &Editor, text: &str) {
    editor.set_text(text);
    let last = text.rsplit('\n').next().unwrap_or_default();
    editor.set_cursor(editor.line_count() - 1, last.len());
}

/// Hands the terminal to `run` (`/shell`, Ctrl+G) and takes it back: the input reader paused,
/// the screen suspended. Awaited, so nothing else here touches the screen until it is back.
async fn hand_over<T: Send + 'static>(
    tui: &Arc<TuiAltScreen>,
    paused: &Arc<AtomicBool>,
    reading: &Arc<Mutex<()>>,
    run: impl FnOnce() -> T + Send + 'static,
) -> Result<T, tokio::task::JoinError> {
    let (tui2, paused, reading) = (tui.clone(), paused.clone(), reading.clone());
    let out = tokio::task::spawn_blocking(move || {
        paused.store(true, Ordering::Relaxed);
        let _turn = reading.lock().unwrap();
        focus_reports(&tui2, false);
        tui2.suspend();
        let out = run();
        tui2.resume();
        focus_reports(&tui2, true);
        paused.store(false, Ordering::Relaxed);
        out
    })
    .await;
    tui.note_resize();
    out
}

fn ctrl(key: &KeyEvent, ch: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char(ch)
}

/// Half a screen, so a page keeps some context.
fn page() -> i32 {
    let rows = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
    (rows as i32 / 2).max(1)
}

/// The open panel, drawn from the shared state as an overlay.
struct Panel {
    state: Arc<Mutex<State>>,
    p: theme::Palette,
    tz: jiff::tz::TimeZone,
}

impl Component for Panel {
    fn render(&self, width: usize) -> Vec<String> {
        let (cols, rows) =
            crossterm::terminal::size().map_or((80, 24), |(w, h)| (w as usize, h as usize));
        let now = crate::unix_millis() as i64;
        let mut s = self.state.lock().unwrap();
        // Below the header, above the footer: a panel taller than that scrolls instead.
        let height = rows.saturating_sub(chrome::header(&s, &self.p, cols, now).len() + 1);
        let snap = s.snapshot.clone();
        let Some(o) = s.overlay.as_mut() else {
            return vec![];
        };
        let list = overlays::rows(o, &snap, &self.p, &self.tz, now, width);
        overlays::render(o, &list, &self.p, width, height)
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The editor in a rounded box, inside the same one-column gutter as everything else:
/// upstream's editor draws only top and bottom rules.
struct InputBox {
    editor: Arc<Editor>,
    border: rpi_tui::theme::Color,
}

impl Component for InputBox {
    fn render(&self, width: usize) -> Vec<String> {
        let width = width.saturating_sub(2);
        let inner = width.saturating_sub(4).max(1);
        let rows = self.editor.render(inner);
        let rows = &rows[1..rows.len().saturating_sub(1).max(1)];
        let unicode = theme::glyph("─") == "─";
        let (tl, tr, bl, br, v) = if unicode {
            ("╭", "╮", "╰", "╯", "│")
        } else {
            ("+", "+", "+", "+", "|")
        };
        let h = theme::glyph("─").repeat(width.saturating_sub(2));
        let b = |s: &str| self.border.fg(s);
        let mut lines = vec![format!(" {}", b(&format!("{tl}{h}{tr}")))];
        for row in rows {
            let shown = rpi_tui::visible_width(&row.replace(rpi_tui::CURSOR_MARKER, ""));
            let pad = inner.saturating_sub(shown);
            lines.push(format!(" {} {row}{} {}", b(v), " ".repeat(pad), b(v)));
        }
        lines.push(format!(" {}", b(&format!("{bl}{h}{br}"))));
        lines
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The terminal's background from an OSC 11 query, sent with a DA1 query that every terminal
/// answers, so a terminal without OSC 11 costs one round trip instead of the timeout.
fn probe_background() -> Option<rpi_tui::RgbColor> {
    use std::io::Write;
    crossterm::terminal::enable_raw_mode().ok()?;
    let mut out = std::io::stdout();
    let sent = out
        .write_all(format!("{}\x1b[c", rpi_tui::query_background_color()).as_bytes())
        .and_then(|_| out.flush())
        .is_ok();
    let mut got = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_millis(300);
    while sent && !da1_done(&got) {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        let mut fd = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one pollfd on stdin, which outlives the call.
        if unsafe { libc::poll(&mut fd, 1, left.as_millis() as i32) } <= 0 {
            break;
        }
        let mut buf = [0u8; 256];
        // SAFETY: reads into a local buffer of the given length.
        let n = unsafe { libc::read(0, buf.as_mut_ptr().cast(), buf.len()) };
        if n <= 0 {
            break;
        }
        got.extend_from_slice(&buf[..n as usize]);
    }
    let _ = crossterm::terminal::disable_raw_mode();
    osc11(&String::from_utf8_lossy(&got))
}

/// The OSC 11 answer cut out of whatever else the terminal sent.
fn osc11(got: &str) -> Option<rpi_tui::RgbColor> {
    let start = got.find("\x1b]11;")?;
    let rest = &got[start..];
    let end = rest
        .find('\x07')
        .map(|i| i + 1)
        .or_else(|| rest.find("\x1b\\").map(|i| i + 2))?;
    rpi_tui::parse_osc11_background_color(&rest[..end])
}

/// A DA1 answer is `ESC [ ? … c`.
fn da1_done(got: &[u8]) -> bool {
    got.windows(3)
        .position(|w| w == b"\x1b[?")
        .is_some_and(|i| got[i..].contains(&b'c'))
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_background_answer_is_found_beside_da1() {
        let got = "\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[?62;22c";
        assert!(super::da1_done(got.as_bytes()));
        assert!(super::osc11(got).unwrap().is_light());
        let dark = "x\x1b]11;rgb:0f0f/1414/1919\x07";
        assert!(super::osc11(dark).unwrap().is_dark());
        assert!(super::osc11("\x1b[?62c").is_none());
        assert!(!super::da1_done(b"\x1b]11;rgb:0/0/0\x07"));
    }

    #[test]
    fn only_an_open_sign_in_is_polled_and_no_faster_than_the_interval() {
        let mut s = super::chrome::State::default();
        let mut poll = super::Poller::default();
        assert!(matches!(
            super::login_poll(&s, 1000, &mut poll),
            super::Poll::Idle
        ));
        let mut o = super::overlays::Overlay::new(super::overlays::Kind::Login);
        o.login.session = Some(super::conn::Login {
            id: "l1".into(),
            provider: "claude-code".into(),
            account: None,
            kind: "setup_token".into(),
        });
        s.overlay = Some(o);
        assert!(matches!(
            super::login_poll(&s, 1000, &mut poll),
            super::Poll::Ask(id) if id == "l1"
        ));
        // Nothing more until half a second has gone by.
        assert!(matches!(
            super::login_poll(&s, 1100, &mut poll),
            super::Poll::Idle
        ));
        assert!(matches!(
            super::login_poll(&s, 1000 + super::LOGIN_POLL_MS, &mut poll),
            super::Poll::Ask(id) if id == "l1"
        ));
        // A closed panel stops asking, whatever the clock says.
        s.overlay = None;
        assert!(matches!(
            super::login_poll(&s, 9999, &mut poll),
            super::Poll::Idle
        ));
        // Reopened on the same login, it is asked about at once: the ask that went out for the
        // panel that was closed does not hold it back for another interval.
        s.overlay = Some(super::overlays::Overlay::new(super::overlays::Kind::Login));
        if let Some(o) = s.overlay.as_mut() {
            o.login.session = Some(super::conn::Login {
                id: "l1".into(),
                ..Default::default()
            });
        }
        assert!(matches!(
            super::login_poll(&s, 9999, &mut poll),
            super::Poll::Ask(id) if id == "l1"
        ));
    }

    #[test]
    fn a_sign_in_the_daemon_stops_answering_is_given_up_on() {
        let state = |id: &str| {
            let mut s = super::chrome::State::default();
            let mut o = super::overlays::Overlay::new(super::overlays::Kind::Login);
            o.login.session = Some(super::conn::Login {
                id: id.into(),
                provider: "claude-code".into(),
                ..Default::default()
            });
            s.overlay = Some(o);
            s
        };
        let s = state("l1");
        let mut poll = super::Poller::default();
        assert!(matches!(
            super::login_poll(&s, 1000, &mut poll),
            super::Poll::Ask(_)
        ));
        // One answer, then silence: the panel keeps asking, so a lost answer is retried.
        poll.answered(&s, 1500);
        assert!(matches!(
            super::login_poll(&s, 2000, &mut poll),
            super::Poll::Ask(_)
        ));
        // ... and after half a minute of it, the sign-in is over rather than frozen: a lost
        // answer used to leave the panel polling nothing and showing a stale phase forever.
        let quiet = 1500 + super::LOGIN_SILENCE_MS;
        assert!(matches!(
            super::login_poll(&s, quiet, &mut poll),
            super::Poll::Silent(id) if id == "l1"
        ));
        // Another panel opening on another login starts its own clock: a poll that stopped
        // being answered for the last one cannot age this one.
        let s = state("l2");
        assert!(matches!(
            super::login_poll(&s, quiet + 60_000, &mut poll),
            super::Poll::Ask(id) if id == "l2"
        ));
    }

    #[test]
    fn giving_up_says_so_on_the_list_and_cancels_the_login() {
        let mut s = super::chrome::State::default();
        let mut o = super::overlays::Overlay::new(super::overlays::Kind::Login);
        o.login.session = Some(super::conn::Login {
            id: "l1".into(),
            ..Default::default()
        });
        s.overlay = Some(o);
        let (asks, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut poll = super::Poller::default();
        super::login_poll(&s, 1000, &mut poll);
        super::give_up(&mut s, &asks, "l1", &mut poll);
        let o = s.overlay.as_ref().expect("the list stays");
        assert_eq!(o.login.running(), None);
        assert_eq!(o.login.problem.as_deref(), Some(super::GIVE_UP));
        // The daemon's copy is asked to stop: nothing will read it again.
        assert!(matches!(
            rx.try_recv(),
            Ok(super::Ask::LoginCancel { login_id }) if login_id == "l1"
        ));
        // And the poll for it is gone, so the next sign-in starts from the clock.
        assert!(matches!(
            super::login_poll(&s, 9999, &mut poll),
            super::Poll::Idle
        ));
        // A give-up for a login the panel is no longer running is a no-op.
        super::give_up(&mut s, &asks, "l1", &mut poll);
        assert_eq!(
            s.overlay.as_ref().unwrap().login.problem.as_deref(),
            Some(super::GIVE_UP)
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_login_error_takes_the_panel_down_even_mid_token() {
        let mut s = super::chrome::State::default();
        assert!(!super::login_over(&s));
        let mut o = super::overlays::Overlay::new(super::overlays::Kind::Login);
        o.login.session = Some(super::conn::Login {
            id: "l1".into(),
            ..Default::default()
        });
        o.login.status = Some(super::conn::LoginStatus {
            phase: "need_input".into(),
            ..Default::default()
        });
        assert!(o.login.needs_input());
        s.overlay = Some(o);
        // The token was taken and the daemon refused it: the prompt must not be left waiting.
        assert!(super::login_over(&s));
        // Another panel is not a sign-in, so an error about one does not close it.
        s.overlay = Some(super::overlays::Overlay::new(super::overlays::Kind::Status));
        assert!(!super::login_over(&s));
    }

    /// The panel a sign-in is started from, as `/login` opens it.
    fn login_panel() -> super::overlays::Overlay {
        super::overlays::Overlay::new(super::overlays::Kind::Login)
    }

    /// Esc can close the panel between `login_start` going out and its answer coming back. The
    /// answer then has no panel to adopt it, and nothing on the tick would ever poll or drop it:
    /// the login would run in the daemon to the end, holding one of its sixteen slots.
    #[tokio::test]
    async fn a_start_whose_panel_closed_first_is_cancelled_rather_than_orphaned() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            super::conn::handshake(&mut lines, &mut w, "tok", "a/one")
                .await
                .unwrap();
            let start: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let answer = serde_json::json!({"jsonrpc": "2.0", "id": start["id"], "result": {
                "loginId": "l1", "provider": "claude-code", "account": null, "kind": "setup_token"}});
            w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            // The panel is gone by now, so the next frame is this login's cancel, not a poll.
            let next: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            (start, next)
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (asks, prx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(super::conn::run(addr, "tok".into(), tx, prx));
        assert!(matches!(rx.recv().await, Some(super::Msg::Up { .. })));

        let mut s = super::chrome::State {
            overlay: Some(login_panel()),
            ..Default::default()
        };
        super::start(&mut s, &asks, "claude-code".into());
        let Some(super::Msg::LoginStarted(login)) = rx.recv().await else {
            panic!("no start answer")
        };
        // Esc while the start was in flight: `close` drops the overlay, and the answer is
        // turned away where the message is routed.
        s.overlay = None;
        let mut poll = super::Poller::default();
        assert!(!super::take_start(&mut s, &asks, &login, &mut poll));
        // Silent to the user: the panel is not reopened and says nothing.
        assert!(s.overlay.is_none());

        let (start, next) = server.await.unwrap();
        assert_eq!(start["method"], "zephyr.login_start");
        assert_eq!(start["params"]["provider"], "claude-code");
        assert_eq!(next["method"], "zephyr.login_cancel");
        assert_eq!(next["params"]["loginId"], "l1");
    }

    /// `unknown login` (a login the daemon dropped, or never started) is not the daemon going
    /// quiet: the panel ends on it without a word. `GIVE_UP` stays for the half minute of
    /// silence, where the sign-in may still be running and there is something to say.
    #[tokio::test]
    async fn an_unknown_login_ends_the_panel_without_a_word() {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut lines = BufReader::new(r).lines();
            super::conn::handshake(&mut lines, &mut w, "tok", "a/one")
                .await
                .unwrap();
            let req: serde_json::Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            let answer = serde_json::json!({"jsonrpc": "2.0", "id": req["id"],
                "error": {"code": -32602, "message": "unknown login"}});
            w.write_all(format!("{answer}\n").as_bytes()).await.unwrap();
            lines.next_line().await.ok();
        });
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (ptx, prx) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(super::conn::run(addr, "tok".into(), tx, prx));
        assert!(matches!(rx.recv().await, Some(super::Msg::Up { .. })));

        let mut s = super::chrome::State::default();
        let mut o = login_panel();
        o.login.session = Some(super::conn::Login {
            id: "l1".into(),
            provider: "claude-code".into(),
            ..Default::default()
        });
        s.overlay = Some(o);
        ptx.send(super::Ask::LoginPoll {
            login_id: "l1".into(),
        })
        .unwrap();
        let Some(super::Msg::LoginGone(login_id)) = rx.recv().await else {
            panic!("no poll answer")
        };
        assert_eq!(login_id, "l1");
        let mut poll = super::Poller::default();
        assert!(matches!(
            super::login_poll(&s, 1000, &mut poll),
            super::Poll::Ask(id) if id == "l1"
        ));
        // The panel showing that login ends, and the poll with it: nothing will answer it.
        assert!(super::login_gone(&s, &login_id, &mut poll));
        assert!(poll.login_id.is_none());
        // Quietly: no reason on the panel, and none of the give-up text.
        let o = s.overlay.as_ref().expect("the caller closes it");
        assert_eq!(o.login.problem, None);
        // A late answer about a login the panel no longer runs says nothing about the one it
        // does: that panel stays up, and its poll stays followed.
        if let Some(o) = s.overlay.as_mut() {
            o.login.session.as_mut().unwrap().id = "l2".into();
        }
        assert!(!super::login_gone(&s, &login_id, &mut poll));
        assert_eq!(s.overlay.as_ref().unwrap().login.running(), Some("l2"));
        assert_eq!(s.overlay.as_ref().unwrap().login.problem, None);
    }

    #[test]
    fn a_completion_edits_only_up_to_the_cursor_and_leaves_it_after_the_insert() {
        let editor = rpi_tui::Editor::simple();
        super::recall(&editor, "see @ap and more\nsecond");
        assert_eq!(editor.cursor_position(), (1, 6));
        editor.set_cursor(0, "see @ap".len());
        assert_eq!(super::before_cursor(&editor), "see @ap");
        super::take(&editor, "see @ap", "see @app/");
        assert_eq!(editor.get_text(), "see @app/ and more\nsecond");
        assert_eq!(super::before_cursor(&editor), "see @app/");
        // A case-insensitive match rewrites what was typed.
        editor.set_text("/p R");
        editor.set_cursor(0, 4);
        super::take(&editor, "/p R", "/p review");
        assert_eq!(editor.get_text(), "/p review");
        assert_eq!(editor.cursor_position(), (0, 9));
    }

    #[test]
    fn copying_is_an_osc_52_with_the_text_in_base64() {
        assert_eq!(super::osc52("hi\n"), "\x1b]52;c;aGkK\x07");
        assert_eq!(super::osc52("한"), "\x1b]52;c;7ZWc\x07");
    }

    #[test]
    fn copy_takes_the_last_reply_or_a_numbered_block() {
        let mut view = super::view::View::new(super::theme::DARK, jiff::tz::TimeZone::UTC);
        assert_eq!(
            super::to_copy(&view, ""),
            Err("no reply to copy yet".into())
        );
        let reply = |text: &str| {
            serde_json::json!({"kind": "assistant", "role": "assistant",
            "content": [{"type": "text", "text": text}]})
        };
        view.replay(&[
            reply("one:\n\n```\nfirst\n```"),
            reply("two:\n\n```sh\nsecond\nline\n```\n"),
        ]);
        assert_eq!(
            super::to_copy(&view, ""),
            Ok((
                "two:\n\n```sh\nsecond\nline\n```".into(),
                "the last reply".into()
            ))
        );
        assert_eq!(
            super::to_copy(&view, " 1"),
            Ok(("first".into(), "#1".into()))
        );
        assert_eq!(
            super::to_copy(&view, "#2"),
            Ok(("second\nline".into(), "#2".into()))
        );
        for bad in ["3", "0", "x"] {
            assert_eq!(
                super::to_copy(&view, bad),
                Err(format!("no code block {bad}"))
            );
        }
    }

    #[test]
    fn only_a_run_that_ends_by_itself_is_cheered() {
        let status = |busy: bool| serde_json::json!({"type": "zephyr_status", "snapshot": {"run": {"busy": busy}}});
        let mut view = super::view::View::new(super::theme::DARK, jiff::tz::TimeZone::UTC);
        for stopped in [false, true] {
            let mut s = super::chrome::State::default();
            super::event(&mut s, &mut view, &status(true));
            s.stopping = stopped;
            super::event(&mut s, &mut view, &status(false));
            assert_eq!(s.finished.is_some(), !stopped);
            assert!(!s.stopping);
        }
    }

    #[test]
    fn ctrl_p_walks_the_current_accounts_models_both_ways() {
        let list = |name: Option<&str>, models: &[&str]| super::conn::AccountModels {
            name: name.map(Into::into),
            model: None,
            models: models.iter().map(|m| m.to_string()).collect(),
        };
        let lists = [
            list(Some("a"), &["x/1", "x/2", "x/3"]),
            list(Some("b"), &["y/1"]),
        ];
        let mut snap = super::conn::Snapshot {
            account: Some("a".into()),
            model: Some("x/1".into()),
            ..Default::default()
        };
        let next = |snap: &super::conn::Snapshot, back| super::next_model(snap, &lists, back);
        assert_eq!(next(&snap, false).as_deref(), Some("x/2"));
        assert_eq!(next(&snap, true).as_deref(), Some("x/3"), "wraps");
        snap.model = Some("z/9".into());
        assert_eq!(
            next(&snap, false).as_deref(),
            Some("x/1"),
            "an unlisted model"
        );
        snap.account = Some("gone".into());
        assert_eq!(next(&snap, false), None);
    }

    #[test]
    fn the_context_is_the_last_good_replys_whole_request() {
        let m = |role: &str, stop: &str| {
            serde_json::json!({"role": role, "stopReason": stop,
                "usage": {"input": 100, "cacheRead": 2000, "cacheWrite": 30, "output": 4, "totalTokens": 9}})
        };
        let mut view = super::view::View::new(super::theme::DARK, jiff::tz::TimeZone::UTC);
        let mut s = super::chrome::State::default();
        let end = |m| serde_json::json!({"type": "message_end", "message": m});
        super::event(&mut s, &mut view, &end(m("assistant", "stop")));
        assert_eq!(s.context, Some(2134));
        // A user turn or a failed reply leaves it as it was.
        super::event(&mut s, &mut view, &end(m("user", "")));
        super::event(&mut s, &mut view, &end(m("assistant", "aborted")));
        assert_eq!(s.context, Some(2134));
    }

    /// A terminal that keeps what is written to it, for driving a real `TuiAltScreen`.
    #[derive(Clone, Default)]
    struct Fake(std::sync::Arc<std::sync::Mutex<String>>);

    impl rpi_tui::Terminal for Fake {
        fn info(&self) -> rpi_tui::TerminalInfo {
            rpi_tui::TerminalInfo {
                columns: 40,
                rows: 6,
                ..Default::default()
            }
        }
        fn write(&self, data: &str) {
            self.0.lock().unwrap().push_str(data);
        }
        fn hide_cursor(&self) {}
        fn show_cursor(&self) {}
        fn move_cursor(&self, _row: usize, _col: usize) {}
        fn clear_screen(&self) {}
        fn set_title(&self, title: &str) {
            self.write(&format!("<title {title}>"));
        }
        fn enable_mouse(&self) {}
        fn disable_mouse(&self) {}
        fn enter_raw_mode(&self) {}
        fn exit_raw_mode(&self) {}
        fn refresh_size(&self) {}
        fn start(
            &self,
            _on_input: Box<dyn Fn(rpi_tui::InputEvent) + Send + Sync>,
            _on_resize: Box<dyn Fn() + Send + Sync>,
        ) {
        }
        fn stop(&self) {}
        fn is_tty(&self) -> bool {
            true
        }
        fn set_progress(&self, _active: bool) {}
        fn flush(&self) {}
    }

    /// A 40x6 screen that is all transcript, drawn with `rows`, and the transcript as placed.
    fn screen(rows: &[&str]) -> (rpi_tui::TuiAltScreen, super::Placed, Fake) {
        let doc = std::sync::Arc::new(rpi_tui::Container::new());
        for r in rows {
            doc.add_child(std::sync::Arc::new(rpi_tui::Text::new(*r, 0, 0)));
        }
        let scroll = std::sync::Arc::new(rpi_tui::ScrollView::primary(doc));
        let root = rpi_tui::VStack::from_children(vec![rpi_tui::StackChild::Entry(
            rpi_tui::StackEntry::new(scroll.clone()).basis(0).grow(1),
        )]);
        let fake = Fake::default();
        let tui = rpi_tui::TuiAltScreen::new(Box::new(fake.clone()), false, None);
        tui.set_layout_root(Some(std::sync::Arc::new(root)));
        tui.start_readerless();
        rpi_tui::TUI::request_render(&tui, true);
        let snap = scroll.snapshot(40).expect("drawn");
        let at = super::Placed {
            header: 0,
            height: scroll.viewport_height(),
            top: snap.scroll_top,
            lines: snap.document.lines(0, snap.document.len()),
        };
        (tui, at, fake)
    }

    #[test]
    fn clicks_on_one_cell_count_up_to_three_within_the_window() {
        let mut c = super::Clicks::default();
        assert_eq!(c.down((5, 2), 1000), 1);
        assert_eq!(c.down((5, 2), 1300), 2);
        assert_eq!(c.down((5, 2), 1600), 3);
        assert_eq!(c.down((5, 2), 1700), 1);
        // Another cell, or too slow, starts again.
        assert_eq!(c.down((6, 2), 1800), 1);
        assert_eq!(c.down((6, 2), 1800 + super::CLICK_MS + 1), 1);
    }

    #[test]
    fn words_and_rows_are_read_in_columns() {
        let row = "   한글 ~/src/main.rs:12 ok";
        // `한글` is two wide characters: columns 3 to 6.
        assert_eq!(super::word_at(row, 5), Some((3, 6)));
        assert_eq!(super::word_at(row, 10), Some((8, 23)));
        assert_eq!(super::word_at(row, 7), None);
        assert_eq!(super::word_at(row, 99), None);
        assert_eq!(super::row_span(row), Some((3, 26)));
        assert_eq!(super::row_span(" ▌ hi  "), Some((3, 4)));
        assert_eq!(super::row_span("   "), None);
    }

    #[test]
    fn double_triple_and_shift_clicks_select_through_rpi_tui() {
        let (tui, at, _) = screen(&["   check the nightly backup", "   and the log"]);
        let select = |anchor, cell, shift, n| {
            tui.clear_selection();
            let (from, to) = super::click_span(&at, anchor, cell, shift, n)?;
            super::drag(&tui, from, to);
            tui.selection_text()
        };
        assert_eq!(select(None, (24, 0), false, 2).as_deref(), Some("backup"));
        assert_eq!(
            select(None, (24, 0), false, 3).as_deref(),
            Some("check the nightly backup")
        );
        // Shift+click runs from the last plain click to this one, across rows.
        let anchor = Some((9, at.row(0).unwrap()));
        assert_eq!(
            select(anchor, (9, 1), true, 1).as_deref(),
            Some("the nightly backup\n   and the")
        );
        // Without an anchor, or a single click, nothing is selected.
        assert_eq!(select(None, (9, 1), true, 1), None);
        assert_eq!(select(None, (9, 1), false, 1), None);
        // A transcript row maps back to the screen, clamped to what is on it.
        assert_eq!((at.screen(1), at.screen(99)), (1, at.height as u16 - 1));
    }

    #[test]
    fn notifications_need_the_terminal_away_and_wait_ten_seconds() {
        let mut a = super::Attention::default();
        // No focus report yet: a terminal without them never notifies.
        assert_eq!(a.ping("reply ready", 0), None);
        a.away = true;
        assert_eq!(
            a.ping("reply ready", 1000).as_deref(),
            Some("\x07\x1b]9;zephyr: reply ready\x07")
        );
        assert_eq!(a.ping("needs your OK", 1000 + super::NOTIFY_MS - 1), None);
        assert!(a.ping("needs your OK", 1000 + super::NOTIFY_MS).is_some());
        a.away = false;
        assert_eq!(a.ping("reply ready", 99_000), None);
    }

    #[test]
    fn settles_and_proposals_are_what_notify() {
        let mut view = super::view::View::new(super::theme::DARK, jiff::tz::TimeZone::UTC);
        let mut s = super::chrome::State::default();
        let status = |busy: bool| serde_json::json!({"type": "zephyr_status", "snapshot": {"run": {"busy": busy}}});
        assert_eq!(super::event(&mut s, &mut view, &status(true)), None);
        assert_eq!(
            super::event(&mut s, &mut view, &status(false)),
            Some("reply ready")
        );
        let reply = |text: &str, command: Option<&str>| serde_json::json!({"type": "zephyr_reply", "text": text, "command": command});
        let asks = "[zephyr] the agent asks to run /job run 3 (#4); /ok 4 runs it, /no 4 drops it";
        assert_eq!(
            super::event(&mut s, &mut view, &reply(asks, None)),
            Some("needs your OK")
        );
        assert!(s.asking);
        let memory =
            "[zephyr] memory change waiting; /memory ok keeps it, /memory undo drops it:\n+x";
        assert!(super::event(&mut s, &mut view, &reply(memory, None)).is_some());
        // An answer to a command typed here is not asking for anything.
        let usage = "[zephyr] usage: /job ok <id>, /job rm <id>";
        assert_eq!(
            super::event(&mut s, &mut view, &reply(usage, Some("/job"))),
            None
        );
    }

    #[test]
    fn the_title_and_progress_bar_follow_the_run() {
        let (tui, _, fake) = screen(&["x"]);
        let mut s = super::chrome::State::default();
        let mut look = ("", false);
        let written = || std::mem::take(&mut *fake.0.lock().unwrap());
        written();
        super::attend(&tui, &s, &mut look);
        assert_eq!(written(), "<title ◇ zephyr>");
        s.snapshot.run.busy = true;
        super::attend(&tui, &s, &mut look);
        assert_eq!(written(), format!("<title ✦ zephyr>{}", super::PROGRESS_ON));
        // Nothing changed, nothing written.
        super::attend(&tui, &s, &mut look);
        assert_eq!(written(), "");
        s.asking = true;
        s.snapshot.run.busy = false;
        super::attend(&tui, &s, &mut look);
        assert_eq!(
            written(),
            format!("<title ✋ zephyr>{}", super::PROGRESS_OFF)
        );
    }

    #[test]
    fn search_keys_and_prompt_jumps() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers as M};
        let k = |code, m| super::find_key(&KeyEvent::new(code, m));
        assert_eq!(k(KeyCode::Char('a'), M::NONE), Some(super::Find::Type('a')));
        assert_eq!(
            k(KeyCode::Char('A'), M::SHIFT),
            Some(super::Find::Type('A'))
        );
        assert_eq!(k(KeyCode::Enter, M::NONE), Some(super::Find::Older));
        assert_eq!(k(KeyCode::Enter, M::SHIFT), Some(super::Find::Newer));
        assert_eq!(k(KeyCode::Char('g'), M::CONTROL), Some(super::Find::Older));
        assert_eq!(
            k(KeyCode::Char('G'), M::CONTROL | M::SHIFT),
            Some(super::Find::Newer)
        );
        assert_eq!(
            k(KeyCode::Char('g'), M::CONTROL | M::SHIFT),
            Some(super::Find::Newer)
        );
        assert_eq!(k(KeyCode::Esc, M::NONE), Some(super::Find::Close));
        // Ctrl+F closes it through its own arm; Ctrl+C and the like keep their meaning.
        assert_eq!(k(KeyCode::Char('f'), M::CONTROL), None);
        assert_eq!(k(KeyCode::PageUp, M::NONE), None);

        let prompts = [2, 10, 30];
        assert_eq!(super::jump(&prompts, 20, true), Some(10));
        assert_eq!(super::jump(&prompts, 10, true), Some(2));
        assert_eq!(super::jump(&prompts, 2, true), None);
        assert_eq!(super::jump(&prompts, 20, false), Some(30));
        assert_eq!(super::jump(&prompts, 30, false), None);
    }
}
