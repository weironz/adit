use super::*;
use iced::widget::column;

pub(crate) fn sftp_panel_overlay(app: &AditApp) -> Element<'_, Message> {
    let Some(browser) = app.manager.sftp_browser() else {
        return Space::new().width(Fill).height(Fill).into();
    };

    // While dragging, the status line becomes a prominent drag hint.
    let (status_text, status_color) = match &app.sftp_drag {
        Some((src, name)) => {
            let count = match src {
                SftpPane::Local => app.sftp_local_selected.len(),
                SftpPane::Remote => app.sftp_remote_selected.len(),
            };
            let selected = match src {
                SftpPane::Local => app.sftp_local_selected.contains(name),
                SftpPane::Remote => app.sftp_remote_selected.contains(name),
            };
            let subject = if selected && count > 1 {
                format!("{count} 项")
            } else {
                format!("«{name}»")
            };
            let target = match src {
                SftpPane::Local => "松开到右侧远程栏上传",
                SftpPane::Remote => "松开到左侧本地栏下载",
            };
            (format!("⠿ 拖拽 {subject} — {target}"), accent())
        }
        None if browser.status.starts_with("error") => (browser.status.clone(), danger()),
        None => (browser.status.clone(), muted_text()),
    };

    let header = row![
        text(format!("SFTP — {}", browser.endpoint))
            .size(15)
            .color(primary_text()),
        Space::new().width(Fill),
        text(status_text).size(11).color(status_color),
        Space::new().width(Length::Fixed(12.0)),
        button("×")
            .width(Length::Fixed(26.0))
            .height(Length::Fixed(24.0))
            .padding(0)
            .style(|_theme, status| close_button_style(status))
            .on_press(Message::CloseSftp),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let panes = row![sftp_local_pane(app, browser), sftp_remote_pane(app, browser)]
        .spacing(10)
        .height(Fill);

    let mut panel_body = column![header].spacing(10);
    if let Some((_, from)) = &app.sftp_rename {
        panel_body = panel_body.push(sftp_rename_bar(from, &app.sftp_rename_to));
    }
    if let Some((_, name, _)) = &app.sftp_delete_target {
        panel_body = panel_body.push(sftp_delete_bar(name));
    }

    // Extra upload via picker / typed path → remote current directory.
    let upload_extra = row![
        button(text("选择文件上传…").size(12))
            .padding([5, 12])
            .style(|_theme, status| primary_button_style(status))
            .on_press(Message::SftpPickUpload),
        text_input("或输入本地路径上传到远程当前目录", &app.sftp_upload_path)
            .on_input(Message::SftpUploadPathChanged)
            .on_submit(Message::SftpUpload)
            .padding([5, 8])
            .style(text_input_style)
            .width(Fill),
        button(text("上传").size(12))
            .padding([5, 12])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::SftpUpload),
    ]
    .spacing(8)
    .align_y(Alignment::Center);

    let panel_body = panel_body
        .push(panes)
        .push(upload_extra)
        .push(sftp_transfer_queue(browser));

    let panel = container(panel_body)
        .width(Fill)
        .height(Fill)
        .padding(14)
        .style(|_theme| connection_dialog_style());

    // Track the cursor over the (full-window) panel so a right-click can anchor
    // its floating actions menu at the pointer.
    mouse_area(
        container(panel)
            .width(Fill)
            .height(Fill)
            .padding(20)
            .style(|_theme| dialog_scrim_style()),
    )
    .on_move(Message::SftpCursorMoved)
    .into()
}

