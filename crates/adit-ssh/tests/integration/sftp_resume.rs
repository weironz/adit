use super::*;

#[test]
fn verified_resume_roundtrips_against_real_sshd() {
    let server = TestServer::start();
    let mut request = SftpRequest::new("127.0.0.1", server.port, USER, PASS);
    request.known_hosts_path = temp_known_hosts();
    let handle = spawn_sftp_session(request).expect("spawn sftp");
    wait_sftp_ready(&handle, Duration::from_secs(20));
    let content = b"synthetic verified resume payload 123456789";
    let partial = temp_path("resume-partial");
    let full = temp_path("resume-full");
    let download = temp_path("resume-download");
    let remote = format!("/tmp/adit-resume-{}", unique());
    fs::write(&partial, &content[..12]).unwrap();
    fs::write(&full, content).unwrap();
    handle
        .send(SftpCommand::Upload {
            id: 1,
            local: partial.clone(),
            remote: remote.clone(),
        })
        .unwrap();
    wait_sftp_done(&handle, Duration::from_secs(20));
    handle
        .send(SftpCommand::UploadWith {
            id: 2,
            local: full.clone(),
            remote: remote.clone(),
            mode: adit_ssh::TransferMode::Resume,
        })
        .unwrap();
    wait_sftp_done(&handle, Duration::from_secs(20));
    fs::write(&download, &content[..10]).unwrap();
    handle
        .send(SftpCommand::DownloadWith {
            id: 3,
            remote: remote.clone(),
            local: download.clone(),
            mode: adit_ssh::TransferMode::Resume,
        })
        .unwrap();
    wait_sftp_done(&handle, Duration::from_secs(20));
    assert_eq!(fs::read(&download).unwrap(), content);
    fs::write(&download, b"wrong-data").unwrap();
    handle
        .send(SftpCommand::DownloadWith {
            id: 4,
            remote,
            local: download.clone(),
            mode: adit_ssh::TransferMode::Resume,
        })
        .unwrap();
    let failed = pump_until(
        Duration::from_secs(20),
        || handle.try_recv(),
        |event| match event {
            SftpEvent::TransferFailed { id: 4, .. } => ControlFlow::Break(true),
            SftpEvent::Done { id: 4, .. } => ControlFlow::Break(false),
            _ => ControlFlow::Continue(()),
        },
    );
    assert_eq!(failed, Some(true));
    assert_eq!(fs::read(&download).unwrap(), b"wrong-data");
    handle.send(SftpCommand::Disconnect).unwrap();
    for path in [partial, full, download] {
        let _ = fs::remove_file(path);
    }
}
