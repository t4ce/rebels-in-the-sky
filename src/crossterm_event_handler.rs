use crate::app::AppEvent;
use crate::tui::TerminalEvent;
use ratatui::crossterm::event::{self, Event as CrosstermEvent, KeyEventKind, MouseEventKind};
use tokio::{select, sync::mpsc, task::JoinHandle, time};
use tokio_util::sync::CancellationToken;

fn is_scroll(kind: MouseEventKind) -> bool {
    matches!(
        kind,
        MouseEventKind::ScrollDown
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight
    )
}

pub fn start_event_handler(
    event_sender: mpsc::Sender<AppEvent>,
    cancellation_token: CancellationToken,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        #[cfg(any(target_os = "trueos", target_os = "zkvm"))]
        let _ = trueos::logl::log_record(trueos::logl::level::IMPORTANT, "apps", format_args!("rebsky input drain started"));
        let poll_interval = time::Duration::from_millis(10); // 100Hz polling
        let poll_timeout = std::time::Duration::from_millis(1);
        let mut reported_poll = false;
        let mut reported_event = false;
        loop {
            select! {
                _ = cancellation_token.cancelled() => {
                    log::info!("Terminal event handler shutting down.");
                    break;
                }

                _ = time::sleep(poll_interval) => {
                    // Drain all pending events to prevent scroll events from backing up the queue.
                    let mut events = vec![];
                    let mut had_error = false;
                    loop {
                        match event::poll(poll_timeout) {
                            Ok(true) => {},
                            Ok(false) => {
                                if !reported_poll {
                                    #[cfg(any(target_os = "trueos", target_os = "zkvm"))]
                                    let _ = trueos::logl::log_record(trueos::logl::level::IMPORTANT, "apps", format_args!("rebsky input poll initialized"));
                                    reported_poll = true;
                                }
                                break;
                            },
                            Err(e) => {
                                log::error!("Failed to poll crossterm input: {e}");
                                trueos_input_error(&e);
                                had_error = true;
                                break;
                            }
                        }
                        match event::read() {
                            Ok(ev) => {
                                if !reported_event {
                                    #[cfg(any(target_os = "trueos", target_os = "zkvm"))]
                                    let _ = trueos::logl::log_record(trueos::logl::level::IMPORTANT, "apps", format_args!("rebsky input first event: {ev:?}"));
                                    reported_event = true;
                                }
                                events.push(ev);
                            },
                            Err(e) => {
                                log::error!("Failed to read crossterm event: {e}");
                                had_error = true;
                                break;
                            }
                        }
                    }

                    // Collapse consecutive scroll events: keep only the last one per direction.
                    let mut last_scroll = None;
                    for ev in events {
                        match ev {
                            CrosstermEvent::Mouse(mouse) if is_scroll(mouse.kind) => {
                                // Replace any pending scroll with the latest one.
                                last_scroll = Some(mouse);
                            }
                            _ => {
                                // Flush pending scroll before processing a non-scroll event.
                                if let Some(scroll) = last_scroll.take() {
                                    let _ = event_sender
                                        .send(AppEvent::TerminalEvent(TerminalEvent::Mouse(scroll)))
                                        .await;
                                }

                                match ev {
                                    CrosstermEvent::Key(key) if key.kind == KeyEventKind::Press => {
                                        let _ = event_sender
                                            .send(AppEvent::TerminalEvent(TerminalEvent::Key(key)))
                                            .await;
                                    }
                                    CrosstermEvent::Mouse(mouse) => {
                                        let _ = event_sender
                                            .send(AppEvent::TerminalEvent(TerminalEvent::Mouse(mouse)))
                                            .await;
                                    }
                                    CrosstermEvent::Resize(w, h) => {
                                        let _ = event_sender
                                            .send(AppEvent::TerminalEvent(TerminalEvent::Resize(w, h)))
                                            .await;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    // Flush any remaining scroll event.
                    if let Some(scroll) = last_scroll.take() {
                        let _ = event_sender
                            .send(AppEvent::TerminalEvent(TerminalEvent::Mouse(scroll)))
                            .await;
                    }

                    if had_error {
                        break;
                    }
                }
            }
        }
    })
}

// The file logger may still be buffered while the UI is running. Mirror input
// failures to the kernel log so a stalled terminal can be diagnosed remotely.
fn trueos_input_error(error: &std::io::Error) {
    #[cfg(any(target_os = "trueos", target_os = "zkvm"))]
    let _ = trueos::logl::log_record(trueos::logl::level::IMPORTANT, "apps", format_args!("rebsky input: {error}"));
    #[cfg(not(any(target_os = "trueos", target_os = "zkvm")))]
    let _ = error;
}
