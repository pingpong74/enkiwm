// SPDX-License-Identifier: MPL-2.0

use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event, InputBackend, InputEvent,
        KeyState, KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent,
    },
    input::{
        keyboard::{FilterResult, Keysym},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::SERIAL_COUNTER,
};

#[allow(unused_imports)]
use tracing::info;

use crate::{math::IVec2, state::Enki};

impl Enki {
    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial = SERIAL_COUNTER.next_serial();
                let time = Event::time_msec(&event);

                self.seat.get_keyboard().unwrap().input::<(), _>(
                    self,
                    event.key_code(),
                    event.state(),
                    serial,
                    time,
                    |data, modifiers, handle| {
                        if event.state() == KeyState::Pressed {
                            let sym = handle.modified_sym();
                            if sym == Keysym::F12 {
                                data.modal_mode = !data.modal_mode;
                                data.update_viewport(true);
                                return FilterResult::Intercept(());
                            }
                            if data.modal_mode {
                                let dir_flipped = match sym {
                                    Keysym::l | Keysym::L => Some(IVec2::X),
                                    Keysym::h | Keysym::H => Some(IVec2::NEG_X),
                                    Keysym::j | Keysym::J => Some(IVec2::NEG_Y),
                                    Keysym::k | Keysym::K => Some(IVec2::Y),
                                    _ => None,
                                };
                                if let Some(dir_flipped) = dir_flipped {
                                    let dir = dir_flipped * IVec2::FLIP_Y;

                                    let grow_dir = |span: &mut i32, origin: &mut i32, d: i32| {
                                        *span += d.abs();
                                        *origin -= (d < 0) as i32;
                                    };

                                    let shrink_dir = |span: &mut i32, origin: &mut i32, d: i32| {
                                        let old = *span;
                                        *span = (*span - d.abs()).max(1);

                                        if *span < old {
                                            *origin += (d < 0) as i32;
                                        }
                                    };

                                    let camera = &mut data.camera;
                                    if modifiers.shift {
                                        grow_dir(&mut camera.span.x, &mut camera.origin.x, dir.x);
                                        grow_dir(&mut camera.span.y, &mut camera.origin.y, dir.y);
                                    } else if modifiers.alt {
                                        shrink_dir(&mut camera.span.x, &mut camera.origin.x, dir.x);
                                        shrink_dir(&mut camera.span.y, &mut camera.origin.y, dir.y);
                                    } else {
                                        camera.origin += dir;
                                    }

                                    data.update_viewport(false);

                                    return FilterResult::Intercept(());
                                }

                                let program = match sym {
                                    Keysym::Return => Some("alacritty"),
                                    Keysym::w => Some("weston-terminal"),
                                    _ => None,
                                };

                                if let Some(program) = program {
                                    std::process::Command::new(program).spawn().ok();
                                }

                                return FilterResult::Intercept(());
                            }
                        }
                        FilterResult::Forward
                    },
                );
            }
            InputEvent::PointerMotion { .. } => {}
            InputEvent::PointerMotionAbsolute { event, .. } => {
                let output = self.space.outputs().next().unwrap();

                let output_geo = self.space.output_geometry(output).unwrap();

                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();

                let serial = SERIAL_COUNTER.next_serial();

                let pointer = self.seat.get_pointer().unwrap();

                let under = self.surface_under(pos);

                pointer.motion(
                    self,
                    under,
                    &MotionEvent {
                        location: pos,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerButton { event, .. } => {
                let pointer = self.seat.get_pointer().unwrap();
                let keyboard = self.seat.get_keyboard().unwrap();

                let serial = SERIAL_COUNTER.next_serial();

                let button = event.button_code();

                let button_state = event.state();

                if ButtonState::Pressed == button_state && !pointer.is_grabbed() {
                    if let Some((window, _loc)) = self
                        .space
                        .element_under(pointer.current_location())
                        .map(|(w, l)| (w.clone(), l))
                    {
                        self.space.raise_element(&window, true);
                        keyboard.set_focus(
                            self,
                            Some(window.toplevel().unwrap().wl_surface().clone()),
                            serial,
                        );
                        self.space.elements().for_each(|window| {
                            window.toplevel().unwrap().send_pending_configure();
                        });
                    } else {
                        self.space.elements().for_each(|window| {
                            window.set_activated(false);
                            window.toplevel().unwrap().send_pending_configure();
                        });
                        keyboard.set_focus(self, Option::<WlSurface>::None, serial);
                    }
                };

                pointer.button(
                    self,
                    &ButtonEvent {
                        button,
                        state: button_state,
                        serial,
                        time: event.time_msec(),
                    },
                );
                pointer.frame(self);
            }
            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();

                let horizontal_amount = event.amount(Axis::Horizontal).unwrap_or_else(|| {
                    event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.
                });
                let vertical_amount = event.amount(Axis::Vertical).unwrap_or_else(|| {
                    event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.
                });
                let horizontal_amount_discrete = event.amount_v120(Axis::Horizontal);
                let vertical_amount_discrete = event.amount_v120(Axis::Vertical);

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if horizontal_amount != 0.0 {
                    frame = frame.value(Axis::Horizontal, horizontal_amount);
                    if let Some(discrete) = horizontal_amount_discrete {
                        frame = frame.v120(Axis::Horizontal, discrete as i32);
                    }
                }
                if vertical_amount != 0.0 {
                    frame = frame.value(Axis::Vertical, vertical_amount);
                    if let Some(discrete) = vertical_amount_discrete {
                        frame = frame.v120(Axis::Vertical, discrete as i32);
                    }
                }

                if source == AxisSource::Finger {
                    if event.amount(Axis::Horizontal) == Some(0.0) {
                        frame = frame.stop(Axis::Horizontal);
                    }
                    if event.amount(Axis::Vertical) == Some(0.0) {
                        frame = frame.stop(Axis::Vertical);
                    }
                }

                let pointer = self.seat.get_pointer().unwrap();
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            _ => {}
        }
    }
}
