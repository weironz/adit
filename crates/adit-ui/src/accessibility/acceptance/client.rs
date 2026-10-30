//! Native UIA client. It runs on an MTA worker while iced pumps its real HWND.
use super::*;
use windows::{
    core::BSTR,
    Win32::{
        Foundation::{HWND, LPARAM, WPARAM},
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_MULTITHREADED,
        },
        UI::{
            Accessibility::*,
            WindowsAndMessaging::{GetForegroundWindow, PostMessageW, WM_KEYDOWN, WM_KEYUP},
        },
    },
};

struct Com;
impl Drop for Com {
    fn drop(&mut self) {
        unsafe {
            CoUninitialize();
        }
    }
}

struct Client {
    automation: IUIAutomation,
    root: IUIAutomationElement,
}

impl Client {
    fn assert_modal_dialog(&self, element: &IUIAutomationElement) -> Result<(), String> {
        if unsafe { element.CurrentControlType() }.map_err(|e| e.to_string())?
            != UIA_WindowControlTypeId
        {
            return Err("confirmation is not a UIA dialog".into());
        }
        let window: IUIAutomationWindowPattern =
            unsafe { element.GetCurrentPatternAs(UIA_WindowPatternId) }
                .map_err(|e| format!("dialog has no UIA window pattern: {e}"))?;
        if !unsafe { window.CurrentIsModal() }
            .map_err(|e| e.to_string())?
            .as_bool()
        {
            return Err("UIA dialog is not marked modal".into());
        }
        Ok(())
    }

    fn elements(&self) -> Vec<IUIAutomationElement> {
        unsafe {
            let Ok(condition) = self.automation.CreateTrueCondition() else {
                return Vec::new();
            };
            let Ok(elements) = self.root.FindAll(TreeScope_Descendants, &condition) else {
                return Vec::new();
            };
            let length = elements.Length().unwrap_or_default();
            (0..length)
                .filter_map(|i| elements.GetElement(i).ok())
                .collect()
        }
    }

    fn find(&self, name: &str, id_fragment: &str) -> Option<IUIAutomationElement> {
        self.elements().into_iter().find(|element| unsafe {
            element.CurrentName().is_ok_and(|n| n == name)
                && element
                    .CurrentAutomationId()
                    .is_ok_and(|id| id.to_string().contains(id_fragment))
        })
    }

    fn by_id(&self, id_fragment: &str) -> Option<IUIAutomationElement> {
        self.elements().into_iter().find(|element| unsafe {
            element
                .CurrentAutomationId()
                .is_ok_and(|id| id.to_string().contains(id_fragment))
        })
    }

    fn wait<T>(&self, what: &str, mut condition: impl FnMut() -> Option<T>) -> Result<T, String> {
        let until = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(value) = condition() {
                return Ok(value);
            }
            if Instant::now() > until {
                return Err(format!("UIA condition timed out: {what}"));
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn named(&self, name: &str, id: &str) -> Result<IUIAutomationElement, String> {
        self.wait(name, || self.find(name, id))
    }

    fn invoke(&self, element: &IUIAutomationElement) -> Result<(), String> {
        unsafe {
            let pattern: IUIAutomationInvokePattern = element
                .GetCurrentPatternAs(UIA_InvokePatternId)
                .map_err(|error| format!("UIA Invoke pattern unavailable: {error}"))?;
            pattern
                .Invoke()
                .map_err(|error| format!("UIA Invoke failed: {error}"))
        }
    }

    fn value(&self, element: &IUIAutomationElement) -> Option<String> {
        unsafe {
            let pattern: IUIAutomationValuePattern =
                element.GetCurrentPatternAs(UIA_ValuePatternId).ok()?;
            Some(pattern.CurrentValue().ok()?.to_string())
        }
    }

    fn set_value(&self, element: &IUIAutomationElement, value: &str) -> Result<(), String> {
        unsafe {
            let pattern: IUIAutomationValuePattern = element
                .GetCurrentPatternAs(UIA_ValuePatternId)
                .map_err(|error| format!("UIA Value pattern unavailable: {error}"))?;
            pattern
                .SetValue(&BSTR::from(value))
                .map_err(|error| format!("UIA SetValue failed: {error}"))
        }
    }

    fn document(&self, owner: SessionId) -> Option<IUIAutomationElement> {
        self.by_id(&format!("/terminal:{owner}"))
    }

    fn text(&self, element: &IUIAutomationElement) -> Option<String> {
        unsafe {
            let text: IUIAutomationTextPattern =
                element.GetCurrentPatternAs(UIA_TextPatternId).ok()?;
            Some(text.DocumentRange().ok()?.GetText(-1).ok()?.to_string())
        }
    }

    fn privacy(&self) -> Result<usize, String> {
        let mut protected = 0;
        for element in self.elements() {
            let name = unsafe { element.CurrentName() }
                .map(|v| v.to_string())
                .unwrap_or_default();
            let value = self.value(&element).unwrap_or_default();
            let text = self.text(&element).unwrap_or_default();
            if [name.as_str(), value.as_str(), text.as_str()]
                .iter()
                .any(|value| value.contains(PASSWORD) || value.contains(PROXY))
            {
                return Err("UIA exposed synthetic protected content".into());
            }
            if unsafe { element.CurrentIsPassword() }.is_ok_and(|v| v.as_bool()) {
                protected += 1;
                if !value.is_empty() && value != "••••" {
                    return Err("UIA password readback was neither denied nor fixed masking".into());
                }
            }
        }
        Ok(protected)
    }
}

fn command(
    sender: &mpsc::Sender<Command>,
    make: impl FnOnce(mpsc::Sender<Snapshot>) -> Command,
) -> Result<Snapshot, String> {
    let (tx, rx) = mpsc::channel();
    sender.send(make(tx)).map_err(|e| e.to_string())?;
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|e| format!("fixture command timed out: {e}"))
}

fn post_key(hwnd: isize, vk: u16, scan: isize) -> Result<(), String> {
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    let down = 1 | (scan << 16);
    let up = down | (3isize << 30);
    unsafe {
        PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(vk as usize), LPARAM(down))
            .and_then(|_| PostMessageW(Some(hwnd), WM_KEYUP, WPARAM(vk as usize), LPARAM(up)))
            .map_err(|error| format!("posting synthetic key to fixture failed: {error}"))
    }
}

