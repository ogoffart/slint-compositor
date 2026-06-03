//! Volume via PulseAudio / PipeWire (libpulse).
//!
//! libpulse works against both PulseAudio and PipeWire's `pipewire-pulse`
//! server. A worker thread drives a threaded mainloop; the UI sends
//! [`VolCommand`]s (set level / toggle mute) and receives [`VolEvent`]s with the
//! default sink's level and mute state. Operations are issued with the mainloop
//! unlocked and awaited by polling their state (kept alive until the callback
//! fires), which avoids the lock/wait/signal dance.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::mainloop::threaded::Mainloop;
use libpulse_binding::volume::{ChannelVolumes, Volume};

/// Default-sink state pushed to the UI thread.
#[derive(Debug, Clone, Copy)]
pub struct VolEvent {
    /// 0..100
    pub volume: f32,
    pub muted: bool,
}

/// Requests from the UI thread.
#[derive(Debug, Clone)]
pub enum VolCommand {
    /// Set the level, 0..100.
    Set(f32),
    ToggleMute,
}

/// Start the volume worker. Returns the event receiver (drained on the UI
/// thread) and command sender, or `None` if the thread can't spawn.
pub fn spawn() -> Option<(Receiver<VolEvent>, Sender<VolCommand>)> {
    let (ev_tx, ev_rx) = std::sync::mpsc::channel::<VolEvent>();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<VolCommand>();
    std::thread::Builder::new()
        .name("s-compositor-volume".into())
        .spawn(move || {
            if let Err(err) = serve(ev_tx, cmd_rx) {
                log::warn!("volume: worker exited: {err}");
            }
        })
        .ok()?;
    Some((ev_rx, cmd_tx))
}

fn serve(ev_tx: Sender<VolEvent>, cmd_rx: Receiver<VolCommand>) -> Result<(), String> {
    let mut mainloop = Mainloop::new().ok_or("no mainloop")?;
    let mut context = Context::new(&mainloop, "s-compositor").ok_or("no context")?;
    context
        .connect(None, FlagSet::NOFLAGS, None)
        .map_err(|e| format!("connect: {e}"))?;
    mainloop.start().map_err(|e| format!("start: {e}"))?;

    // Wait for the context to become ready.
    if !wait_ready(&context) {
        return Err("context never became ready".into());
    }

    // Latest channel layout of the default sink, reused when setting volume.
    let channels: Arc<Mutex<Option<ChannelVolumes>>> = Arc::new(Mutex::new(None));

    // Initial read, then refresh on a timer or whenever a command arrives.
    refresh(&mut mainloop, &context, &ev_tx, &channels);
    loop {
        match cmd_rx.recv_timeout(Duration::from_millis(800)) {
            Ok(VolCommand::Set(pct)) => {
                if let Some(name) = default_sink_name(&mut mainloop, &context) {
                    if let Some(mut cv) = *channels.lock().unwrap() {
                        let target = Volume(
                            (pct.clamp(0.0, 100.0) / 100.0 * Volume::NORMAL.0 as f32) as u32,
                        );
                        let chans = cv.len().max(1);
                        cv.set(chans, target);
                        mainloop.lock();
                        let _ = context
                            .introspect()
                            .set_sink_volume_by_name(&name, &cv, None);
                        mainloop.unlock();
                    }
                }
                refresh(&mut mainloop, &context, &ev_tx, &channels);
            }
            Ok(VolCommand::ToggleMute) => {
                if let Some(name) = default_sink_name(&mut mainloop, &context) {
                    let muted = last_muted(&mut mainloop, &context, &name);
                    mainloop.lock();
                    let _ = context
                        .introspect()
                        .set_sink_mute_by_name(&name, !muted, None);
                    mainloop.unlock();
                }
                refresh(&mut mainloop, &context, &ev_tx, &channels);
            }
            Err(RecvTimeoutError::Timeout) => {
                refresh(&mut mainloop, &context, &ev_tx, &channels);
            }
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    Ok(())
}

/// Block (briefly) until the PulseAudio context reports Ready (or fails).
fn wait_ready(context: &Context) -> bool {
    for _ in 0..200 {
        match context.get_state() {
            State::Ready => return true,
            State::Failed | State::Terminated => return false,
            _ => std::thread::sleep(Duration::from_millis(10)),
        }
    }
    false
}

/// Run `op` to completion by polling its state (kept alive until done).
fn await_op<G: ?Sized>(op: libpulse_binding::operation::Operation<G>) {
    use libpulse_binding::operation::State;
    for _ in 0..200 {
        match op.get_state() {
            State::Running => std::thread::sleep(Duration::from_millis(2)),
            _ => break,
        }
    }
}

fn default_sink_name(mainloop: &mut Mainloop, context: &Context) -> Option<String> {
    let result = Arc::new(Mutex::new(None::<String>));
    mainloop.lock();
    let op = {
        let result = result.clone();
        context.introspect().get_server_info(move |info| {
            *result.lock().unwrap() = info.default_sink_name.as_ref().map(|s| s.to_string());
        })
    };
    mainloop.unlock();
    await_op(op);
    let name = result.lock().unwrap().clone();
    name
}

fn last_muted(mainloop: &mut Mainloop, context: &Context, name: &str) -> bool {
    let muted = Arc::new(Mutex::new(false));
    mainloop.lock();
    let op = {
        let muted = muted.clone();
        context
            .introspect()
            .get_sink_info_by_name(name, move |res| {
                if let ListResult::Item(info) = res {
                    *muted.lock().unwrap() = info.mute;
                }
            })
    };
    mainloop.unlock();
    await_op(op);
    let m = *muted.lock().unwrap();
    m
}

/// Read the default sink's volume/mute and push a [`VolEvent`].
fn refresh(
    mainloop: &mut Mainloop,
    context: &Context,
    ev_tx: &Sender<VolEvent>,
    channels: &Arc<Mutex<Option<ChannelVolumes>>>,
) {
    let Some(name) = default_sink_name(mainloop, context) else {
        return;
    };
    let state = Arc::new(Mutex::new(None::<(f32, bool, ChannelVolumes)>));
    mainloop.lock();
    let op = {
        let state = state.clone();
        context
            .introspect()
            .get_sink_info_by_name(&name, move |res| {
                if let ListResult::Item(info) = res {
                    let pct = info.volume.avg().0 as f32 / Volume::NORMAL.0 as f32 * 100.0;
                    *state.lock().unwrap() = Some((pct, info.mute, info.volume));
                }
            })
    };
    mainloop.unlock();
    await_op(op);

    let snapshot = *state.lock().unwrap();
    if let Some((volume, muted, cv)) = snapshot {
        *channels.lock().unwrap() = Some(cv);
        let _ = ev_tx.send(VolEvent { volume, muted });
    }
}
