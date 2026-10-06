//! Elevated device capture. The GUI, configuration and gamepads stay unprivileged.
use super::protocol::{ButtonState, HEADER, InputMessage};
use super::*;
use std::{io::Write, os::fd::AsRawFd};
use zbus::{blocking::Proxy, zvariant::OwnedObjectPath};

fn packet(output: &mut impl Write, message: InputMessage) -> io::Result<()> {
    output.write_all(&message.encode())?;
    output.flush()
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let uid: u32 = std::env::var("PKEXEC_UID")?.parse()?;
    // SAFETY: geteuid has no pointer arguments or side effects.
    if uid == 0 || unsafe { libc::geteuid() } != 0 {
        return Err("use pkexec from a local user session".into());
    }
    if std::env::args_os().skip(1).collect::<Vec<_>>() != ["--input-helper"] {
        return Err("input helper accepts only --input-helper".into());
    }
    let bus = zbus::blocking::connection::Builder::system()?
        .method_timeout(std::time::Duration::from_secs(1))
        .build()?;
    let manager = Proxy::new(
        &bus,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )?;
    let user_path: OwnedObjectPath = manager.call("GetUser", &(uid,))?;
    let user = Proxy::new(
        &bus,
        "org.freedesktop.login1",
        user_path,
        "org.freedesktop.login1.User",
    )?;
    let (_, session_path): (String, OwnedObjectPath) = user.get_property("Display")?;
    let session = Proxy::new(
        &bus,
        "org.freedesktop.login1",
        session_path,
        "org.freedesktop.login1.Session",
    )?;
    let (session_uid, _): (u32, OwnedObjectPath) = session.get_property("User")?;
    let (seat, _): (String, OwnedObjectPath) = session.get_property("Seat")?;
    if session_uid != uid || seat.is_empty() || session.get_property::<bool>("Remote")? {
        return Err("a local graphical session is required".into());
    }

    let mut output = io::stdout();
    // A stalled or exited GUI must not keep this elevated owner blocked.
    // SAFETY: stdout is a valid fd owned by this process; only its flags change.
    unsafe {
        let flags = libc::fcntl(output.as_raw_fd(), libc::F_GETFL);
        if flags == -1
            || libc::fcntl(output.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) == -1
        {
            return Err(io::Error::last_os_error().into());
        }
    }
    output.write_all(HEADER)?;
    output.flush()?;
    let mut devices: Vec<InputDevice> = Vec::new();
    let mut unsupported = HashSet::new();
    let mut enabled = false;
    let mut suspend_detector = SuspendDetector::new();
    let mut next_session = Instant::now();
    let mut next_scan = Instant::now();
    let mut next_reconcile = Instant::now();
    loop {
        let mut control = libc::pollfd {
            fd: 0,
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: control is initialized and borrowed only for the poll call.
        if unsafe { libc::poll(&mut control, 1, 5) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error.into());
        }
        // EOF, a parent crash, or unexpected control input ends this helper.
        if control.revents != 0 {
            break;
        }
        let now = Instant::now();
        if now >= next_session {
            let active = session.get_property::<bool>("Active")?
                && !session.get_property::<bool>("LockedHint")?;
            if active != enabled || suspend_detector.resumed() {
                devices.clear();
                unsupported.clear();
                enabled = active;
                packet(&mut output, InputMessage::Reset { enabled })?;
                next_scan = now;
                next_reconcile = now;
            }
            packet(&mut output, InputMessage::Heartbeat { enabled })?;
            next_session = now + Duration::from_millis(50);
        }
        if !enabled {
            continue;
        }
        if now >= next_scan {
            let existing = devices.iter().map(|device| device.path.clone()).collect();
            let discovery = discover_devices(&existing, &unsupported, &seat);
            let unavailable = devices.is_empty() && discovery.devices.is_empty();
            let permission_denied =
                unavailable && discovery_error(&discovery) == PlatformInputError::PermissionDenied;
            packet(
                &mut output,
                InputMessage::Backend {
                    available: !unavailable,
                    permission_denied,
                },
            )?;
            devices.extend(discovery.devices);
            if let Some(paths) = discovery.observed_paths {
                unsupported.retain(|path| paths.contains(path));
            }
            unsupported.extend(discovery.unsupported_paths);
            next_scan = now + DEVICE_SCAN_INTERVAL;
        }
        let mut removed = Vec::new();
        for (index, input) in devices.iter_mut().enumerate() {
            let events = match input.device.fetch_events() {
                Ok(events) => events.collect::<Vec<_>>(),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(_) => {
                    removed.push(index);
                    continue;
                }
            };
            for event in events {
                match event.destructure() {
                    EventSummary::Key(_, key, value @ (0 | 1)) if linux_control(key).is_some() => {
                        packet(
                            &mut output,
                            InputMessage::Key {
                                code: u32::from(key.0),
                                state: if value == 1 {
                                    ButtonState::Pressed
                                } else {
                                    ButtonState::Released
                                },
                            },
                        )?;
                    }
                    EventSummary::RelativeAxis(_, axis, value) => {
                        input.relative_motion.observe(axis, value)
                    }
                    EventSummary::Synchronization(_, SynchronizationCode::SYN_REPORT, _) => {
                        if let Some((x, y)) = input.relative_motion.finish_report() {
                            packet(
                                &mut output,
                                InputMessage::Motion {
                                    dx: x as f64,
                                    dy: y as f64,
                                },
                            )?;
                        }
                    }
                    EventSummary::Synchronization(_, SynchronizationCode::SYN_DROPPED, _) => {
                        input.relative_motion.discard_report()
                    }
                    _ => {}
                }
            }
        }
        if !removed.is_empty() {
            for index in removed.into_iter().rev() {
                devices.swap_remove(index);
            }
            packet(&mut output, InputMessage::Reset { enabled })?;
            next_reconcile = now;
        }
        if now >= next_reconcile {
            // A failed query removes that device on the next fetch; only send a complete
            // snapshot when every open device has answered, avoiding false releases.
            let pressed = devices
                .iter()
                .map(|input| input.device.get_key_state())
                .collect::<Result<Vec<_>, _>>();
            if let Ok(states) = pressed {
                packet(&mut output, InputMessage::ReconcileStart)?;
                let mut keys = BTreeSet::new();
                for state in states {
                    keys.extend(state.iter().map(|key| key.0));
                }
                for code in keys {
                    packet(
                        &mut output,
                        InputMessage::ReconcileKey {
                            code: u32::from(code),
                        },
                    )?;
                }
                packet(&mut output, InputMessage::ReconcileEnd)?;
            }
            next_reconcile = now + RECONCILIATION_INTERVAL;
        }
    }
    Ok(())
}