fn post_escape_edge(hwnd: isize, repeat: bool, release: bool) -> Result<(), String> {
    let hwnd = HWND(hwnd as *mut std::ffi::c_void);
    let flags = 1
        | (0x01isize << 16)
        | if release {
            3isize << 30
        } else if repeat {
            1isize << 30
        } else {
            0
        };
    unsafe {
        PostMessageW(
            Some(hwnd),
            if release { WM_KEYUP } else { WM_KEYDOWN },
            WPARAM(0x1b),
            LPARAM(flags),
        )
        .map_err(|error| format!("posting synthetic Escape to fixture failed: {error}"))
    }
}

fn host_focus_diagnostics(client: &Client, hwnd: isize) -> String {
    let actions: Vec<_> = [
        "host-key:reject",
        "host-key:reject-record",
        "host-key:once",
        "host-key:trust",
    ]
    .into_iter()
    .map(|key| {
        let focused = client.by_id(key).and_then(|element| unsafe {
            element.CurrentHasKeyboardFocus().ok().map(|v| v.as_bool())
        });
        format!("{key}={focused:?}")
    })
    .collect();
    let foreground_is_fixture = unsafe { GetForegroundWindow() }.0 as isize == hwnd;
    format!("actions={actions:?}; foreground_is_fixture={foreground_is_fixture}")
}

