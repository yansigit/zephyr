//! `zephyr tui`: Zephyr's own terminal client on the attach port, built from rpi-tui
//! components. The loop follows upstream's `rpi-cli/src/tui_shell.rs::run_driver_tui`
//! (pi-rust v0.12.0, MIT): an input reader thread, a submit channel and the render scheduler.

mod chrome;
mod conn;
mod overlays;
mod theme;
mod view;

use crate::config::Config;
use anyhow::Context;
use chrome::{Link, Region, Retry, State};
use conn::{Ask, Msg};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
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
    let scroll = Arc::new(ScrollView::primary(document));
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
    tui.set_title("zephyr");
    tui.start_render_scheduler();
    tui.start_readerless();

    let (submit_tx, mut submits) = mpsc::unbounded_channel::<String>();
    editor.on_submit(Arc::new(move |text: &str| {
        let _ = submit_tx.send(text.to_string());
    }));
    let (input_tx, mut inputs) = mpsc::unbounded_channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    let reader = {
        let stop = stop.clone();
        tokio::task::spawn_blocking(move || {
            while !stop.load(Ordering::Relaxed) {
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
    let mut was_up = false;
    // The composer text whose autocomplete Esc dismissed.
    let mut dismissed: Option<String> = None;
    let outcome: anyhow::Result<()> = loop {
        tokio::select! {
            msg = msgs.recv() => {
                let Some(msg) = msg else { break Ok(()) };
                let mut s = state.lock().unwrap();
                match msg {
                    Msg::Up { snapshot, messages } => {
                        s.link = Link::Up;
                        s.snapshot = *snapshot;
                        s.retry = None;
                        s.stopping = false;
                        view.replay(&messages);
                        if was_up {
                            view.notice("reconnected");
                        }
                        was_up = true;
                        for text in s.unsent.drain(..) {
                            let _ = prompt_tx.send(Ask::Prompt(text));
                        }
                    }
                    Msg::Event(ev) => event(&mut s, &mut view, &ev),
                    Msg::Models(list) => {
                        if let Some(o) = s.overlay.as_mut().filter(|o| o.kind == Kind::Model) {
                            o.models = Some(list);
                            o.placed = false;
                        }
                    }
                    Msg::Down(why) if was_up => s.link = Link::Down { why },
                    Msg::Down(why) => s.link = Link::Connecting(Some(why)),
                    Msg::Fatal(e) => break Err(anyhow::anyhow!(e)),
                }
                drop(s);
                tui.request_render(false);
            }
            input = inputs.recv() => {
                let Some(input) = input else { break Ok(()) };
                match input {
                    Event::Resize(..) => tui.note_resize(),
                    Event::Mouse(m) if !tui.handle_scroll_to_end_indicator_mouse_event(&m) => {
                        match m.kind {
                            MouseEventKind::ScrollUp => tui.scroll_by(-3),
                            MouseEventKind::ScrollDown => tui.scroll_by(3),
                            _ => {}
                        }
                    }
                    Event::Paste(text) if state.lock().unwrap().overlay.is_none() => {
                        editor.handle_paste(&text)
                    }
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        let now = crate::unix_millis() as i64;
                        let (busy, panel_open, completing) = {
                            let s = state.lock().unwrap();
                            (s.snapshot.run.busy, s.overlay.is_some(), s.complete.is_some())
                        };
                        let text = editor.get_text();
                        let empty = text.is_empty();
                        let plain = key.modifiers.is_empty();
                        let mut s = state.lock().unwrap();
                        match key {
                            k if ctrl(&k, 'd') && empty => break Ok(()),
                            k if panel_open => {
                                let snap = s.snapshot.clone();
                                let o = s.overlay.as_mut().expect("open");
                                let kind = o.kind;
                                let action = if ctrl(&k, 'c') {
                                    Action::Close
                                } else {
                                    let rows = overlays::rows(o, &snap, &palette, &tz, now);
                                    overlays::key(o, k, &rows)
                                };
                                match action {
                                    Action::None => {}
                                    Action::Close => close(&tui, &editor, &mut s),
                                    Action::Pick(cmd) => {
                                        close(&tui, &editor, &mut s);
                                        match kind {
                                            Kind::Model => send(&mut s, &prompt_tx, cmd),
                                            _ => editor.set_text(&cmd),
                                        }
                                    }
                                }
                            }
                            KeyEvent {
                                code: KeyCode::Up | KeyCode::Down | KeyCode::Tab | KeyCode::Enter | KeyCode::Esc,
                                ..
                            } if completing && plain => {
                                let c = s.complete.as_mut().expect("completing");
                                let n = c.items.len();
                                let insert = overlays::insert(overlays::COMMANDS[c.items[c.selected]].1);
                                // Still typing the command name, not its arguments.
                                let partial = insert.starts_with(&text) && insert != text;
                                match key.code {
                                    KeyCode::Up => c.selected = (c.selected + n - 1) % n,
                                    KeyCode::Down => c.selected = (c.selected + 1) % n,
                                    KeyCode::Esc => {
                                        s.complete = None;
                                        dismissed = Some(text.clone());
                                    }
                                    KeyCode::Tab if partial => editor.set_text(insert),
                                    KeyCode::Tab => {}
                                    // Enter takes the choice, and runs it when it needs no argument.
                                    _ if partial && insert.ends_with(' ') => editor.set_text(insert),
                                    _ => {
                                        if partial {
                                            editor.set_text(insert);
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
                            KeyEvent { code: KeyCode::Esc, .. } if busy => {
                                if !s.stopping && s.link == Link::Up {
                                    s.stopping = true;
                                    let _ = prompt_tx.send(Ask::Prompt("/stop".into()));
                                }
                            }
                            KeyEvent { code: KeyCode::PageUp, modifiers: KeyModifiers::NONE, .. } => {
                                tui.scroll_by(-page())
                            }
                            KeyEvent { code: KeyCode::PageDown, modifiers: KeyModifiers::NONE, .. } => {
                                tui.scroll_by(page())
                            }
                            k => {
                                drop(s);
                                editor.handle_key(k);
                            }
                        }
                    }
                    _ => {}
                }
                complete(&mut state.lock().unwrap(), &editor.get_text(), &mut dismissed);
                tui.request_render(false);
            }
            text = submits.recv() => {
                let Some(text) = text else { break Ok(()) };
                let mut s = state.lock().unwrap();
                // These open a panel here instead of a reply card.
                match text.trim() {
                    "" => {}
                    "/help" => open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Help),
                    "/status" => open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Status),
                    "/model" => open(&tui, &panel, &editor, &mut s, &prompt_tx, Kind::Model),
                    text => send(&mut s, &prompt_tx, text.to_string()),
                }
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
                if hint_over || s.animated() || view.running() {
                    tui.request_render(false);
                }
            }
        }
    };
    stop.store(true, Ordering::Relaxed);
    let _ = reader.await;
    tui.stop(Default::default());
    outcome?;
    println!("detached — zephyr keeps running (zephyr attach to return)");
    Ok(())
}

/// Routes one live event: zephyr's own to the chrome, the agent's to the transcript.
fn event(s: &mut State, view: &mut view::View, ev: &Value) {
    match ev["type"].as_str().unwrap_or_default() {
        "zephyr_status" => {
            s.snapshot = serde_json::from_value(ev["snapshot"].clone()).unwrap_or_default();
            if !s.snapshot.run.busy {
                s.stopping = false;
            }
        }
        "zephyr_reply" => view.reply(
            ev["command"].as_str(),
            ev["text"].as_str().unwrap_or_default(),
        ),
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
            view.apply(ev);
        }
    }
}

/// Sends a prompt now, or keeps it for the next connection.
fn send(s: &mut State, asks: &mpsc::UnboundedSender<Ask>, text: String) {
    if s.link == Link::Up {
        let _ = asks.send(Ask::Prompt(text));
    } else {
        s.unsent.push(text);
    }
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
    if kind == Kind::Model {
        let _ = asks.send(Ask::Models);
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

/// Keeps slash autocomplete in step with the composer's text.
fn complete(s: &mut State, text: &str, dismissed: &mut Option<String>) {
    if s.overlay.is_some() || dismissed.as_deref() == Some(text) {
        s.complete = None;
        return;
    }
    *dismissed = None;
    let items = overlays::matches(text);
    let selected = (s.complete.as_ref())
        .filter(|c| c.items == items)
        .map_or(0, |c| c.selected);
    s.complete = (!items.is_empty()).then_some(overlays::Complete { items, selected });
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
        let list = overlays::rows(o, &snap, &self.p, &self.tz, now);
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
}
