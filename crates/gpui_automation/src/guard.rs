//! Keeping the real mouse out of scripted runs.
//!
//! Automated windows open on the user's desktop. When the real cursor crosses one, the
//! platform delivers its moves, exits and wheel events through the same
//! `Window::dispatch_event` as the driver's synthetic ones, so a scripted drag can end where
//! the real cursor was rather than where the script put it. [`pointer_guard`] drops those
//! platform events while automation is running: the driver marks its own dispatches, and a
//! capture-phase listener stops everything else before any element sees it.
//!
//! gpui still records the real cursor position before listeners run, which only affects
//! hover styling until the driver's next pointer event. Keyboard input is not filtered.

use std::cell::Cell;

use gpui::{
    AnyElement, App, DispatchPhase, Empty, IntoElement, MouseDownEvent, MouseEvent, MouseExitEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, ScrollEvent, Styled as _, Window, canvas,
    div,
};

thread_local! {
    static SYNTHETIC: Cell<bool> = const { Cell::new(false) };
}

/// Marks input dispatched while it is alive as the driver's own.
pub(crate) struct SyntheticInput {
    previous: bool,
}

impl SyntheticInput {
    pub(crate) fn begin() -> Self {
        Self {
            previous: SYNTHETIC.with(|flag| flag.replace(true)),
        }
    }
}

impl Drop for SyntheticInput {
    fn drop(&mut self) {
        SYNTHETIC.with(|flag| flag.set(self.previous));
    }
}

/// Whether the event being dispatched right now came from the automation driver.
pub fn is_synthetic_input() -> bool {
    SYNTHETIC.with(Cell::get)
}

/// An invisible element that, while automation is running, swallows pointer events the
/// platform delivers (the real mouse) so they can't interleave with scripted input. Render it
/// as the first child of each window's root view; when automation is off it is an empty
/// element and registers nothing.
pub fn pointer_guard() -> AnyElement {
    if crate::is_active() {
        guard_element()
    } else {
        Empty.into_any_element()
    }
}

fn guard_element() -> AnyElement {
    div()
        .absolute()
        .size_0()
        .child(canvas(
            |_, _, _| (),
            |_, (), window, _| {
                block_real::<MouseMoveEvent>(window);
                block_real::<MouseDownEvent>(window);
                block_real::<MouseUpEvent>(window);
                block_real::<MouseExitEvent>(window);
                block_real::<ScrollEvent>(window);
            },
        ))
        .into_any_element()
}

fn block_real<E: MouseEvent>(window: &mut Window) {
    window.on_mouse_event(
        |_: &E, phase: DispatchPhase, _: &mut Window, cx: &mut App| {
            if phase == DispatchPhase::Capture && !is_synthetic_input() {
                cx.stop_propagation();
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use std::rc::Rc;

    use gpui::{
        Context, InteractiveElement as _, Modifiers, MouseButton, Render, TestAppContext,
        VisualTestContext, point, px, size,
    };

    use super::*;

    /// A window whose whole area counts the pointer events that reach it.
    struct Probe {
        seen: Rc<Cell<usize>>,
    }

    impl Render for Probe {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let (moves, downs, wheels) = (self.seen.clone(), self.seen.clone(), self.seen.clone());
            div().relative().size_full().child(guard_element()).child(
                div()
                    .id("probe")
                    .size_full()
                    .on_mouse_move(move |_, _, _| moves.set(moves.get() + 1))
                    .on_mouse_down(MouseButton::Left, move |_, _, _| downs.set(downs.get() + 1))
                    .on_scroll(move |_, _, _| wheels.set(wheels.get() + 1)),
            )
        }
    }

    fn open(cx: &mut TestAppContext) -> (Rc<Cell<usize>>, &mut VisualTestContext) {
        let seen = Rc::new(Cell::new(0));
        let probe_seen = seen.clone();
        let window = cx.open_window(size(px(400.), px(300.)), move |_, _| Probe {
            seen: probe_seen,
        });
        let cx = VisualTestContext::from_window(window.into(), cx).into_mut();
        cx.run_until_parked();
        (seen, cx)
    }

    fn pointer_events(cx: &mut VisualTestContext) {
        let at = point(px(100.), px(100.));
        cx.simulate_mouse_move(at, None, Modifiers::none());
        cx.simulate_mouse_down(at, MouseButton::Left, Modifiers::none());
        cx.simulate_mouse_up(at, MouseButton::Left, Modifiers::none());
        cx.simulate_event(ScrollEvent {
            position: at,
            ..Default::default()
        });
    }

    #[gpui::test]
    fn platform_pointer_events_are_dropped(cx: &mut TestAppContext) {
        let (seen, cx) = open(cx);
        pointer_events(cx);
        assert_eq!(seen.get(), 0, "the platform's events never reach elements");
    }

    #[gpui::test]
    fn driver_pointer_events_get_through(cx: &mut TestAppContext) {
        let (seen, cx) = open(cx);
        {
            let _mark = SyntheticInput::begin();
            pointer_events(cx);
        }
        assert_eq!(seen.get(), 3, "move, down and wheel all arrive");
    }

    #[test]
    fn synthetic_marks_nest_and_unwind() {
        assert!(!is_synthetic_input());
        {
            let _outer = SyntheticInput::begin();
            assert!(is_synthetic_input());
            {
                let _inner = SyntheticInput::begin();
                assert!(is_synthetic_input());
            }
            assert!(
                is_synthetic_input(),
                "the inner mark restores the outer one"
            );
        }
        assert!(!is_synthetic_input());
    }

    #[test]
    fn a_panic_does_not_leave_input_marked() {
        let result = std::panic::catch_unwind(|| {
            let _mark = SyntheticInput::begin();
            panic!("dispatch failed");
        });
        assert!(result.is_err());
        assert!(!is_synthetic_input());
    }
}