pub(crate) fn sftp_local_pane<'a>(app: &'a AditApp, browser: &'a SftpBrowser) -> Element<'a, Message> {
    let header = row![
        text("本地").size(13).color(primary_text()),
        button(text("↑").size(12))
            .padding([3, 9])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::SftpLocalUp),
        button(text("⟳").size(12))
            .padding([3, 9])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::SftpLocalRefresh),
        text_input("本地路径（回车跳转）", &app.sftp_local_path_edit)
            .on_input(Message::SftpLocalPathChanged)
            .on_submit(Message::SftpLocalGo)
            .padding([3, 6])
            .style(toolbar_input_style)
            .width(Fill),
        sftp_batch_button(
            "上传选中",
            app.sftp_local_selected.len(),
            Message::SftpTransferSelected(SftpPane::Local),
        ),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let (key, ascending) = app.sftp_local_sort;
    let mut items: Vec<&LocalEntry> = browser.local_entries.iter().collect();
    items.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            sftp_cmp(
                key,
                ascending,
                (&a.name, a.size, a.mtime),
                (&b.name, b.size, b.mtime),
            )
        })
    });

    let mut list = column![sftp_nav_row("../", Message::SftpLocalUp)].spacing(1);
    for entry in items {
        let selected = app.sftp_local_selected.contains(&entry.name);
        list = list.push(sftp_local_entry_row(entry, selected));
    }

    let drop_active = app.sftp_drag.as_ref().map(|(p, _)| *p) == Some(SftpPane::Remote)
        && app.sftp_drag_over == Some(SftpPane::Local);

    let pane = container(
        column![
            header,
            sftp_sort_header(SftpPane::Local, app.sftp_local_sort),
            container(scrollable(list).height(Fill))
                .height(Fill)
                .padding(3)
                .style(|_theme| sftp_list_inner_style()),
        ]
        .spacing(6),
    )
    .width(Length::FillPortion(1))
    .height(Fill)
    .padding(8)
    .style(move |_theme| sftp_pane_style_dropzone(drop_active));

    sftp_drag_layer(app, SftpPane::Local, pane.into())
}

