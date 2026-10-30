//! Keyboard containment and explicit focus for the security confirmations.
use super::*;
use iced::{
    advanced::{
        layout, overlay, renderer,
        widget::{self, Tree},
        Clipboard, Layout, Renderer as _, Shell, Widget,
    },
    Event, Rectangle, Size,
};
use std::{
    cell::RefCell,
    rc::Rc,
    sync::{Arc, Mutex},
};

struct Shield<'a> {
    child: Element<'a, Message>,
    capture_input: bool,
    block_operations: bool,
    escape: Option<Message>,
    mission_input: bool,
    mission_scope: String,
    form_group: Option<FocusGroup>,
}

#[derive(Default)]
struct ShieldState {
    mission_input: bool,
    escape_armed: bool,
    mission_scope: String,
}

pub(crate) fn shield(child: Element<'_, Message>, escape: Option<Message>) -> Element<'_, Message> {
    Element::new(Shield {
        child,
        capture_input: true,
        block_operations: false,
        escape,
        mission_input: false,
        mission_scope: String::new(),
        form_group: None,
    })
}

pub(crate) fn background(child: Element<'_, Message>, blocked: bool) -> Element<'_, Message> {
    Element::new(Shield {
        child,
        capture_input: false,
        block_operations: blocked,
        escape: None,
        mission_input: false,
        mission_scope: String::new(),
        form_group: None,
    })
}

pub(crate) fn mission_name_input(child: Element<'_, Message>) -> Element<'_, Message> {
    Element::new(Shield {
        child,
        capture_input: false,
        block_operations: false,
        escape: None,
        mission_input: true,
        mission_scope: accessibility::current_build_scope(),
        form_group: None,
    })
}

pub(crate) fn form_input<'a>(
    child: Element<'a, Message>,
    group: &FocusGroup,
) -> Element<'a, Message> {
    Element::new(Shield {
        child,
        capture_input: false,
        block_operations: false,
        escape: None,
        mission_input: false,
        mission_scope: String::new(),
        form_group: Some(group.clone()),
    })
}

pub(crate) fn mission_escape_event(
    event: &Event,
    eligible: bool,
    armed: &mut bool,
    shell: &mut Shell<'_, Message>,
) -> bool {
    match event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: Key::Named(Named::Escape),
            repeat: false,
            ..
        }) if eligible => {
            *armed = true;
            shell.capture_event();
            true
        }
        Event::Keyboard(keyboard::Event::KeyReleased {
            key: Key::Named(Named::Escape),
            ..
        }) if *armed => {
            *armed = false;
            shell.publish(Message::FinishMissionName(false));
            shell.capture_event();
            true
        }
        _ => false,
    }
}

fn capture_modal_input(event: &Event, shell: &mut Shell<'_, Message>, escape: Option<&Message>) {
    if let (true, Some(message)) = (
        matches!(
            event,
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: Key::Named(Named::Escape),
                ..
            })
        ),
        escape,
    ) {
        shell.publish(message.clone());
    }
    if matches!(event, Event::Keyboard(_) | Event::InputMethod(_)) {
        shell.capture_event();
    }
}

