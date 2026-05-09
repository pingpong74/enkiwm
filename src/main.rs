// SPDX-License-Identifier: MPL-2.0

#![allow(irrefutable_let_patterns)]

mod handlers;

mod grabs;
mod input;
mod state;
mod winit;
mod layout;
mod math;
mod command;

use std::io::IsTerminal;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
pub use state::Enki;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();

    let mut event_loop: EventLoop<Enki> = EventLoop::try_new()?;

    let display: Display<Enki> = Display::new()?;

    let mut state = Enki::new(&mut event_loop, display);

    // Open a Wayland/X11 window for our nested compositor
    crate::winit::init_winit(&mut event_loop, &mut state)?;

    // Env vars to setup stable wayland functionallity
    std::env::remove_var("DISPLAY");
    // Set WAYLAND_DISPLAY to our socket name, so child processes connect to Enki rather than the host compositor
    std::env::set_var("WAYLAND_DISPLAY", &state.socket_name);
    std::env::set_var("OZONE_PLATFORM", "wayland");
    std::env::set_var("QT_QPA_PLATFORM", "wayland");


    // Spawn a test client, that will run under Enki
    spawn_client();

    event_loop.run(None, &mut state, move |_| {
        // Enki is running
    })?;

    Ok(())
}

fn init_logging() {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        let is_tty = std::io::stdout().is_terminal();
        tracing_subscriber::fmt().with_ansi(is_tty).init();
    }
}

fn spawn_client() {
    let mut args = std::env::args().skip(1);
    let flag = args.next();
    let arg = args.next();

    match (flag.as_deref(), arg) {
        (Some("-c") | Some("--command"), Some(command)) => {
            std::process::Command::new(command).spawn().ok();
        }
        _ => {
            std::process::Command::new("weston-terminal").spawn().ok();
        }
    }
}
