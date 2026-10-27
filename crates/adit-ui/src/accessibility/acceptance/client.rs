//! Native UIA client. It runs on an MTA worker while iced pumps its real HWND.
use super::*;
use windows::{
    core::BSTR,
    Win32::{
        Foundation::HWND,
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_MULTITHREADED,
        },
        UI::Accessibility::*,
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

pub(super) fn run(hwnd: isize, owners: &[SessionId]) -> Result<Value, String> {
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
    Ok(json!({
        "hwnd": hwnd, "native_uia": true, "production_iced_widgets": true,
        "navigation_invoke": true, "resource_invoke_and_value": true,
        "editor_set_value_and_save_invoke": true, "session_invoke": true,
        "editor_explicit_focus": true,
        "protected_controls_seen": protected, "protected_readback": "masked-or-denied",
        "terminal_text_pattern": true, "terminal_scroll_owner_isolation": true,
        "remote_text_never_executed": true,
    }))
}