pub(crate) fn sftp_remote_pane<'a>(app: &'a AditApp, browser: &'a SftpBrowser) -> Element<'a, Message> {
    let header = row![
        text("远程").size(13).color(primary_text()),
        button(text("↑").size(12))
            .padding([3, 9])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::SftpUp),
        button(text("⟳").size(12))
            .padding([3, 9])
            .style(|_theme, status| secondary_button_style(status))
            .on_press(Message::SftpRefresh),
        text_input("远程路径（回车跳转）", &app.sftp_remote_path_edit)
            .on_input(Message::SftpRemotePathChanged)
            .on_submit(Message::SftpRemoteGo)
            .padding([3, 6])
            .style(toolbar_input_style)
            .width(Fill),
        sftp_batch_button(
            "下载选中",
            app.sftp_remote_selected.len(),
            Message::SftpTransferSelected(SftpPane::Remote),
        ),
    ]
    .spacing(6)
    .align_y(Alignment::Center);

    let (key, ascending) = app.sftp_remote_sort;
    let mut items: Vec<&SftpEntry> = browser.entries.iter().collect();
    items.sort_by(|a, b| {
        b.is_dir.cmp(&a.is_dir).then_with(|| {
            sftp_cmp(
                key,
                ascending,
                (&a.name, a.size, a.mtime.map(u64::from)),
                (&b.name, b.size, b.mtime.map(u64::from)),
            )
        })
    });

    let mut content = column![header, sftp_sort_header(SftpPane::Remote, app.sftp_remote_sort)]
        .spacing(6);

    let mut list = column![sftp_nav_row("../", Message::SftpUp)].spacing(1);
    for entry in items {
        let selected = app.sftp_remote_selected.contains(&entry.name);
        list = list.push(sftp_remote_entry_row(entry, selected));
    }
    content = content.push(
        container(scrollable(list).height(Fill))
            .height(Fill)
            .padding(3)
            .style(|_theme| sftp_list_inner_style()),
    );

    content = content.push(
        row![
            text_input("新文件夹名", &app.sftp_new_folder)
                .on_input(Message::SftpNewFolderChanged)
                .on_submit(Message::SftpMkdir)
                .padding([4, 8])
                .style(text_input_style)
                .width(Fill),
            button(text("新建").size(11))
                .padding([4, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::SftpMkdir),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    );

    let drop_active = app.sftp_drag.as_ref().map(|(p, _)| *p) == Some(SftpPane::Local)
        && app.sftp_drag_over == Some(SftpPane::Remote);

    let pane = container(content)
        .width(Length::FillPortion(1))
        .height(Fill)
        .padding(8)
        .style(move |_theme| sftp_pane_style_dropzone(drop_active));

    sftp_drag_layer(app, SftpPane::Remote, pane.into())
}

/// Rename bar shown at the panel level for whichever pane is being edited.
pub(crate) fn sftp_rename_bar<'a>(from: &str, rename_to: &'a str) -> Element<'a, Message> {
    container(
        row![
            text(format!("重命名 {from} →"))
                .size(12)
                .color(primary_text()),
            text_input("新名称", rename_to)
                .on_input(Message::SftpRenameToChanged)
                .on_submit(Message::SftpConfirmRename)
                .padding([4, 8])
                .style(text_input_style)
                .width(Fill),
            button(text("确定").size(11))
                .padding([4, 10])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::SftpConfirmRename),
            button(text("取消").size(11))
                .padding([4, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::SftpCancelRename),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding(6)
    .style(|_theme| profile_edit_menu_style())
    .into()
}

/// Delete-confirmation bar shown at the panel level for whichever pane is being edited.
pub(crate) fn sftp_delete_bar(name: &str) -> Element<'static, Message> {
    container(
        row![
            text(format!("确认删除 {name}?"))
                .size(12)
                .color(danger())
                .width(Fill),
            button(text("删除").size(11))
                .padding([4, 10])
                .style(|_theme, status| primary_button_style(status))
                .on_press(Message::SftpConfirmDelete),
            button(text("取消").size(11))
                .padding([4, 10])
                .style(|_theme, status| secondary_button_style(status))
                .on_press(Message::SftpCancelDelete),
        ]
        .spacing(6)
        .align_y(Alignment::Center),
    )
    .padding(6)
    .style(|_theme| error_panel_style())
    .into()
}

pub(crate) fn sftp_transfer_queue(browser: &SftpBrowser) -> Element<'static, Message> {
    let mut done = 0usize;
    let mut failed = 0usize;
    let mut active = 0usize;
    let mut cancelled = 0usize;
    for item in &browser.transfers {
        match item.status {
            TransferStatus::Done => done += 1,
            TransferStatus::Failed => failed += 1,
            TransferStatus::Cancelled => cancelled += 1,
            TransferStatus::Pending | TransferStatus::Active => active += 1,
        }
    }

    let mut clear = button(text("清空已完成").size(11))
        .padding([3, 10])
        .style(|_theme, status| secondary_button_style(status));
    if done + failed + cancelled > 0 {
        clear = clear.on_press(Message::SftpClearTransfers);
    }

    // "Stop all" only appears while something is actually running or queued.
    let mut controls = row![].spacing(6).align_y(Alignment::Center);
    if active > 0 {
        controls = controls.push(
            button(text("全部停止").size(11))
                .padding([3, 10])
                .style(|_theme, status| danger_button_style(status))
                .on_press(Message::SftpCancelAll),
        );
    }
    controls = controls.push(clear);

    let title = row![
        text("传输队列").size(11).color(primary_text()),
        text(format!("完成 {done} · 失败 {failed} · 停止 {cancelled} · 进行 {active}"))
            .size(10)
            .color(muted_text()),
        Space::new().width(Fill),
        controls,
    ]
    .spacing(10)
    .align_y(Alignment::Center);

    let column_header = row![
        text("文件").size(10).color(muted_text()).width(Length::FillPortion(2)),
        text("目标路径").size(10).color(muted_text()).width(Length::FillPortion(3)),
        text("大小").size(10).color(muted_text()).width(Length::Fixed(72.0)),
        text("进度").size(10).color(muted_text()).width(Length::Fixed(112.0)),
        text("速度").size(10).color(muted_text()).width(Length::Fixed(78.0)),
        text("状态").size(10).color(muted_text()).width(Length::Fixed(48.0)),
        Space::new().width(Length::Fixed(44.0)),
    ]
    .spacing(8);

    let body: Element<'static, Message> = if browser.transfers.is_empty() {
        text("（暂无传输）").size(11).color(muted_text()).into()
    } else {
        let mut list = column![].spacing(1);
        for item in browser.transfers.iter().rev() {
            list = list.push(sftp_transfer_row(item));
        }
        scrollable(list).height(Length::Fixed(108.0)).into()
    };

    container(column![title, column_header, body].spacing(4))
        .width(Fill)
        .padding(8)
        .style(|_theme| sftp_pane_style())
        .into()
}