fn dialog_acceptance(
    client: &Client,
    commands: &mpsc::Sender<Command>,
    hwnd: isize,
) -> Result<(), String> {
    use adit_session::HostKeyDecision;

    let background = client.named("资源库", "navigation:Hosts")?;
    let first = command(commands, |ack| Command::Host(false, ack))?;
    let first_id = first.challenge.ok_or("first host challenge missing")?;
    let dialog = client.named("确认主机密钥", "host-key:dialog")?;
    client.assert_modal_dialog(&dialog)?;
    client.named("fixture.invalid:2222", "host-key:host")?;
    client.named("密钥类型: ssh-ed25519", "host-key:type")?;
    client.named(
        &format!("SHA256 指纹: {FINGERPRINT}"),
        "host-key:fingerprint",
    )?;
    client.named(
        "首次连接此主机。请通过其它可信渠道核对指纹后再信任。",
        "host-key:warning",
    )?;
    if client.by_id("host-key:previous").is_some() || client.by_id("navigation:Missions").is_some()
    {
        return Err("first host dialog exposed previous fingerprint or background controls".into());
    }
    // A cached provider from the covered view must not be able to act either.
    let _ = client.invoke(&background);
    let after_background = command(commands, Command::Barrier)?;
    if after_background.challenge != Some(first_id)
        || !after_background.decisions.is_empty()
        || after_background.main_view != first.main_view
        || after_background.active_session != first.active_session
        || after_background.terminal_input != first.terminal_input
    {
        return Err("background action crossed the modal boundary".into());
    }

    for (changed, key, label, decision) in [
        (false, "host-key:reject", "拒绝", HostKeyDecision::Reject),
        (
            true,
            "host-key:reject-record",
            "拒绝并记住",
            HostKeyDecision::RejectAndRecord,
        ),
        (false, "host-key:once", "仅本次", HostKeyDecision::TrustOnce),
        (
            true,
            "host-key:trust",
            "更新并继续",
            HostKeyDecision::TrustAndRecord,
        ),
    ] {
        let installed = command(commands, |ack| Command::Host(changed, ack))?;
        let id = installed.challenge.ok_or("host challenge missing")?;
        let scope = format!(":{id}/host-key:dialog");
        client.wait("current host challenge", || client.by_id(&scope))?;
        if changed {
            client.named("⚠ 主机密钥已变更", "host-key:dialog")?;
            client.named(&format!("此前记录的指纹: {PREVIOUS}"), "host-key:previous")?;
            client.named(
                "密钥变更可能意味着中间人攻击。仅在你确知服务器更换过密钥时才接受。",
                "host-key:warning",
            )?;
        } else {
            client.named("信任并继续", "host-key:trust")?;
        }
        let button = client.named(label, key)?;
        if unsafe { button.CurrentControlType() }.map_err(|e| e.to_string())?
            != UIA_ButtonControlTypeId
        {
            return Err(format!("host action {key} is not a UIA button"));
        }
        unsafe { button.SetFocus() }.map_err(|e| format!("host button SetFocus failed: {e}"))?;
        client.wait("UIA focused the actual host button", || {
            unsafe { button.CurrentHasKeyboardFocus() }
                .ok()?
                .as_bool()
                .then_some(())
        })?;
        if command(commands, Command::Barrier)?.terminal_focused {
            return Err("UIA focus did not suspend remote keyboard input".into());
        }
        client.invoke(&button)?;
        let result = command(commands, Command::Barrier)?;
        if result.decisions.last() != Some(&(id, decision)) {
            return Err(format!("host action {key} did not reach its challenge"));
        }
        if !result.terminal_focused || result.active_session != first.active_session {
            return Err("host confirmation did not restore the original session focus".into());
        }
        client.wait("host dialog closed after decision", || {
            client.by_id("host-key:dialog").is_none().then_some(())
        })?;
    }

    // Post both keys without an intervening application update or UIA wait.
    let installed = command(commands, |ack| Command::Host(false, ack))?;
    let id = installed.challenge.ok_or("batched challenge missing")?;
    client.wait("batched host challenge", || {
        client.by_id(&format!(":{id}/host-key:dialog"))
    })?;
    let reject = client.named("拒绝", "host-key:reject")?;
    client.wait("batched challenge defaults to reject", || {
        unsafe { reject.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    post_key(hwnd, 0x09, 0x0f)?;
    post_key(hwnd, 0x20, 0x39)?;
    client.wait("batched Tab and Space select reject-and-record", || {
        let result = command(commands, Command::Barrier).ok()?;
        (result.decisions.last() == Some(&(id, HostKeyDecision::RejectAndRecord))).then_some(())
    })?;

    // Queue a real UIA SetFocus for Trust, but hold dispatch while actual iced
    // keyboard events move the same challenge back to Reject. The queued
    // request must not overtake that later choice when dispatch resumes.
    let installed = command(commands, |ack| Command::Host(false, ack))?;
    let id = installed.challenge.ok_or("stale-focus challenge missing")?;
    client.wait("stale-focus host challenge", || {
        client.by_id(&format!(":{id}/host-key:dialog"))
    })?;
    let trust = client.named("信任并继续", "host-key:trust")?;
    let reject = client.named("拒绝", "host-key:reject")?;
    let before = command(commands, Command::Hold)?;
    unsafe { trust.SetFocus() }.map_err(|e| format!("queuing UIA focus failed: {e}"))?;
    let queued = command(commands, Command::InspectHeld)?;
    if queued.focus_enqueued <= before.focus_enqueued {
        return Err("the actual UIA focus request was not enqueued".into());
    }
    for _ in 0..3 {
        post_key(hwnd, 0x09, 0x0f)?;
    }
    client.wait("batched Tab events reached Trust while UIA is held", || {
        unsafe { trust.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    post_key(hwnd, 0x09, 0x0f)?;
    client.wait("later Tab explicitly selects Reject", || {
        unsafe { reject.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    let after = command(commands, Command::Barrier)?;
    if after.stale_focus_rejected <= before.stale_focus_rejected {
        return Err("the queued UIA focus was not rejected as stale".into());
    }
    post_key(hwnd, 0x0d, 0x1c)?;
    client.wait("stale UIA focus cannot change the Enter decision", || {
        let result = command(commands, Command::Barrier).ok()?;
        (result.decisions.last() == Some(&(id, HostKeyDecision::Reject))).then_some(())
    })?;

    // Keep Trust applied, then queue Reject without draining it. An Enter
    // delivered to the real HWND while Reject is pending must do nothing.
    let installed = command(commands, |ack| Command::Host(false, ack))?;
    let id = installed
        .challenge
        .ok_or("pending-rejection challenge missing")?;
    client.wait("pending-rejection challenge", || {
        client.by_id(&format!(":{id}/host-key:dialog"))
    })?;
    let trust = client.named("信任并继续", "host-key:trust")?;
    let reject = client.named("拒绝", "host-key:reject")?;
    unsafe { trust.SetFocus() }.map_err(|e| format!("trust SetFocus failed: {e}"))?;
    client.wait("trust is applied before pending rejection", || {
        unsafe { trust.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    let before = command(commands, Command::Hold)?;
    unsafe { reject.SetFocus() }.map_err(|e| format!("reject SetFocus failed: {e}"))?;
    diagnostics::event(format_args!(
        "pending-reject begin challenge={id} decisions={:?}",
        before.decisions
    ));
    let queued = command(commands, Command::InspectHeld)?;
    if queued.focus_enqueued <= before.focus_enqueued {
        return Err("pending rejection was not actually queued".into());
    }
    post_key(hwnd, 0x0d, 0x1c)?;
    client.wait("Enter observed while the rejection is pending", || {
        (command(commands, Command::InspectHeld)
            .ok()?
            .keyboard_attempts
            > before.keyboard_attempts)
            .then_some(())
    })?;
    client.invoke(&trust)?;
    if command(commands, Command::InspectHeld)?.held_host_actions != before.held_host_actions {
        return Err("Enter activated Trust while a rejection was pending".into());
    }
    let after = command(commands, Command::Barrier)?;
    diagnostics::event(format_args!(
        "pending-reject barrier challenge={id} current={:?} decision_delta={:?}",
        after.challenge,
        after.decisions.get(before.decisions.len()..)
    ));
    if after.decisions != before.decisions {
        return Err(format!("UIA Invoke changed decisions while rejection was pending: challenge={id} current={:?} delta={:?}", after.challenge, after.decisions.get(before.decisions.len()..)));
    }
    client.wait("pending rejection applied", || {
        unsafe { reject.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    post_key(hwnd, 0x0d, 0x1c)?;
    client.wait("Enter after pending rejection rejects", || {
        let result = command(commands, Command::Barrier).ok()?;
        (result.decisions.last() == Some(&(id, HostKeyDecision::Reject))).then_some(())
    })?;

    // A real pending Trust Invoke must be superseded by the second refusal,
    // which is not the first button in the host-key dialog.
    let installed = command(commands, |ack| Command::Host(false, ack))?;
    let id = installed
        .challenge
        .ok_or("reject-and-record challenge missing")?;
    client.wait("reject-and-record challenge", || {
        client.by_id(&format!(":{id}/host-key:dialog"))
    })?;
    let trust = client.named("信任并继续", "host-key:trust")?;
    let refusal = client.named("拒绝并记住", "host-key:reject-record")?;
    let before = command(commands, Command::Hold)?;
    client.invoke(&trust)?;
    let trust_queued = command(commands, Command::InspectHeld)?;
    if trust_queued.invoke_enqueued <= before.invoke_enqueued {
        return Err("pending Trust Invoke was not enqueued".into());
    }
    client.invoke(&refusal)?;
    let refusal_queued = command(commands, Command::InspectHeld)?;
    if refusal_queued.invoke_enqueued <= trust_queued.invoke_enqueued {
        return Err("reject-and-record Invoke was not enqueued after Trust".into());
    }
    let result = command(commands, Command::Barrier)?;
    if result.decisions.get(before.decisions.len()..)
        != Some(&[(id, HostKeyDecision::RejectAndRecord)][..])
    {
        return Err(format!(
            "pending Trust / reject-and-record decision delta was {:?}",
            result.decisions.get(before.decisions.len()..)
        ));
    }

    // Hold the already-generated Enter message itself, then accept a newer
    // UIA Reject request before allowing that old message to commit.
    let installed = command(commands, |ack| Command::Host(false, ack))?;
    let id = installed
        .challenge
        .ok_or("queued-enter challenge missing")?;
    client.wait("queued-enter challenge", || {
        client.by_id(&format!(":{id}/host-key:dialog"))
    })?;
    let trust = client.named("信任并继续", "host-key:trust")?;
    let reject = client.named("拒绝", "host-key:reject")?;
    unsafe { trust.SetFocus() }.map_err(|e| format!("queued-enter trust focus failed: {e}"))?;
    client.wait("queued-enter trust applied", || {
        unsafe { trust.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    let before = command(commands, Command::Hold)?;
    post_key(hwnd, 0x0d, 0x1c)?;
    client.wait("old Enter reached application queue", || {
        (command(commands, Command::InspectHeld)
            .ok()?
            .held_host_actions
            > before.held_host_actions)
            .then_some(())
    })?;
    unsafe { reject.SetFocus() }.map_err(|e| format!("queued-enter reject focus failed: {e}"))?;
    let queued = command(commands, Command::InspectHeld)?;
    if queued.focus_enqueued <= before.focus_enqueued {
        return Err("new rejection was not enqueued before the old Enter".into());
    }
    let replayed = command(commands, Command::ReplayHeld)?;
    if replayed.decisions != before.decisions {
        return Err("old queued Enter committed after the new rejection".into());
    }
    client.wait("queued-enter rejection applied", || {
        unsafe { reject.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    post_key(hwnd, 0x0d, 0x1c)?;
    client.wait("queued-enter challenge rejects", || {
        let result = command(commands, Command::Barrier).ok()?;
        (result.decisions.last() == Some(&(id, HostKeyDecision::Reject))).then_some(())
    })?;

    let installed = command(commands, |ack| Command::Host(false, ack))?;
    let id = installed
        .challenge
        .ok_or("legacy-focus challenge missing")?;
    client.wait("legacy-focus challenge", || {
        client.by_id(&format!(":{id}/host-key:dialog"))
    })?;
    let reject = client.named("拒绝", "host-key:reject")?;
    client
        .wait("legacy-focus challenge defaults to reject", || {
            unsafe { reject.CurrentHasKeyboardFocus() }
                .ok()?
                .as_bool()
                .then_some(())
        })
        .map_err(|error| format!("{error}; {}", host_focus_diagnostics(client, hwnd)))?;
    command(commands, Command::LegacyFocus)?;
    std::thread::sleep(Duration::from_millis(100));
    client.wait("legacy global focus did not clear reject", || {
        unsafe { reject.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    post_key(hwnd, 0x0d, 0x1c)?;
    client.wait("Enter after old focus rejects", || {
        let result = command(commands, Command::Barrier).ok()?;
        (result.decisions.last() == Some(&(id, HostKeyDecision::Reject))).then_some(())
    })?;

    // Exercise the real HWND's keyboard path as well as UIA Invoke. Each new
    // challenge must begin at Reject, regardless of the previous selection.
    for (index, key, decision) in [
        (0, "host-key:reject", HostKeyDecision::Reject),
        (
            1,
            "host-key:reject-record",
            HostKeyDecision::RejectAndRecord,
        ),
        (2, "host-key:once", HostKeyDecision::TrustOnce),
        (3, "host-key:trust", HostKeyDecision::TrustAndRecord),
    ] {
        let installed = command(commands, |ack| Command::Host(false, ack))?;
        let id = installed.challenge.ok_or("keyboard challenge missing")?;
        client.wait("keyboard challenge drawn", || {
            client.by_id(&format!(":{id}/host-key:dialog"))
        })?;
        let refuse = client
            .by_id("host-key:reject")
            .ok_or("reject button missing")?;
        client
            .wait(&format!("new challenge {index} defaults to reject"), || {
                unsafe { refuse.CurrentHasKeyboardFocus() }
                    .ok()?
                    .as_bool()
                    .then_some(())
            })
            .map_err(|error| format!("{error}; {}", host_focus_diagnostics(client, hwnd)))?;
        for _ in 0..index {
            post_key(hwnd, 0x09, 0x0f)?; // Tab
            std::thread::sleep(Duration::from_millis(100));
        }
        let target = client.by_id(key).ok_or("keyboard target missing")?;
        client.wait("Tab reached the selected host action", || {
            unsafe { target.CurrentHasKeyboardFocus() }
                .ok()?
                .as_bool()
                .then_some(())
        })?;
        if index % 2 == 0 {
            post_key(hwnd, 0x0d, 0x1c)?; // Enter
        } else {
            post_key(hwnd, 0x20, 0x39)?; // Space
        }
        client.wait("keyboard action reached its challenge", || {
            let result = command(commands, Command::Barrier).ok()?;
            (result.decisions.last() == Some(&(id, decision))).then_some(())
        })?;
    }

    // Hold the queue so an old Invoke is definitely queued before replacement.
    let decision_count = command(commands, Command::Barrier)?.decisions.len();
    let old = command(commands, |ack| Command::Host(false, ack))?;
    let old_id = old.challenge.ok_or("old host challenge missing")?;
    client.wait("old host dialog", || {
        client.by_id(&format!(":{old_id}/host-key:dialog"))
    })?;
    let old_button = client.named("拒绝", "host-key:reject")?;
    command(commands, Command::Hold)?;
    client.invoke(&old_button)?;
    let replacement = command(commands, |ack| Command::Host(false, ack))?;
    let replacement_id = replacement
        .challenge
        .ok_or("replacement host challenge missing")?;
    if old_id == replacement_id || replacement.decisions.len() != decision_count {
        return Err("queued host action reached a replacement challenge".into());
    }
    client.wait("replacement host dialog", || {
        client.by_id(&format!(":{replacement_id}/host-key:dialog"))
    })?;
    let _ = client.invoke(&old_button);
    let after_stale = command(commands, Command::Barrier)?;
    if after_stale.decisions.len() != decision_count
        || after_stale.challenge != Some(replacement_id)
    {
        return Err("cached host action reached a replacement challenge".into());
    }
    client.invoke(&client.named("拒绝", "host-key:reject")?)?;
    let result = command(commands, Command::Barrier)?;
    if result.decisions.last() != Some(&(replacement_id, HostKeyDecision::Reject)) {
        return Err("replacement host challenge was not actionable".into());
    }
    client.wait("host dialog closed", || {
        client.by_id("host-key:dialog").is_none().then_some(())
    })?;

    command(commands, Command::Link)?;
    let link_dialog = client.named("打开链接？", "hyperlink:dialog")?;
    client.assert_modal_dialog(&link_dialog)?;
    client.named(LINK, "hyperlink:destination")?;
    client.named(
        "此链接来自终端输出，请确认目标地址后再打开：",
        "hyperlink:warning",
    )?;
    if client.by_id("navigation:Missions").is_some() {
        return Err("link dialog exposed background controls".into());
    }
    let cancel = client.named("取消", "hyperlink:cancel")?;
    client.wait("link confirmation defaults to cancel", || {
        unsafe { cancel.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    let old_link = client.named("打开", "hyperlink:open")?;
    post_key(hwnd, 0x09, 0x0f)?;
    client.wait("Tab reaches the link open button", || {
        unsafe { old_link.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    command(commands, Command::Hold)?;
    client.invoke(&old_link)?;
    // Even an identical URL is a different confirmation, not a renewed lease.
    let replacement = command(commands, Command::Link)?;
    if !replacement.link_pending || !replacement.opened_links.is_empty() {
        return Err("queued link action opened a replacement confirmation".into());
    }
    client.wait("replacement link dialog", || {
        let element = client.by_id("hyperlink:open")?;
        let old = unsafe { old_link.CurrentAutomationId() }.ok();
        let new = unsafe { element.CurrentAutomationId() }.ok();
        (old.is_none() || old != new).then_some(element)
    })?;
    let _ = client.invoke(&old_link);
    let after_stale = command(commands, Command::Barrier)?;
    if !after_stale.link_pending || !after_stale.opened_links.is_empty() {
        return Err("cached link action opened a replacement confirmation".into());
    }
    client.invoke(&client.named("取消", "hyperlink:cancel")?)?;
    let cancelled = command(commands, Command::Barrier)?;
    if cancelled.link_pending || !cancelled.opened_links.is_empty() {
        return Err("link cancellation did not close without opening".into());
    }
    client.wait("cancelled link dialog closed", || {
        client.by_id("hyperlink:dialog").is_none().then_some(())
    })?;
    command(commands, Command::Link)?;
    client.named(LINK, "hyperlink:destination")?;
    client.invoke(&client.named("打开", "hyperlink:open")?)?;
    let opened = command(commands, Command::Barrier)?;
    if opened.link_pending || opened.opened_links != [LINK] {
        return Err("link confirmation did not reach the isolated open sink".into());
    }

    command(commands, Command::Menu)?;
    let installed = command(commands, |ack| Command::Host(false, ack))?;
    let id = installed
        .challenge
        .ok_or("menu-covered confirmation missing")?;
    let reject = client.named("拒绝", "host-key:reject")?;
    let before_menu_invoke = command(commands, Command::Hold)?;
    unsafe { reject.SetFocus() }.map_err(|e| format!("UIA focus for menu test failed: {e}"))?;
    client.wait("menu test focused Reject", || {
        unsafe { reject.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    client.invoke(&reject)?;
    let queued_menu_invoke = command(commands, Command::InspectHeld)?;
    if queued_menu_invoke.focus_enqueued <= before_menu_invoke.focus_enqueued
        || queued_menu_invoke.invoke_enqueued <= before_menu_invoke.invoke_enqueued
    {
        return Err("menu test did not queue both UIA Focus and Invoke".into());
    }
    command(commands, Command::Barrier)?;
    client
        .wait("menu keeps remote focus suspended", || {
            let state = command(commands, Command::Barrier).ok()?;
            (state.decisions.last() == Some(&(id, HostKeyDecision::Reject))
                && state.active_menu == Some(MenuKind::File)
                && !state.terminal_focused)
                .then_some(())
        })
        .map_err(|error| {
            let state = command(commands, Command::Barrier).ok();
            format!(
                "{error}; challenge={id} latest_decision={:?} menu={:?} terminal_focused={:?}",
                state.as_ref().and_then(|value| value.decisions.last()),
                state.as_ref().and_then(|value| value.active_menu),
                state.as_ref().map(|value| value.terminal_focused)
            )
        })?;
    post_key(hwnd, 0x1b, 0x01)?;
    client.wait("remote focus returns after the menu closes", || {
        let state = command(commands, Command::Barrier).ok()?;
        (state.active_menu.is_none() && state.terminal_focused).then_some(())
    })?;

    let connection = command(commands, Command::Connection)?;
    if !connection.connection_open || connection.terminal_focused {
        return Err("synthetic password dialog did not suspend remote input".into());
    }
    let password = client.wait("connection password input", || {
        client.by_id("protected:Password / passphrase")
    })?;
    unsafe { password.SetFocus() }
        .map_err(|e| format!("connection password UIA focus failed: {e}"))?;
    client.wait("UIA focused connection password", || {
        unsafe { password.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    let cancelled = command(commands, Command::CancelConnection)?;
    if cancelled.connection_open || !cancelled.terminal_focused {
        return Err("connection cancellation did not restore the pre-dialog focus".into());
    }

    let rename = command(commands, Command::Rename)?;
    if !rename.mission_name_editing {
        return Err("fixture did not start the synthetic mission rename".into());
    }
    command(commands, Command::Menu)?;
    client.wait("menu covers the mission rename", || {
        let state = command(commands, Command::Barrier).ok()?;
        (state.active_menu == Some(MenuKind::File) && client.by_id("navigation:Missions").is_none())
            .then_some(())
    })?;
    post_escape_edge(hwnd, false, false)?;
    client.wait("Escape dismisses menu without cancelling rename", || {
        let state = command(commands, Command::Barrier).ok()?;
        (state.active_menu.is_none() && state.mission_name_editing).then_some(())
    })?;
    for _ in 0..3 {
        post_escape_edge(hwnd, true, false)?;
    }
    post_escape_edge(hwnd, false, true)?;
    let after_repeat = command(commands, Command::Barrier)?;
    if !after_repeat.mission_name_editing {
        return Err("repeated Escape after menu dismissal cancelled the rename".into());
    }
    command(commands, Command::Link)?;
    client.named("打开链接？", "hyperlink:dialog")?;
    post_key(hwnd, 0x1b, 0x01)?;
    std::thread::sleep(Duration::from_millis(150));
    let covered_rename = command(commands, Command::Barrier)?;
    if !covered_rename.link_pending || !covered_rename.mission_name_editing {
        return Err("modal Escape changed the covered mission rename".into());
    }
    client.invoke(&client.named("取消", "hyperlink:cancel")?)?;
    let after_cancel = command(commands, Command::Barrier)?;
    if after_cancel.link_pending || !after_cancel.mission_name_editing {
        return Err("link cancellation did not preserve the mission rename".into());
    }
    let hidden = command(commands, Command::ToggleSidebar)?;
    if hidden.sidebar_visible || !hidden.mission_name_editing {
        return Err("hiding the mission sidebar did not preserve its draft".into());
    }
    post_key(hwnd, 0x1b, 0x01)?;
    client.wait("Escape cancels a hidden mission rename", || {
        let state = command(commands, Command::Barrier).ok()?;
        (!state.mission_name_editing).then_some(())
    })?;
    Ok(())
}

pub(super) fn run(
    hwnd: isize,
    owners: &[SessionId],
    commands: mpsc::Sender<Command>,
) -> Result<Value, String> {
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
        .map_err(|e| format!("UIA COM initialization failed: {e}"))?;
    let _com = Com;
    let automation: IUIAutomation =
        unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
            .map_err(|e| format!("UIA client creation failed: {e}"))?;
    let root = unsafe { automation.ElementFromHandle(HWND(hwnd as *mut std::ffi::c_void)) }
        .map_err(|e| format!("UIA cannot read the synthetic HWND: {e}"))?;
    let client = Client { automation, root };
    let hosts = client.named("资源库", "navigation:Hosts")?;
    if unsafe { hosts.CurrentControlType() }.map_err(|e| e.to_string())? != UIA_ButtonControlTypeId
    {
        return Err("navigation does not expose the actual button role".into());
    }
    let before_edit = unsafe { client.named("编辑", "resource:edit")?.CurrentAutomationId() }
        .map_err(|e| e.to_string())?
        .to_string();
    let resource = client.named("SYNTHETIC node-2", "resource:host_card")?;
    if client.value(&resource).as_deref() != Some("fixture.invalid") {
        return Err("resource entry does not expose the actual host value".into());
    }
    client.invoke(&resource)?;
    let edit = client.wait("selected resource's edit operation", || {
        let element = client.find("编辑", "resource:edit")?;
        (unsafe { element.CurrentAutomationId() }.ok()? != before_edit).then_some(element)
    })?;
    client.invoke(&edit)?;
    let name = client.named("名称", "field:名称")?;
    client.wait("editor loaded the invoked profile", || {
        (client.value(&name).as_deref() == Some("SYNTHETIC node-2")).then_some(())
    })?;
    let password = client.named("连接密码", "protected:连接密码")?;
    if !unsafe { password.CurrentIsPassword() }
        .map_err(|e| e.to_string())?
        .as_bool()
    {
        return Err("protected input has no UIA password role".into());
    }
    let protected = client.privacy()?;
    for element in client.elements() {
        if unsafe { element.CurrentIsPassword() }.is_ok_and(|v| v.as_bool()) {
            let _ = client.set_value(&element, "MUST-NOT-BE-SET");
        }
    }
    unsafe { name.SetFocus() }.map_err(|e| format!("UIA focus action failed: {e}"))?;
    client.wait("UIA SetFocus reached the actual text input", || {
        unsafe { name.CurrentHasKeyboardFocus() }
            .ok()?
            .as_bool()
            .then_some(())
    })?;
    client.set_value(&name, "SYNTHETIC renamed")?;
    let host = client.named("主机", "field:主机")?;
    client.set_value(&host, "edited.invalid")?;
    client.wait("SetValue updated the rendered name and host", || {
        (client.value(&name).as_deref() == Some("SYNTHETIC renamed")
            && client.value(&host).as_deref() == Some("edited.invalid"))
        .then_some(())
    })?;
    client.privacy()?;
    client.invoke(&client.named("保存", "editor:save")?)?;
    client.named("SYNTHETIC renamed", "resource:host_card")?;

    client.invoke(&client.named("专注工作区", "navigation:Terminal")?)?;
    client.wait("first real terminal document", || {
        client.document(owners[0])
    })?;
    client.invoke(&client.named("SYNTHETIC node-0", &format!("session:{}", owners[0]))?)?;
    client.wait(
        "session item Invoke changed the actual active session",
        || {
            let item = client.by_id(&format!("/session:{}", owners[0]))?;
            (client.value(&item).as_deref() == Some("当前会话")).then_some(())
        },
    )?;
    client.invoke(&client.named("任务分屏", "navigation:Missions")?)?;
    let first = client.wait("first mission document", || client.document(owners[0]))?;
    let second = client.wait("second mission document", || client.document(owners[1]))?;
    let first_before = client.wait("first document text", || client.text(&first))?;
    let second_before = client.wait("second document text", || client.text(&second))?;
    if !second_before.contains("REMOTE OUTPUT IS DATA") {
        return Err("TextPattern did not expose the real terminal snapshot".into());
    }
    let range = client.wait("second session's scrollbar", || {
        client.by_id(&format!("/terminal-scroll:{}", owners[1]))
    })?;
    let range: IUIAutomationRangeValuePattern =
        unsafe { range.GetCurrentPatternAs(UIA_RangeValuePatternId) }.map_err(|e| e.to_string())?;
    let max = unsafe { range.CurrentMaximum() }.map_err(|e| e.to_string())?;
    if max <= 0.0 {
        return Err("fixture did not create actual scrollback".into());
    }
    if unsafe { range.CurrentIsReadOnly() }
        .map_err(|e| e.to_string())?
        .as_bool()
    {
        return Err("the real terminal scrollbar is incorrectly read-only".into());
    }
    unsafe { range.SetValue(max) }
        .map_err(|e| format!("UIA terminal scroll SetValue failed: {e}"))?;
    client.wait(
        "scroll action changed only its own terminal snapshot",
        || {
            let a = client.text(&first)?;
            let b = client.text(&second)?;
            (a == first_before && b != second_before).then_some(())
        },
    )?;
    dialog_acceptance(&client, &commands, hwnd)?;
    let focus_statistics = command(&commands, Command::Barrier)?;
    Ok(json!({
        "hwnd": hwnd, "native_uia": true, "production_iced_widgets": true,
        "navigation_invoke": true, "resource_invoke_and_value": true,
        "editor_set_value_and_save_invoke": true, "session_invoke": true,
        "editor_explicit_focus": true,
        "protected_controls_seen": protected, "protected_readback": "masked-or-denied",
        "terminal_text_pattern": true, "terminal_scroll_owner_isolation": true,
        "remote_text_never_executed": true,
        "host_key_uia_details_and_four_actions": true,
        "link_uia_destination_cancel_and_open": true,
        "dialog_modal_isolation": true,
        "host_key_keyboard_tab_enter_space_and_safe_default": true,
        "batched_keyboard_and_legacy_focus_safe": true,
        "same_challenge_queued_uia_focus_cannot_override_keyboard": true,
        "pending_uia_rejection_blocks_enter_on_old_trust": true,
        "pending_uia_rejection_blocks_invoke_on_old_trust": true,
        "reject_and_record_supersedes_pending_uia_trust": true,
        "queued_enter_cannot_commit_after_new_uia_rejection": true,
        "confirmation_uia_focus_requests_enqueued": focus_statistics.focus_enqueued,
        "stale_confirmation_uia_focus_rejected": focus_statistics.stale_focus_rejected,
        "uia_dialog_focus_restores_original_session": true,
        "menu_defers_remote_focus_restore": true,
        "connection_password_cancel_restores_remote_focus": true,
        "menu_escape_and_covered_mission_rename": true,
        "escape_repeats_preserve_rename": true,
        "hidden_mission_rename_can_be_cancelled": true,
        "link_keyboard_tab_and_safe_default": true,
        "queued_and_cached_stale_dialog_actions_rejected": true,
        "links_opened_only_in_fixture_sink": true,
    }))
}