impl Widget<Message, Theme, iced::Renderer> for Shield<'_> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<ShieldState>()
    }
    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(ShieldState {
            mission_input: self.mission_input,
            escape_armed: false,
            mission_scope: self.mission_scope.clone(),
        })
    }
    fn size(&self) -> Size<Length> {
        self.child.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.child.as_widget().size_hint()
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.child.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<ShieldState>();
        if state.mission_input != self.mission_input || state.mission_scope != self.mission_scope {
            state.mission_input = self.mission_input;
            state.escape_armed = false;
            state.mission_scope.clone_from(&self.mission_scope);
        }
        tree.diff_children(std::slice::from_ref(&self.child));
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.child
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        if !self.block_operations {
            self.child
                .as_widget_mut()
                .operate(&mut tree.children[0], layout, renderer, operation);
        }
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.mission_input
            && !shell.is_event_captured()
            && mission_escape_event(
                event,
                true,
                &mut tree.state.downcast_mut::<ShieldState>().escape_armed,
                shell,
            )
        {
            return;
        }
        if let Some(group) = &self.form_group {
            type InputState = iced::widget::text_input::State<
                <iced::Renderer as iced::advanced::text::Renderer>::Paragraph,
            >;
            let child = &tree.children[0];
            let input_focused = std::iter::once(child)
                .chain(child.children.first())
                .any(|input| {
                    input.tag == widget::tree::Tag::of::<InputState>()
                        && input.state.downcast_ref::<InputState>().is_focused()
                });
            let activation = group.form_activation();
            let mut messages = Vec::new();
            let mut child_shell = Shell::new(&mut messages);
            self.child.as_widget_mut().update(
                &mut tree.children[0],
                event,
                layout,
                cursor,
                renderer,
                clipboard,
                &mut child_shell,
                viewport,
            );
            shell.merge(child_shell, |message| {
                if matches!(
                    &message,
                    Message::SubmitAuthPrompt { .. }
                        | Message::ConfirmConnection
                        | Message::ConfirmConnectionFor(_)
                ) {
                    Message::DialogFocusActivate {
                        activation: activation.clone(),
                        action: Box::new(message),
                        lease: None,
                    }
                } else {
                    message
                }
            });
            let activation_key = match event {
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: Key::Named(Named::Enter | Named::Space),
                    ..
                }) => true,
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: Key::Character(value),
                    ..
                }) => value.as_str() == " ",
                _ => false,
            };
            if input_focused && activation_key {
                shell.capture_event();
            }
        } else {
            self.child.as_widget_mut().update(
                &mut tree.children[0],
                event,
                layout,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
        // iced::stack otherwise forwards uncaptured keyboard and IME events to
        // a focused input underneath the scrim, including in another HWND.
        if self.capture_input {
            capture_modal_input(event, shell, self.escape.as_ref());
        }
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.child.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, iced::Renderer>> {
        self.child.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.child.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
}

pub(crate) struct FocusState {
    scope: String,
    index: usize,
    group: FocusGroup,
    pointer_activation: Option<FocusActivation>,
}

// UIA issues requests off-thread while iced applies them later. The revision,
// pending request and selection share one lock so checking a ticket and
// applying or activating its selection have one ordering.
#[derive(Debug)]
struct FocusSelection {
    #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
    diagnostic_id: u64,
    revision: u64,
    selected: Option<usize>,
    pending: bool,
    pending_target: Option<usize>,
    pending_invoke: bool,
    #[cfg_attr(not(windows), allow(dead_code))]
    pending_refusal: bool,
    blocked: bool,
}

impl FocusSelection {
    fn cancel(&mut self) {
        // An old trust selection may not be activated after an accepted
        // request was lost or its window lease was revoked.
        self.blocked = self.selected != Some(0);
        self.pending = false;
        self.pending_target = None;
        self.pending_invoke = false;
        self.pending_refusal = false;
        self.revision = self.revision.wrapping_add(1);
    }
}

/// Classify the action itself, not its position: host-key dialogs have two
/// refusal buttons while the link dialog has only one.
#[cfg(windows)]
pub(crate) fn is_refusal(action: &Message) -> bool {
    matches!(
        action,
        Message::RespondHostKey {
            decision: adit_session::HostKeyDecision::Reject
                | adit_session::HostKeyDecision::RejectAndRecord,
            ..
        } | Message::CancelOpenHyperlink(_)
            | Message::CancelAuthPrompt { .. }
            | Message::CancelConnection
            | Message::CancelConnectionFor(_)
    )
}

#[cfg(all(windows, feature = "native-accessibility-acceptance"))]
fn trace_selection(event: &'static str, target: Option<usize>, state: &FocusSelection) {
    accessibility::diagnostics::event(format_args!(
        "{event} group={} revision={} selected={:?} pending={} pending_target={:?} pending_invoke={} pending_refusal={} blocked={} target={target:?}",
        state.diagnostic_id, state.revision, state.selected, state.pending, state.pending_target, state.pending_invoke, state.pending_refusal, state.blocked,
    ));
}

#[derive(Clone, Debug)]
pub(crate) struct FocusRevision(Arc<Mutex<FocusSelection>>);

#[derive(Clone, Debug)]
pub struct FocusTicket {
    revision: FocusRevision,
    value: u64,
}

#[derive(Clone, Debug)]
pub struct FocusActivation {
    revision: FocusRevision,
    value: u64,
    index: Option<usize>,
    pending_target: Option<usize>,
}

impl FocusActivation {
    #[cfg(windows)]
    pub(crate) fn belongs_to(&self, revision: &FocusRevision) -> bool {
        self.revision.same_as(revision)
    }

    pub(crate) fn current(&self) -> bool {
        self.revision.0.lock().is_ok_and(|state| self.valid(&state))
    }

    fn valid(&self, state: &FocusSelection) -> bool {
        let settled = !state.pending
            && !state.blocked
            && self.index.is_none_or(|index| state.selected == Some(index));
        let requested = state.pending
            && state.pending_invoke
            && self.index.is_none()
            && self.pending_target.is_some()
            && self.pending_target == state.pending_target;
        state.revision == self.value && (settled || requested)
    }

    pub(crate) fn commit<T>(
        &self,
        allowed: impl FnOnce() -> bool,
        action: impl FnOnce() -> T,
    ) -> Option<T> {
        let mut state = self.revision.0.lock().ok()?;
        #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
        trace_selection("commit-check", self.pending_target.or(self.index), &state);
        if !self.valid(&state) {
            return None;
        }
        if !allowed() {
            state.cancel();
            return None;
        }
        // Once a decision commits, even an accepted focus task for this
        // dialog must not foreground a window after the dialog is gone.
        state.revision = state.revision.wrapping_add(1);
        state.pending = false;
        state.pending_target = None;
        state.pending_invoke = false;
        state.pending_refusal = false;
        state.blocked = false;
        #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
        trace_selection(
            "commit-accepted",
            self.pending_target.or(self.index),
            &state,
        );
        Some(action())
    }

    pub(crate) fn cancel(&self) {
        if let Ok(mut state) = self.revision.0.lock() {
            if self.valid(&state) {
                state.cancel();
            }
        }
    }
}

impl FocusRevision {
    #[cfg(windows)]
    pub(crate) fn enqueue_activation(
        &self,
        target: Option<usize>,
        refusal: bool,
        send: impl FnOnce(FocusActivation) -> bool,
    ) -> bool {
        let Some(target) = target else { return false };
        let Ok(mut state) = self.0.lock() else {
            return false;
        };
        #[cfg(feature = "native-accessibility-acceptance")]
        trace_selection("invoke-attempt", Some(target), &state);
        // A refusal can supersede an outstanding trust request. A trust
        // invocation cannot supersede a newer, still-pending refusal.
        if !refusal
            && (state.blocked
                || (state.pending
                    && (state.pending_refusal || state.pending_target != Some(target))))
        {
            #[cfg(feature = "native-accessibility-acceptance")]
            trace_selection("invoke-denied", Some(target), &state);
            return false;
        }
        let activation = FocusActivation {
            revision: self.clone(),
            value: state.revision.wrapping_add(1),
            index: None,
            pending_target: Some(target),
        };
        if send(activation.clone()) {
            state.revision = activation.value;
            state.pending = true;
            state.pending_target = Some(target);
            state.pending_invoke = true;
            state.pending_refusal = refusal;
            #[cfg(feature = "native-accessibility-acceptance")]
            trace_selection("invoke-enqueued", Some(target), &state);
            true
        } else {
            if refusal {
                state.cancel();
            }
            #[cfg(feature = "native-accessibility-acceptance")]
            trace_selection("invoke-queue-full", Some(target), &state);
            false
        }
    }

    #[cfg(test)]
    pub(crate) fn issue(&self) -> FocusTicket {
        let mut issued = None;
        assert!(self.enqueue(None, false, |ticket| {
            issued = Some(ticket);
            true
        }));
        issued.unwrap()
    }

    #[cfg(any(windows, test))]
    pub(crate) fn enqueue(
        &self,
        target: Option<usize>,
        refusal: bool,
        send: impl FnOnce(FocusTicket) -> bool,
    ) -> bool {
        let Ok(mut state) = self.0.lock() else {
            return false;
        };
        #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
        trace_selection("focus-attempt", target, &state);
        // UIA's Invoke emits Focus first. It must not turn a pending refusal
        // into a pending trust before the corresponding Click arrives.
        if state.pending && state.pending_refusal && (state.pending_invoke || !refusal) {
            #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
            trace_selection("focus-denied", target, &state);
            return false;
        }
        let ticket = FocusTicket {
            revision: self.clone(),
            value: state.revision.wrapping_add(1),
        };
        if send(ticket.clone()) {
            state.revision = ticket.value;
            state.pending = true;
            state.pending_target = target;
            state.pending_invoke = false;
            state.pending_refusal = refusal;
            #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
            trace_selection("focus-enqueued", target, &state);
            true
        } else {
            if refusal {
                state.cancel();
            }
            #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
            trace_selection("focus-queue-full", target, &state);
            false
        }
    }

    pub(crate) fn same_as(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    #[cfg(windows)]
    pub(crate) fn retire(&self) {
        if let Ok(mut state) = self.0.lock() {
            state.revision = state.revision.wrapping_add(1);
            state.pending = false;
            state.pending_target = None;
            state.pending_invoke = false;
            state.pending_refusal = false;
        }
    }
}

impl FocusTicket {
    pub(crate) fn current(&self) -> bool {
        self.revision
            .0
            .lock()
            .is_ok_and(|state| state.revision == self.value)
    }

    pub(crate) fn belongs_to(&self, revision: &FocusRevision) -> bool {
        self.revision.same_as(revision)
    }

    #[cfg(windows)]
    pub(crate) fn while_pending<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let state = self.revision.0.lock().ok()?;
        (state.revision == self.value && state.pending).then(action)
    }

    pub(crate) fn cancel(&self) {
        if let Ok(mut state) = self.revision.0.lock() {
            if state.revision == self.value && state.pending {
                // Do not mutate a background window's selection. A previously
                // selected trust action stays inert until an explicit fresh
                // focus or local navigation resolves it.
                state.cancel();
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct FocusGroup {
    revision: Rc<RefCell<FocusRevision>>,
}

impl FocusGroup {
    pub(crate) fn new() -> Self {
        Self {
            revision: Rc::new(RefCell::new(FocusRevision(Arc::new(Mutex::new(
                FocusSelection {
                    #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
                    diagnostic_id: {
                        static NEXT: std::sync::atomic::AtomicU64 =
                            std::sync::atomic::AtomicU64::new(1);
                        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    },
                    revision: 0,
                    selected: Some(0),
                    pending: false,
                    pending_target: None,
                    pending_invoke: false,
                    pending_refusal: false,
                    blocked: false,
                },
            ))))),
        }
    }

    fn select(&self, index: usize) {
        if let Ok(mut state) = self.revision.borrow().0.lock() {
            if state.pending && state.pending_invoke && state.pending_refusal {
                return;
            }
            state.revision = state.revision.wrapping_add(1);
            state.selected = Some(index);
            state.pending = false;
            state.pending_target = None;
            state.pending_invoke = false;
            state.pending_refusal = false;
            state.blocked = false;
            #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
            trace_selection("local-select", Some(index), &state);
        }
    }

    fn press(&self, index: usize) -> Option<FocusActivation> {
        let revision = self.revision();
        let mut state = revision.0.lock().ok()?;
        if state.pending && state.pending_invoke && state.pending_refusal {
            return None;
        }
        state.revision = state.revision.wrapping_add(1);
        state.selected = Some(index);
        state.pending = false;
        state.pending_target = None;
        state.pending_invoke = false;
        state.pending_refusal = false;
        state.blocked = false;
        Some(FocusActivation {
            revision: revision.clone(),
            value: state.revision,
            index: Some(index),
            pending_target: None,
        })
    }

    fn form_activation(&self) -> Option<FocusActivation> {
        let revision = self.revision();
        let state = revision.0.lock().ok()?;
        if state.pending || state.blocked {
            return None;
        }
        Some(FocusActivation {
            revision: revision.clone(),
            value: state.revision,
            index: None,
            pending_target: None,
        })
    }

    fn inherit(&self, old: &Self) {
        // Keep outstanding tickets tied to the same window and dialog through
        // view rebuilds; a new scope instead gets a fresh revision.
        let revision = old.revision.borrow().clone();
        #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
        if let Ok(state) = revision.0.lock() {
            trace_selection("inherit-group", None, &state);
        }
        *self.revision.borrow_mut() = revision;
    }

    pub(crate) fn revision(&self) -> FocusRevision {
        self.revision.borrow().clone()
    }
}

impl Default for FocusGroup {
    fn default() -> Self {
        Self::new()
    }
}

impl FocusState {
    pub(crate) fn activation(&self) -> Option<FocusActivation> {
        self.group.form_activation()
    }

    pub(crate) fn focused(&self) -> bool {
        self.group
            .revision()
            .0
            .lock()
            .is_ok_and(|state| state.selected == Some(self.index))
    }

    pub(crate) fn focus(&mut self) {
        self.group.select(self.index);
    }

    pub(crate) fn focus_if_current(
        &mut self,
        ticket: Option<&FocusTicket>,
        allowed: impl FnOnce() -> bool,
    ) -> bool {
        let Some(ticket) = ticket else {
            if allowed() {
                self.focus();
                return true;
            }
            return false;
        };
        let revision = self.group.revision();
        if ticket.belongs_to(&revision) {
            if let Ok(mut state) = revision.0.lock() {
                if state.revision == ticket.value && state.pending && allowed() {
                    state.selected = Some(self.index);
                    state.pending = false;
                    state.pending_target = None;
                    state.pending_invoke = false;
                    state.pending_refusal = false;
                    state.blocked = false;
                    #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
                    trace_selection("focus-applied", Some(self.index), &state);
                    return true;
                }
            }
        }
        false
    }

    #[cfg(windows)]
    pub(crate) fn revision(&self) -> FocusRevision {
        self.group.revision()
    }

    #[cfg(windows)]
    pub(crate) fn index(&self) -> usize {
        self.index
    }
}

pub(crate) struct ButtonFocus {
    pub scope: String,
    pub id: widget::Id,
    pub index: usize,
    pub count: usize,
    pub group: FocusGroup,
    pub action: Message,
}

struct FocusButton<'a> {
    child: Element<'a, Message>,
    focus: ButtonFocus,
    capture_keyboard: bool,
}

pub(crate) fn button(child: Element<'_, Message>, focus: ButtonFocus) -> Element<'_, Message> {
    Element::new(FocusButton {
        child,
        focus,
        capture_keyboard: true,
    })
}

/// Form fields own their keyboard input, while their actions still need the
/// same ordered, window-scoped activation lease as other confirmations.
pub(crate) fn form_action(child: Element<'_, Message>, focus: ButtonFocus) -> Element<'_, Message> {
    Element::new(FocusButton {
        child,
        focus,
        capture_keyboard: true,
    })
}

fn confirmation_key(
    focus: &ButtonFocus,
    state: &FocusState,
    event: &Event,
    shell: &mut Shell<'_, Message>,
) -> bool {
    // Row visits later siblings even after one captured the event.
    if shell.is_event_captured() {
        return true;
    }
    let Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) = event else {
        return false;
    };
    if modifiers.control() || modifiers.alt() || modifiers.logo() {
        return false;
    }
    let revision = state.group.revision();
    let Ok(mut selection) = revision.0.lock() else {
        shell.capture_event();
        return true;
    };
    if selection.selected != Some(state.index) {
        return false;
    }
    match key {
        Key::Named(Named::Tab) => {
            if selection.pending && selection.pending_invoke && selection.pending_refusal {
                shell.capture_event();
                return true;
            }
            let next = if selection.blocked {
                0
            } else if modifiers.shift() {
                // Do not wrap the safe default backwards onto trust/open: an
                // immediately following Enter could otherwise accept before
                // the newly selected action has even been painted.
                focus.index.saturating_sub(1)
            } else {
                (focus.index + 1) % focus.count
            };
            // The next event in this same iced batch must see this selection.
            selection.revision = selection.revision.wrapping_add(1);
            selection.selected = Some(next);
            selection.pending = false;
            selection.pending_target = None;
            selection.pending_invoke = false;
            selection.pending_refusal = false;
            selection.blocked = false;
            #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
            trace_selection("keyboard-tab", Some(next), &selection);
            shell.request_redraw();
            shell.capture_event();
            true
        }
        Key::Named(Named::Enter | Named::Space) => {
            #[cfg(all(windows, feature = "native-accessibility-acceptance"))]
            {
                accessibility::diagnostics::keyboard_attempt();
                trace_selection("keyboard-activate", Some(focus.index), &selection);
            }
            if !selection.pending && !selection.blocked {
                shell.publish(Message::DialogFocusActivate {
                    activation: Some(FocusActivation {
                        revision: revision.clone(),
                        value: selection.revision,
                        index: Some(focus.index),
                        pending_target: None,
                    }),
                    action: Box::new(focus.action.clone()),
                    lease: None,
                });
            }
            shell.capture_event();
            true
        }
        Key::Character(value) if value.as_str() == " " => {
            if !selection.pending && !selection.blocked {
                shell.publish(Message::DialogFocusActivate {
                    activation: Some(FocusActivation {
                        revision: revision.clone(),
                        value: selection.revision,
                        index: Some(focus.index),
                        pending_target: None,
                    }),
                    action: Box::new(focus.action.clone()),
                    lease: None,
                });
            }
            shell.capture_event();
            true
        }
        _ => false,
    }
}

impl Widget<Message, Theme, iced::Renderer> for FocusButton<'_> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<FocusState>()
    }
    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(FocusState {
            scope: self.focus.scope.clone(),
            index: self.focus.index,
            group: self.focus.group.clone(),
            pointer_activation: None,
        })
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.child.as_widget())]
    }
    fn diff(&self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<FocusState>();
        if state.scope == self.focus.scope {
            self.focus.group.inherit(&state.group);
        } else {
            state.pointer_activation = None;
        }
        state.scope = self.focus.scope.clone();
        state.index = self.focus.index;
        state.group = self.focus.group.clone();
        tree.diff_children(std::slice::from_ref(&self.child));
    }
    fn size(&self) -> Size<Length> {
        self.child.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.child.as_widget().size_hint()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.child
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &iced::Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        // Global iced focus operations unfocus every focusable they encounter.
        // Confirmation focus is custom, so an old unrelated task cannot clear
        // the safe default in a newly opened confirmation.
        operation.custom(
            Some(&self.focus.id),
            layout.bounds(),
            tree.state.downcast_mut::<FocusState>(),
        );
        self.child
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &iced::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        if self.capture_keyboard
            && confirmation_key(
                &self.focus,
                tree.state.downcast_ref::<FocusState>(),
                event,
                shell,
            )
        {
            return;
        }
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            | Event::Touch(iced::touch::Event::FingerPressed { .. })
                if cursor.is_over(layout.bounds()) =>
            {
                let state = tree.state.downcast_mut::<FocusState>();
                state.pointer_activation = state.group.press(self.focus.index);
                shell.request_redraw();
            }
            _ => {}
        }
        let mut child_messages = Vec::new();
        let mut child_shell = Shell::new(&mut child_messages);
        self.child.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            &mut child_shell,
            viewport,
        );
        let activation = if matches!(
            event,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
                | Event::Touch(iced::touch::Event::FingerLifted { .. })
                | Event::Touch(iced::touch::Event::FingerLost { .. })
        ) {
            tree.state
                .downcast_mut::<FocusState>()
                .pointer_activation
                .take()
        } else {
            None
        };
        shell.merge(child_shell, |message| Message::DialogFocusActivate {
            activation: activation.clone(),
            action: Box::new(message),
            lease: None,
        });
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        self.child.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &iced::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, iced::Renderer>> {
        self.child.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.child.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
        if tree.state.downcast_ref::<FocusState>().focused() {
            renderer.fill_quad(
                renderer::Quad {
                    bounds: layout.bounds(),
                    border: iced::Border {
                        color: accent(),
                        width: 2.0,
                        radius: 4.0.into(),
                    },
                    ..Default::default()
                },
                Color::TRANSPARENT,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn mfa_cancel_is_a_security_refusal() {
        assert!(is_refusal(&Message::CancelAuthPrompt {
            session_id: SessionId::new(),
            challenge_id: 1
        }));
        assert!(!is_refusal(&Message::SubmitAuthPrompt {
            session_id: SessionId::new(),
            challenge_id: 1
        }));
    }

    #[cfg(windows)]
    #[test]
    fn queued_form_submit_and_later_refusal_do_not_send_answers() {
        let group = FocusGroup::new();
        let old_submit = group.form_activation().unwrap();
        let mut cancel = None;
        assert!(group.revision().enqueue_activation(Some(0), true, |value| {
            cancel = Some(value);
            true
        }));
        assert!(old_submit.commit(|| true, || ()).is_none());
        assert!(group.form_activation().is_none());
        assert!(cancel.unwrap().commit(|| true, || ()).is_some());
    }

    fn focused_action(message: &Message) -> &Message {
        match message {
            Message::DialogFocusActivate { action, .. } => action,
            _ => message,
        }
    }

    fn pressed(named: Named, modifiers: keyboard::Modifiers) -> Event {
        let key = Key::Named(named);
        let code = match named {
            Named::Tab => keyboard::key::Code::Tab,
            Named::Enter => keyboard::key::Code::Enter,
            Named::Space => keyboard::key::Code::Space,
            _ => keyboard::key::Code::Escape,
        };
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        })
    }

    #[test]
    fn batched_tabs_and_activation_use_the_synchronously_selected_action() {
        use adit_session::HostKeyDecision;
        let group = FocusGroup::new();
        let decisions = [
            HostKeyDecision::Reject,
            HostKeyDecision::RejectAndRecord,
            HostKeyDecision::TrustOnce,
            HostKeyDecision::TrustAndRecord,
        ];
        let buttons: Vec<_> = decisions
            .iter()
            .enumerate()
            .map(|(index, decision)| ButtonFocus {
                scope: "challenge:synthetic".into(),
                id: widget::Id::from(format!("button-{index}")),
                index,
                count: decisions.len(),
                group: group.clone(),
                action: Message::RespondHostKey {
                    session_id: SessionId::new(),
                    challenge_id: 1,
                    decision: *decision,
                },
            })
            .collect();
        let states: Vec<_> = (0..decisions.len())
            .map(|index| FocusState {
                scope: "challenge:synthetic".into(),
                index,
                group: group.clone(),
                pointer_activation: None,
            })
            .collect();
        let mut messages = Vec::new();
        let mut batch = |events: &[Event]| {
            for event in events {
                let mut shell = Shell::new(&mut messages);
                for (button, state) in buttons.iter().zip(&states) {
                    confirmation_key(button, state, event, &mut shell);
                }
                assert!(shell.is_event_captured());
            }
        };
        batch(&[
            pressed(Named::Tab, keyboard::Modifiers::empty()),
            pressed(Named::Space, keyboard::Modifiers::empty()),
        ]);
        assert!(matches!(
            messages.last().map(focused_action),
            Some(Message::RespondHostKey {
                decision: HostKeyDecision::RejectAndRecord,
                ..
            })
        ));
        messages.clear();
        group.select(3);
        for event in [
            pressed(Named::Tab, keyboard::Modifiers::SHIFT),
            pressed(Named::Enter, keyboard::Modifiers::empty()),
        ] {
            let mut shell = Shell::new(&mut messages);
            for (button, state) in buttons.iter().zip(&states) {
                confirmation_key(button, state, &event, &mut shell);
            }
            assert!(shell.is_event_captured());
        }
        assert!(matches!(
            messages.last().map(focused_action),
            Some(Message::RespondHostKey {
                decision: HostKeyDecision::TrustOnce,
                ..
            })
        ));
        assert_eq!(messages.len(), 1);
        messages.clear();
        group.select(0);
        for event in [
            pressed(Named::Tab, keyboard::Modifiers::SHIFT),
            pressed(Named::Enter, keyboard::Modifiers::empty()),
        ] {
            let mut shell = Shell::new(&mut messages);
            for (button, state) in buttons.iter().zip(&states) {
                confirmation_key(button, state, &event, &mut shell);
            }
            assert!(shell.is_event_captured());
        }
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            focused_action(&messages[0]),
            Message::RespondHostKey {
                decision: HostKeyDecision::Reject,
                ..
            }
        ));
    }

    #[cfg(windows)]
    #[test]
    fn queued_reject_invoke_cannot_be_overridden_by_tab_and_enter() {
        use adit_session::HostKeyDecision;
        let group = FocusGroup::new();
        let session_id = SessionId::new();
        let decisions = [
            HostKeyDecision::Reject,
            HostKeyDecision::RejectAndRecord,
            HostKeyDecision::TrustOnce,
            HostKeyDecision::TrustAndRecord,
        ];
        let buttons: Vec<_> = decisions
            .iter()
            .enumerate()
            .map(|(index, decision)| ButtonFocus {
                scope: "challenge:fixture".into(),
                id: widget::Id::from(format!("button-{index}")),
                index,
                count: 4,
                group: group.clone(),
                action: Message::RespondHostKey {
                    session_id,
                    challenge_id: 116,
                    decision: *decision,
                },
            })
            .collect();
        let states: Vec<_> = (0..4)
            .map(|index| FocusState {
                scope: "challenge:fixture".into(),
                index,
                group: group.clone(),
                pointer_activation: None,
            })
            .collect();
        let mut rejection = None;
        assert!(group.revision().enqueue_activation(Some(0), true, |value| {
            rejection = Some(value);
            true
        }));
        let mut messages = Vec::new();
        for event in [
            pressed(Named::Tab, keyboard::Modifiers::empty()),
            pressed(Named::Tab, keyboard::Modifiers::empty()),
            pressed(Named::Tab, keyboard::Modifiers::empty()),
            pressed(Named::Enter, keyboard::Modifiers::empty()),
        ] {
            let mut shell = Shell::new(&mut messages);
            for (button, state) in buttons.iter().zip(&states) {
                confirmation_key(button, state, &event, &mut shell);
            }
        }
        assert!(
            rejection.unwrap().current(),
            "queued rejection lost to keyboard navigation"
        );
        assert!(
            messages.is_empty(),
            "keyboard activated a decision before the queued refusal"
        );
    }

    #[cfg(windows)]
    #[test]
    fn queued_reject_invoke_cannot_be_overridden_by_pointer_or_unscoped_focus() {
        let group = FocusGroup::new();
        let mut rejection = None;
        assert!(group.revision().enqueue_activation(Some(1), true, |value| {
            rejection = Some(value);
            true
        }));
        assert!(group.press(3).is_none());
        group.select(3);
        assert!(!group.revision().enqueue(Some(0), true, |_| true));
        assert!(rejection.unwrap().current());
        let selected = group.revision();
        assert_eq!(selected.0.lock().unwrap().selected, Some(0));
    }

    #[cfg(windows)]
    #[test]
    fn queued_refusal_focus_does_not_block_later_keyboard_navigation() {
        let group = FocusGroup::new();
        let mut ticket = None;
        assert!(group.revision().enqueue(Some(0), true, |value| {
            ticket = Some(value);
            true
        }));
        let button = ButtonFocus {
            scope: "fixture".into(),
            id: widget::Id::from("reject"),
            index: 0,
            count: 4,
            group: group.clone(),
            action: Message::ClearError,
        };
        let state = FocusState {
            scope: "fixture".into(),
            index: 0,
            group: group.clone(),
            pointer_activation: None,
        };
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        assert!(confirmation_key(
            &button,
            &state,
            &pressed(Named::Tab, keyboard::Modifiers::empty()),
            &mut shell
        ));
        assert!(!ticket.unwrap().current());
        let selected = group.revision();
        assert_eq!(selected.0.lock().unwrap().selected, Some(1));
    }

    #[test]
    fn legacy_global_focus_operation_cannot_clear_confirmation_default() {
        use iced::advanced::widget::Operation;
        let group = FocusGroup::new();
        let mut state = FocusState {
            scope: "challenge".into(),
            index: 0,
            group,
            pointer_activation: None,
        };
        let mut old =
            widget::operation::focusable::focus::<()>(widget::Id::from("stale-background"));
        old.custom(
            Some(&widget::Id::from("reject")),
            Rectangle::default(),
            &mut state,
        );
        assert!(state.focused());
    }

    #[test]
    fn mission_escape_arm_is_revoked_by_a_covering_modal() {
        let build = |scope| {
            let _building = accessibility::building(None, scope);
            mission_name_input(iced::widget::text_input("synthetic", "draft").into())
        };
        let rename = build("main");
        let mut tree = Tree::new(rename.as_widget());
        tree.state.downcast_mut::<ShieldState>().escape_armed = true;
        let covered = build("hyperlink:1");
        covered.as_widget().diff(&mut tree);
        assert!(!tree.state.downcast_ref::<ShieldState>().escape_armed);
        let restored = build("main");
        restored.as_widget().diff(&mut tree);
        assert!(!tree.state.downcast_ref::<ShieldState>().escape_armed);
    }

    #[test]
    fn menu_escape_is_captured_and_targets_only_the_visible_menu() {
        let event = pressed(Named::Escape, keyboard::Modifiers::empty());
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        capture_modal_input(
            &event,
            &mut shell,
            Some(&Message::DismissMenu(MenuEscapeTarget::Active(
                MenuKind::File,
            ))),
        );
        assert!(shell.is_event_captured());
        assert!(matches!(
            messages.as_slice(),
            [Message::DismissMenu(MenuEscapeTarget::Active(
                MenuKind::File
            ))]
        ));
    }

    fn button_for(scope: &str, index: usize, group: &FocusGroup) -> Element<'static, Message> {
        let id = widget::Id::from("synthetic-button");
        button(
            iced::widget::button("synthetic").into(),
            ButtonFocus {
                scope: scope.to_owned(),
                id,
                index,
                count: 4,
                group: group.clone(),
                action: Message::ClearError,
            },
        )
    }

    #[test]
    fn replacement_confirmation_resets_to_its_safe_default() {
        let group = FocusGroup::new();
        let old = button_for("challenge:old", 0, &group);
        let mut tree = Tree::new(old.as_widget());
        assert!(tree.state.downcast_ref::<FocusState>().focused());
        group.select(3);
        old.as_widget().diff(&mut tree);
        assert!(!tree.state.downcast_ref::<FocusState>().focused());
        let replacement_group = FocusGroup::new();
        let replacement = button_for("challenge:new", 0, &replacement_group);
        replacement.as_widget().diff(&mut tree);
        assert!(tree.state.downcast_ref::<FocusState>().focused());

        let trust = button_for("challenge:old", 3, &group);
        let mut tree = Tree::new(trust.as_widget());
        tree.state.downcast_mut::<FocusState>().focus();
        let replacement = button_for("challenge:new", 3, &replacement_group);
        replacement.as_widget().diff(&mut tree);
        assert!(!tree.state.downcast_ref::<FocusState>().focused());
    }

    #[test]
    fn rebuilt_buttons_keep_one_shared_selection_for_same_challenge() {
        let old_group = FocusGroup::new();
        let old: Vec<_> = (0..4)
            .map(|i| button_for("challenge:same", i, &old_group))
            .collect();
        let mut trees: Vec<_> = old
            .iter()
            .map(|button| Tree::new(button.as_widget()))
            .collect();
        old_group.select(2);
        let new_group = FocusGroup::new();
        let rebuilt: Vec<_> = (0..4)
            .map(|i| button_for("challenge:same", i, &new_group))
            .collect();
        for (button, tree) in rebuilt.iter().zip(&mut trees) {
            button.as_widget().diff(tree);
        }
        let focused: Vec<_> = trees
            .iter()
            .enumerate()
            .filter_map(|(i, tree)| {
                tree.state
                    .downcast_ref::<FocusState>()
                    .focused()
                    .then_some(i)
            })
            .collect();
        assert_eq!(focused, vec![2]);
    }

    #[test]
    fn acceptance_and_selection_write_are_ordered_and_pending_cannot_activate_trust() {
        use std::{sync::mpsc, time::Duration};
        let group = FocusGroup::new();
        group.select(3);
        let revision = group.revision();
        let old = revision.issue();
        let (start_tx, start_rx) = mpsc::channel();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            start_rx.recv().unwrap();
            attempt_tx.send(()).unwrap();
            done_tx.send(revision.issue()).unwrap();
        });
        let mut trust = FocusState {
            scope: "challenge:fixture".into(),
            index: 3,
            group: group.clone(),
            pointer_activation: None,
        };
        trust.focus_if_current(Some(&old), || {
            start_tx.send(()).unwrap();
            attempt_rx.recv().unwrap();
            assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
            true
        });
        let newer_reject = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        let button = ButtonFocus {
            scope: "challenge:fixture".into(),
            id: widget::Id::from("trust"),
            index: 3,
            count: 4,
            group: group.clone(),
            action: Message::ClearError,
        };
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        assert!(confirmation_key(
            &button,
            &trust,
            &pressed(Named::Enter, keyboard::Modifiers::empty()),
            &mut shell,
        ));
        assert!(messages.is_empty(), "pending rejection activated old trust");
        let mut reject = FocusState {
            scope: "challenge:fixture".into(),
            index: 0,
            group: group.clone(),
            pointer_activation: None,
        };
        reject.focus_if_current(Some(&newer_reject), || true);
        assert!(reject.focused());
        assert!(!trust.focused());
    }

    #[test]
    fn a_newer_accepted_rejection_prevents_the_old_trust_write() {
        let group = FocusGroup::new();
        let revision = group.revision();
        let old = revision.issue();
        let reject = revision.issue();
        let mut trust = FocusState {
            scope: "challenge:fixture".into(),
            index: 3,
            group: group.clone(),
            pointer_activation: None,
        };
        trust.focus_if_current(Some(&old), || true);
        assert!(!trust.focused());
        let mut refusal = FocusState {
            scope: "challenge:fixture".into(),
            index: 0,
            group,
            pointer_activation: None,
        };
        refusal.focus_if_current(Some(&reject), || true);
        assert!(refusal.focused());
    }

    #[test]
    fn queued_enter_cannot_commit_after_a_newer_focus_request() {
        let group = FocusGroup::new();
        group.select(3);
        let state = FocusState {
            scope: "challenge:fixture".into(),
            index: 3,
            group: group.clone(),
            pointer_activation: None,
        };
        let button = ButtonFocus {
            scope: "challenge:fixture".into(),
            id: widget::Id::from("trust"),
            index: 3,
            count: 4,
            group: group.clone(),
            action: Message::ClearError,
        };
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        assert!(confirmation_key(
            &button,
            &state,
            &pressed(Named::Enter, keyboard::Modifiers::empty()),
            &mut shell,
        ));
        assert_eq!(messages.len(), 1);
        let rejection = group.revision().issue();
        let mut app = AditApp {
            last_error: Some("synthetic".into()),
            ..AditApp::default()
        };
        let _ = update_loop::update(&mut app, messages.pop().unwrap());
        assert_eq!(app.last_error.as_deref(), Some("synthetic"));
        assert!(rejection.current());
    }

    #[test]
    fn pointer_release_cannot_commit_after_a_newer_focus_request() {
        use iced_runtime::user_interface::{Cache, UserInterface};
        let group = FocusGroup::new();
        let control = button(
            iced::widget::button("synthetic trust")
                .width(120)
                .height(40)
                .on_press(Message::ClearError)
                .into(),
            ButtonFocus {
                scope: "challenge:fixture".into(),
                id: widget::Id::from("trust"),
                index: 3,
                count: 4,
                group: group.clone(),
                action: Message::ClearError,
            },
        );
        let mut renderer = iced::Renderer::Secondary(iced_tiny_skia::Renderer::new(
            iced::Font::default(),
            iced::Pixels(16.0),
        ));
        let mut ui = UserInterface::build(
            control,
            iced::Size::new(150.0, 60.0),
            Cache::new(),
            &mut renderer,
        );
        let mut clipboard = iced::advanced::clipboard::Null;
        let cursor = mouse::Cursor::Available(iced::Point::new(10.0, 10.0));
        let mut messages = Vec::new();
        ui.update(
            &[Event::Mouse(mouse::Event::ButtonPressed(
                mouse::Button::Left,
            ))],
            cursor,
            &mut renderer,
            &mut clipboard,
            &mut messages,
        );
        let rejection = group.revision().issue();
        ui.update(
            &[Event::Mouse(mouse::Event::ButtonReleased(
                mouse::Button::Left,
            ))],
            cursor,
            &mut renderer,
            &mut clipboard,
            &mut messages,
        );
        assert_eq!(messages.len(), 1);
        let mut app = AditApp {
            last_error: Some("synthetic".into()),
            ..AditApp::default()
        };
        let _ = update_loop::update(&mut app, messages.pop().unwrap());
        assert_eq!(app.last_error.as_deref(), Some("synthetic"));
        assert!(rejection.current());

        ui.update(
            &[
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ],
            cursor,
            &mut renderer,
            &mut clipboard,
            &mut messages,
        );
        assert_eq!(messages.len(), 1);
        let _ = update_loop::update(&mut app, messages.pop().unwrap());
        assert!(app.last_error.is_none());
    }

    #[test]
    fn rejected_enqueue_cannot_discard_an_accepted_rejection() {
        let group = FocusGroup::new();
        group.select(3);
        let revision = group.revision();
        let accepted = revision.issue();
        assert!(!revision.enqueue(None, false, |_| false));
        assert!(accepted.current());
        let mut refusal = FocusState {
            scope: "challenge:fixture".into(),
            index: 0,
            group: group.clone(),
            pointer_activation: None,
        };
        refusal.focus_if_current(Some(&accepted), || true);
        let trust = FocusState {
            scope: "challenge:fixture".into(),
            index: 3,
            group,
            pointer_activation: None,
        };
        assert!(refusal.focused());
        assert!(!trust.focused());
    }

    #[test]
    fn dequeued_ticket_cannot_observe_an_uncommitted_generation() {
        use std::{sync::mpsc, time::Duration};
        let revision = FocusGroup::new().revision();
        let (queue_tx, queue_rx) = mpsc::sync_channel::<FocusTicket>(1);
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let consumer = std::thread::spawn(move || {
            let ticket = queue_rx.recv().unwrap();
            attempt_tx.send(()).unwrap();
            done_tx.send(ticket.current()).unwrap();
        });
        assert!(revision.enqueue(None, false, |ticket| {
            queue_tx.try_send(ticket).unwrap();
            attempt_rx.recv().unwrap();
            assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
            true
        }));
        assert!(done_rx.recv_timeout(Duration::from_secs(2)).unwrap());
        consumer.join().unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn foreground_action_and_acceptance_share_the_ticket_order() {
        use std::{sync::mpsc, time::Duration};
        let revision = FocusGroup::new().revision();
        let old = revision.issue();
        let (start_tx, start_rx) = mpsc::channel();
        let (attempt_tx, attempt_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            start_rx.recv().unwrap();
            attempt_tx.send(()).unwrap();
            done_tx.send(revision.issue()).unwrap();
        });
        assert!(old
            .while_pending(|| {
                start_tx.send(()).unwrap();
                attempt_rx.recv().unwrap();
                assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
            })
            .is_some());
        let newer = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert!(!old.current());
        assert!(newer.current());
        assert!(old
            .while_pending(|| panic!("stale foreground action"))
            .is_none());
    }

    #[cfg(windows)]
    #[test]
    fn retiring_a_confirmation_revokes_its_pending_foreground_action() {
        let revision = FocusGroup::new().revision();
        let pending = revision.issue();
        revision.retire();
        assert!(!pending.current());
        assert!(pending
            .while_pending(|| panic!("retired dialog foregrounded its window"))
            .is_none());
    }

    #[test]
    fn abandoned_request_leaves_background_selection_but_blocks_trust() {
        let group = FocusGroup::new();
        group.select(3);
        let pending = group.revision().issue();
        pending.cancel();
        let trust = FocusState {
            scope: "challenge:fixture".into(),
            index: 3,
            group: group.clone(),
            pointer_activation: None,
        };
        assert!(trust.focused());
        let button = ButtonFocus {
            scope: "challenge:fixture".into(),
            id: widget::Id::from("trust"),
            index: 3,
            count: 4,
            group: group.clone(),
            action: Message::ClearError,
        };
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        assert!(confirmation_key(
            &button,
            &trust,
            &pressed(Named::Enter, keyboard::Modifiers::empty()),
            &mut shell,
        ));
        assert!(messages.is_empty());
        let mut shell = Shell::new(&mut messages);
        assert!(confirmation_key(
            &button,
            &trust,
            &pressed(Named::Tab, keyboard::Modifiers::empty()),
            &mut shell,
        ));
        let refusal = FocusState {
            scope: "challenge:fixture".into(),
            index: 0,
            group,
            pointer_activation: None,
        };
        assert!(refusal.focused());
        assert!(!pending.current());
    }

    #[test]
    fn modal_captures_keyboard_and_all_ime_phases_before_the_background() {
        use iced::advanced::input_method;
        for event in [
            Event::Keyboard(keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::default(),
            )),
            Event::InputMethod(input_method::Event::Opened),
            Event::InputMethod(input_method::Event::Preedit("SYNTHETIC".into(), Some(0..1))),
            Event::InputMethod(input_method::Event::Commit("SYNTHETIC".into())),
            Event::InputMethod(input_method::Event::Closed),
        ] {
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            capture_modal_input(&event, &mut shell, None);
            assert!(shell.is_event_captured());
        }
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        capture_modal_input(&Event::Mouse(mouse::Event::CursorLeft), &mut shell, None);
        assert!(!shell.is_event_captured());
    }
}