pub(crate) fn sftp_transfer_row(item: &TransferItem) -> Element<'static, Message> {
    let arrow = match item.direction {
        TransferDirection::Upload => "↑",
        TransferDirection::Download => "↓",
    };
    let (label, color) = match item.status {
        TransferStatus::Pending => ("排队", muted_text()),
        TransferStatus::Active => ("传输中", accent()),
        TransferStatus::Done => ("完成", Color::from_rgb8(34, 197, 94)),
        TransferStatus::Failed => ("失败", danger()),
        TransferStatus::Cancelled => ("已停止", muted_text()),
    };
    let stoppable = matches!(item.status, TransferStatus::Pending | TransferStatus::Active);
    // A completed transfer is always 100% — including 0-byte files, where
    // dividing by total would otherwise leave it at 0%.
    let (fraction, pct) = if matches!(item.status, TransferStatus::Done) {
        (1.0, 100)
    } else if item.total > 0 {
        (
            (item.done as f32 / item.total as f32).clamp(0.0, 1.0),
            item.done.saturating_mul(100).checked_div(item.total).unwrap_or(0),
        )
    } else {
        (0.0, 0)
    };
    let speed = if item.bps > 0 {
        format!("{}/s", human_size(item.bps))
    } else {
        String::from("—")
    };
    // On failure, show the reason in place of the destination so it's visible.
    let (detail, detail_color) = match (&item.status, &item.error) {
        (TransferStatus::Failed, Some(reason)) => (reason.clone(), danger()),
        _ => (item.dest.clone(), muted_text()),
    };

    let progress = row![
        progress_bar(0.0..=1.0, fraction)
            .length(Length::Fixed(70.0))
            .girth(Length::Fixed(6.0)),
        text(format!("{pct}%"))
            .size(9)
            .color(muted_text())
            .width(Length::Fixed(34.0)),
    ]
    .spacing(4)
    .align_y(Alignment::Center);

    // Only a running/queued transfer can be stopped; a finished row shows a
    // blank of the same width so every row's columns stay aligned.
    let action: Element<'static, Message> = if stoppable {
        button(text("停止").size(9))
            .padding([1, 6])
            .style(|_theme, status| danger_button_style(status))
            .on_press(Message::SftpCancelTransfer(item.id))
            .into()
    } else {
        Space::new().width(Length::Fixed(44.0)).into()
    };

    row![
        row![
            text(arrow).size(10).color(muted_text()),
            text(item.name.clone()).size(10).color(primary_text()),
        ]
        .spacing(4)
        .width(Length::FillPortion(2)),
        text(detail)
            .size(10)
            .color(detail_color)
            .width(Length::FillPortion(3)),
        text(human_size(item.total))
            .size(10)
            .color(muted_text())
            .width(Length::Fixed(72.0)),
        container(progress).width(Length::Fixed(112.0)),
        text(speed).size(10).color(muted_text()).width(Length::Fixed(78.0)),
        text(label).size(10).color(color).width(Length::Fixed(48.0)),
        container(action).width(Length::Fixed(44.0)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

pub(crate) fn sftp_nav_row(label: &'static str, message: Message) -> Element<'static, Message> {
    button(text(label).size(12).color(accent()))
        .width(Fill)
        .padding([4, 8])
        .style(|_theme, status| sftp_entry_button_style(status))
        .on_press(message)
        .into()
}

pub(crate) fn sftp_local_entry_row(entry: &LocalEntry, selected: bool) -> Element<'static, Message> {
    let owned = entry.name.clone();
    let is_dir = entry.is_dir;
    // Right-click anywhere on the row opens the actions menu (SecureFX-style).
    let context = Message::ShowSftpContextMenu(SftpPane::Local, owned.clone(), is_dir);
    if is_dir {
        // A folder: left-click navigates in, right-click opens the menu.
        return mouse_area(
            container(
                row![
                    text(format!("{}/", entry.name))
                        .size(12)
                        .color(accent())
                        .width(Fill),
                    text("DIR").size(10).color(muted_text()).width(Length::Fixed(64.0)),
                    text(sftp_date(entry.mtime))
                        .size(10)
                        .color(muted_text())
                        .width(Length::Fixed(118.0)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([4, 8])
            .style(move |_theme| sftp_row_highlight(selected)),
        )
        .on_press(Message::SftpLocalNavigate(owned))
        .on_right_press(context)
        .interaction(mouse::Interaction::Pointer)
        .into();
    }

    // File: click to select, double-click to upload, right-click for the menu.
    mouse_area(
        container(
            row![
                text(entry.name.clone())
                    .size(12)
                    .color(primary_text())
                    .width(Fill),
                text(human_size(entry.size))
                    .size(10)
                    .color(muted_text())
                    .width(Length::Fixed(64.0)),
                text(sftp_date(entry.mtime))
                    .size(10)
                    .color(muted_text())
                    .width(Length::Fixed(118.0)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .padding([4, 8])
        .style(move |_theme| sftp_row_highlight(selected)),
    )
    .on_press(Message::SftpRowPress(SftpPane::Local, owned))
    .on_right_press(context)
    .into()
}

pub(crate) fn sftp_remote_entry_row(entry: &SftpEntry, selected: bool) -> Element<'static, Message> {
    let owned = entry.name.clone();
    let is_dir = entry.is_dir;
    let context = Message::ShowSftpContextMenu(SftpPane::Remote, owned.clone(), is_dir);
    if is_dir {
        // A folder: left-click navigates in, right-click opens the menu.
        return mouse_area(
            container(
                row![
                    text(format!("{}/", entry.name))
                        .size(12)
                        .color(accent())
                        .width(Fill),
                    text("DIR").size(10).color(muted_text()).width(Length::Fixed(64.0)),
                    text(sftp_date(entry.mtime.map(u64::from)))
                        .size(10)
                        .color(muted_text())
                        .width(Length::Fixed(118.0)),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
            )
            .width(Fill)
            .padding([4, 8])
            .style(move |_theme| sftp_row_highlight(selected)),
        )
        .on_press(Message::SftpNavigate(owned))
        .on_right_press(context)
        .interaction(mouse::Interaction::Pointer)
        .into();
    }

    // File: click to select, double-click to download, right-click for the menu.
    mouse_area(
        container(
            row![
                text(entry.name.clone())
                    .size(12)
                    .color(primary_text())
                    .width(Fill),
                text(human_size(entry.size))
                    .size(10)
                    .color(muted_text())
                    .width(Length::Fixed(64.0)),
                text(sftp_date(entry.mtime.map(u64::from)))
                    .size(10)
                    .color(muted_text())
                    .width(Length::Fixed(118.0)),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        )
        .width(Fill)
        .padding([4, 8])
        .style(move |_theme| sftp_row_highlight(selected)),
    )
    .on_press(Message::SftpRowPress(SftpPane::Remote, owned))
    .on_right_press(context)
    .into()
}

pub(crate) fn sftp_row_highlight(selected: bool) -> container::Style {
    container::Style {
        background: Some(Background::Color(if selected {
            accent_soft()
        } else {
            transparent()
        })),
        ..container::Style::default()
    }
}

/// The floating right-click actions menu for one SFTP pane entry (SecureFX-style).
pub(crate) fn sftp_context_menu_card(
    pane: SftpPane,
    name: String,
    is_dir: bool,
) -> Element<'static, Message> {
    let mut items = column![].spacing(1);
    if is_dir {
        // Open the folder (navigate into it).
        let open = match pane {
            SftpPane::Local => Message::SftpLocalNavigate(name.clone()),
            SftpPane::Remote => Message::SftpNavigate(name.clone()),
        };
        items = items.push(profile_menu_item("打开", open, false));
    }
    // Transfer to the other pane. Folder transfer is recursive.
    let (transfer_label, transfer_msg) = match pane {
        SftpPane::Remote => ("下载 ↓", Message::SftpDownload(name.clone())),
        SftpPane::Local => ("上传 ↑", Message::SftpUploadLocal(name.clone())),
    };
    items = items
        .push(profile_menu_item(transfer_label, transfer_msg, false))
        .push(profile_menu_item(
            "重命名",
            Message::SftpBeginRename(pane, name.clone()),
            false,
        ))
        .push(profile_menu_divider())
        .push(profile_menu_item(
            "删除",
            Message::SftpBeginDelete(pane, name, is_dir),
            true,
        ));

    container(items)
        .padding(4)
        .width(Length::Fixed(PROFILE_MENU_WIDTH))
        .style(|_theme| profile_context_menu_style())
        .into()
}

pub(crate) fn sftp_context_overlay(
    app: &AditApp,
    pane: SftpPane,
    name: String,
    is_dir: bool,
) -> Element<'_, Message> {
    floating_context_menu(
        app,
        sftp_context_menu_card(pane, name, is_dir),
        Message::HideSftpContextMenu,
    )
}

/// A batch-action button that shows the selection count and is disabled (no
/// `on_press`) when nothing is selected.
pub(crate) fn sftp_batch_button(label: &'static str, count: usize, message: Message) -> Element<'static, Message> {
    let caption = if count > 0 {
        format!("{label} ({count})")
    } else {
        label.to_string()
    };
    let button = button(text(caption).size(12))
        .padding([3, 10])
        .style(|_theme, status| secondary_button_style(status));
    if count > 0 {
        button.on_press(message).into()
    } else {
        button.into()
    }
}

pub(crate) fn sftp_date(mtime: Option<u64>) -> String {
    mtime.map(format_mtime).unwrap_or_else(|| String::from("—"))
}

/// Local UTC offset in seconds, cached for the session (timezone is stable).
/// Falls back to 0 (UTC) if it cannot be determined (e.g. the soundness guard
/// on multi-threaded Unix; on Windows it always resolves).
pub(crate) fn local_offset_secs() -> i64 {
    static OFFSET: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    *OFFSET.get_or_init(|| {
        time::UtcOffset::current_local_offset()
            .map(|offset| i64::from(offset.whole_seconds()))
            .unwrap_or(0)
    })
}

/// Format a Unix timestamp as local `YYYY-MM-DD HH:MM`.
pub(crate) fn format_mtime(secs: u64) -> String {
    let local = (secs as i64).saturating_add(local_offset_secs()).max(0) as u64;
    format_epoch_utc(local)
}

/// Format seconds-since-epoch (UTC) as `YYYY-MM-DD HH:MM` using the
/// days-from-civil algorithm (no external date dependency).
pub(crate) fn format_epoch_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };

    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}")
}

pub(crate) fn sftp_pane_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color()),
        ..container::Style::default()
    }
}

/// Wrap a pane in drag plumbing: tracks the cursor while a drag is in flight
/// (so the ghost can follow it) and overlays the ghost when this pane is the one
/// under the pointer.
pub(crate) fn sftp_drag_layer<'a>(
    app: &AditApp,
    pane: SftpPane,
    body: Element<'a, Message>,
) -> Element<'a, Message> {
    let dragging = app.sftp_drag.is_some();
    let content: Element<'a, Message> = match app.sftp_drag_cursor {
        Some(position) if dragging && app.sftp_drag_over == Some(pane) => {
            let (name, count) = drag_subject(app);
            stack![body, drag_ghost(name, count, position)]
                .width(Length::FillPortion(1))
                .height(Fill)
                .into()
        }
        _ => body,
    };
    let mut area = mouse_area(content).on_enter(Message::SftpDragEnter(pane));
    if dragging {
        area = area.on_move(move |point| Message::SftpDragMove(pane, point));
    }
    area.into()
}

