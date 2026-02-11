use crate::Raven;
use smithay::{desktop::Window, utils::SERIAL_COUNTER};

pub enum Action {
    FocusNext,
    FocusPrevious,
}

enum Direction {
    Next,
    Previous,
}

impl Action {
    pub fn execute(self, raven: &mut Raven) {
        match self {
            Action::FocusNext => {
                change_focus(Direction::Next, raven);
            }
            Action::FocusPrevious => {
                change_focus(Direction::Previous, raven);
            }
        };
    }
}

fn change_focus(direction: Direction, raven: &mut Raven) {
    let keyboard = raven.seat.get_keyboard().unwrap();
    let serial = SERIAL_COUNTER.next_serial();

    let windows: Vec<Window> = raven.space.elements().cloned().collect();
    if windows.is_empty() {
        return;
    }

    let current_focus = keyboard.current_focus();
    let current_idx = current_focus.and_then(|surf| {
        raven
            .window_for_surface(&surf)
            .and_then(|w| windows.iter().position(|win| win == &w))
    });

    let target_idx = match (direction, current_idx) {
        (Direction::Next, Some(i)) => usize::min(i + 1, windows.len() - 1),
        (Direction::Previous, Some(i)) => i.saturating_sub(1),
        _ => return,
    };

    let target = &windows[target_idx];

    if let Some(toplevel) = target.toplevel() {
        raven.set_keyboard_focus(Some(toplevel.wl_surface().clone()), serial);
    }
}
