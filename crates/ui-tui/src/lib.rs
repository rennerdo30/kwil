//! Terminal frontend.
//!
//! Same model as the GUI (`kwil_view_model::View`). Event-driven: one
//! thread blocks on terminal input and forwards events into a channel that
//! also carries worker results, so the UI thread sleeps until something
//! happens. ratatui writes only changed cells, so the TUI stays fast over
//! SSH.

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use kwil_view_model::{Notify, View};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use std::sync::mpsc;

enum Msg {
    Input(Event),
    IndexDone,
    Notice(String),
    InputClosed,
}

pub fn run(view: View, on_first_frame: Box<dyn FnOnce(Notify)>) -> std::io::Result<()> {
    run_traced(view, on_first_frame, None)
}

pub fn run_traced(
    mut view: View,
    on_first_frame: Box<dyn FnOnce(Notify)>,
    trace: Option<std::sync::Arc<kwil_bench::startup::Trace>>,
) -> std::io::Result<()> {
    let (tx, rx) = mpsc::channel::<Msg>();
    if let Some(file) = view.file() {
        let t = tx.clone();
        let trace = trace.clone();
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8));
        file.index_in_background(threads, move |result| {
            let event = match result {
                Ok(()) => {
                    if let Some(trace) = trace {
                        trace.index_done();
                    }
                    Msg::IndexDone
                }
                Err(e) => Msg::Notice(e.to_string()),
            };
            let _ = t.send(event);
        });
    }
    let mut terminal = ratatui::init();
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableMouseCapture)?;
    {
        let t = tx.clone();
        std::thread::spawn(move || {
            loop {
                match event::read() {
                    Ok(ev) => {
                        if t.send(Msg::Input(ev)).is_err() {
                            break;
                        }
                    }
                    Err(_) => {
                        let _ = t.send(Msg::InputClosed);
                        break;
                    }
                }
            }
        });
    }

    let result = (|| -> std::io::Result<()> {
        let size = terminal.size()?;
        view.set_rows(size.height.saturating_sub(1) as usize);
        terminal.draw(|f| draw(f, &view))?;
        if let Some(trace) = &trace {
            trace.first_frame();
            if view.file().is_none() || view.file().is_some_and(|f| f.index().is_some()) {
                trace.index_done();
                trace.finish();
                return Ok(());
            }
        }
        let t = tx.clone();
        on_first_frame(Box::new(move |msg| {
            let _ = t.send(Msg::Notice(msg));
        }));
        drop(tx);
        while let Ok(msg) = rx.recv() {
            let changed = match msg {
                Msg::InputClosed => break,
                Msg::IndexDone => {
                    if let Some(trace) = &trace {
                        trace.finish();
                        break;
                    }
                    view.index_ready();
                    true
                }
                Msg::Notice(n) => {
                    if trace.is_some() {
                        return Err(std::io::Error::other(n));
                    }
                    view.set_notice(n);
                    true
                }
                Msg::Input(Event::Resize(_, h)) => {
                    view.set_rows(h.saturating_sub(1) as usize);
                    true
                }
                Msg::Input(Event::Key(k)) if k.kind != KeyEventKind::Release => match key(&mut view, k) {
                    KeyResult::Quit => break,
                    KeyResult::Changed(c) => c,
                },
                Msg::Input(Event::Mouse(m)) => match m.kind {
                    MouseEventKind::ScrollDown => view.scroll(3) != Default::default(),
                    MouseEventKind::ScrollUp => view.scroll(-3) != Default::default(),
                    _ => false,
                },
                Msg::Input(_) => false,
            };
            if changed {
                // ratatui diffs against the previous frame and writes only
                // the changed cells.
                terminal.draw(|f| draw(f, &view))?;
            }
        }
        Ok(())
    })();

    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableMouseCapture);
    ratatui::restore();
    result
}

enum KeyResult {
    Quit,
    Changed(bool),
}

fn key(view: &mut View, k: KeyEvent) -> KeyResult {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let page = view.rows().max(1) as i64;
    let d = match k.code {
        KeyCode::Char('q') if ctrl => return KeyResult::Quit,
        KeyCode::Esc => return KeyResult::Quit,
        KeyCode::Down => view.scroll(1),
        KeyCode::Up => view.scroll(-1),
        KeyCode::PageDown | KeyCode::Char(' ') => view.scroll(page),
        KeyCode::PageUp => view.scroll(-page),
        KeyCode::End if ctrl => view.scroll_to_end(),
        KeyCode::Home if ctrl => view.scroll_to_start(),
        _ => return KeyResult::Changed(false),
    };
    KeyResult::Changed(d != Default::default())
}

fn draw(f: &mut ratatui::Frame, view: &View) {
    let area = f.area();
    let gutter_style = Style::default().fg(Color::Rgb(0x5b, 0x6b, 0x85));
    for row in 0..view.rows().min(area.height.saturating_sub(1) as usize) {
        let y = area.y + row as u16;
        let Some(text) = view.line(row) else { continue };
        let line = Line::from(vec![
            Span::styled(format!("{:>7} ", view.line_number(row)), gutter_style),
            Span::raw(String::from_utf8_lossy(text).replace('\t', "    ")),
        ]);
        f.render_widget(Paragraph::new(line), Rect { x: area.x, y, width: area.width, height: 1 });
    }
    let status = Paragraph::new(view.status_text())
        .style(Style::default().fg(Color::Rgb(0x8f, 0xe3, 0xff)).bg(Color::Rgb(0x08, 0x0d, 0x18)));
    f.render_widget(
        status,
        Rect { x: area.x, y: area.y + area.height.saturating_sub(1), width: area.width, height: 1 },
    );
}