/// The dragged file name and how many items the drag carries (the selection if
/// the dragged file is part of a multi-selection, else 1).
pub(crate) fn drag_subject(app: &AditApp) -> (String, usize) {
    match &app.sftp_drag {
        Some((src, name)) => {
            let selection = match src {
                SftpPane::Local => &app.sftp_local_selected,
                SftpPane::Remote => &app.sftp_remote_selected,
            };
            let count = if selection.contains(name) && selection.len() > 1 {
                selection.len()
            } else {
                1
            };
            (name.clone(), count)
        }
        None => (String::new(), 0),
    }
}

/// A floating chip that follows the cursor inside the pane during a drag,
/// positioned with leading spacers (pane-relative coordinates from `on_move`).
pub(crate) fn drag_ghost(name: String, count: usize, position: Point) -> Element<'static, Message> {
    let label = if count > 1 {
        format!("⠿ {name}  +{}", count - 1)
    } else {
        format!("⠿ {name}")
    };
    column![
        Space::new().height(Length::Fixed((position.y + 12.0).max(0.0))),
        row![
            Space::new().width(Length::Fixed((position.x + 14.0).max(0.0))),
            container(text(label).size(11).color(primary_text()))
                .padding([3, 8])
                .style(|_theme| drag_ghost_style()),
        ],
    ]
    .width(Fill)
    .height(Fill)
    .into()
}

pub(crate) fn drag_ghost_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.5, accent()),
        shadow: Shadow {
            color: Color::from_rgba(0.0, 0.0, 0.0, 0.25),
            offset: Vector::new(0.0, 2.0),
            blur_radius: 8.0,
        },
        ..container::Style::default()
    }
}

/// Pane container that highlights (tinted background + accent border) when it is
/// the active drop target of a pane-to-pane drag.
pub(crate) fn sftp_pane_style_dropzone(active: bool) -> container::Style {
    let mut style = sftp_pane_style();
    if active {
        style.background = Some(Background::Color(accent_soft()));
        style.border = border(RADIUS_SM, 2.0, accent());
    }
    style
}

pub(crate) fn sort_header_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => panel_background_hover(),
        _ => transparent(),
    };
    base_button_style(background, muted_text(), transparent())
}

/// One clickable column header that sorts a pane and shows an arrow when active.
pub(crate) fn sftp_sort_cell(
    label: &'static str,
    pane: SftpPane,
    column: SftpSortKey,
    active: (SftpSortKey, bool),
    width: Length,
) -> Element<'static, Message> {
    let is_active = active.0 == column;
    let arrow = if is_active {
        if active.1 {
            " ▲"
        } else {
            " ▼"
        }
    } else {
        ""
    };
    let color = if is_active { accent() } else { muted_text() };
    button(text(format!("{label}{arrow}")).size(10).color(color))
        .width(width)
        .padding([2, 4])
        .style(|_theme, status| sort_header_button_style(status))
        .on_press(Message::SftpSort(pane, column))
        .into()
}

/// The sortable column header row shared by both panes; the trailing space keeps
/// the columns roughly aligned with the per-row action buttons on the right.
pub(crate) fn sftp_sort_header(pane: SftpPane, active: (SftpSortKey, bool)) -> Element<'static, Message> {
    row![
        sftp_sort_cell("名称", pane, SftpSortKey::Name, active, Length::Fill),
        sftp_sort_cell("大小", pane, SftpSortKey::Size, active, Length::Fixed(64.0)),
        sftp_sort_cell("修改时间", pane, SftpSortKey::Modified, active, Length::Fixed(118.0)),
        Space::new().width(Length::Fixed(132.0)),
    ]
    .spacing(6)
    .padding([0, 6])
    .into()
}

/// Compare two entries by the active sort column/direction (dirs are grouped
/// first by the caller, so this only orders within a group).
pub(crate) fn sftp_cmp(
    key: SftpSortKey,
    ascending: bool,
    a: (&str, u64, Option<u64>),
    b: (&str, u64, Option<u64>),
) -> std::cmp::Ordering {
    let base = match key {
        SftpSortKey::Name => a.0.to_lowercase().cmp(&b.0.to_lowercase()),
        SftpSortKey::Size => a.1.cmp(&b.1),
        SftpSortKey::Modified => a.2.unwrap_or(0).cmp(&b.2.unwrap_or(0)),
    };
    if ascending {
        base
    } else {
        base.reverse()
    }
}

pub(crate) fn sftp_list_inner_style() -> container::Style {
    container::Style {
        background: Some(Background::Color(surface_alt())),
        text_color: Some(primary_text()),
        border: border(RADIUS_SM, 1.0, border_color()),
        ..container::Style::default()
    }
}

pub(crate) fn sftp_entry_button_style(status: button::Status) -> button::Style {
    let background = match status {
        button::Status::Hovered => panel_background_hover(),
        button::Status::Pressed => accent_soft(),
        _ => transparent(),
    };
    base_button_style(background, primary_text(), transparent())
}

pub(crate) fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

pub(crate) fn form_endpoint(app: &AditApp) -> String {
    let username = app.profile_username.trim();
    let host = app.profile_host.trim();
    let port = app.profile_port.trim();

    if username.is_empty() || host.is_empty() || port.is_empty() {
        String::from("会话信息不完整")
    } else {
        format!("{username}@{host}:{port}")
    }
}

pub(crate) fn form_matches_selected_profile(app: &AditApp) -> bool {
    let Some(profile_id) = app.selected_profile else {
        return false;
    };
    let Some(profile) = app.manager.profile(profile_id) else {
        return false;
    };

    profile.group == app.profile_group.trim()
        && profile.name == app.profile_name.trim()
        && profile.host == app.profile_host.trim()
        && profile.port.to_string() == app.profile_port.trim()
        && profile.username == app.profile_username.trim()
        && profile.auth_method == app.profile_auth_method
        && profile.identity_file == app.profile_identity_file.trim()
        && profile.protocol == app.profile_protocol
        && profile.startup_command == app.profile_startup_command.trim()
        && profile.terminal_type == app.profile_terminal_type.trim()
        // Compare the raw field to the canonical saved spec (not the parsed set):
        // an unsaved/invalid hop the user typed must read as "modified", never
        // "saved", so the silent-drop guard on save isn't masked by the indicator.
        && app.profile_jumps.trim() == jumps_to_spec(&profile.jumps)
        && profile.environment == app.profile_environment
        && profile.accent_color.as_deref().unwrap_or_default() == app.profile_accent_color.trim()
        && profile.label.as_deref().unwrap_or_default() == app.profile_label.trim()
}
