use super::*;

fn context(policy: Policy) -> Context {
    Context {
        policy,
        selected: None,
        panes: Vec::new(),
        metrics: [0.0; 6],
        main_view: MainView::Terminal,
        tile_mode: TileMode::Grid,
        local_layout: [false; 2],
    }
}

fn panel(instance: SessionId, cwd: &str) -> Destination {
    Destination::Panel {
        instance,
        revision: 0,
        cwd: cwd.into(),
    }
}

fn policy(destination: &Destination) -> Policy {
    match destination {
        Destination::Panel {
            instance,
            revision,
            cwd,
        } => Policy::Panel {
            instance: *instance,
            revision: *revision,
            cwd: cwd.clone(),
        },
        Destination::Shell {
            session,
            revision,
            cwd,
        } => Policy::Shells(HashMap::from([(*session, (*revision, cwd.clone()))])),
    }
}

fn metrics() -> Metrics {
    Metrics {
        dpi: 144,
        width: 960,
        height: 720,
    }
}
fn bounds() -> iced::Rectangle {
    iced::Rectangle::new(Point::ORIGIN, iced::Size::new(640.0, 480.0))
}
fn region(destination: Destination) -> Region {
    Region {
        destination,
        bounds: bounds(),
    }
}

fn fixture() -> (Shared, window::Id, Destination) {
    let mut state = Shared::default();
    let window = window::Id::unique();
    let target = panel(SessionId::new(), "/a");
    state.install(window, 17);
    publish(&mut state, window, target.clone());
    (state, window, target)
}

fn publish(state: &mut Shared, window: window::Id, target: Destination) {
    let policy = policy(&target);
    state.reconcile(window, context(policy.clone()));
    state.publish(
        window,
        Some(metrics()),
        bounds(),
        policy,
        vec![region(target)],
    );
}

fn hit(state: &mut Shared, window: window::Id) -> Option<Lease> {
    state.hover(window, 17, Some((metrics(), 300, 150)))
}

#[test]
fn physical_client_pixels_are_scaled_once_at_96_120_144_and_192_dpi() {
    for dpi in [96, 120, 144, 192] {
        let metrics = Metrics {
            dpi,
            width: 1920,
            height: 1080,
        };
        assert_eq!(
            metrics.logical_point((dpi * 2) as i32, dpi as i32),
            Some(Point::new(192.0, 96.0))
        );
        assert!(metrics.logical_point(-1, 30).is_none());
        assert!(metrics.logical_point(1920, 30).is_none());
        assert!(metrics.logical_point(30, 1080).is_none());
    }
    assert!(Metrics {
        dpi: 0,
        width: 100,
        height: 100
    }
    .logical_point(1, 1)
    .is_none());
}

#[test]
fn first_legal_hit_binds_even_when_drag_enter_is_over_chrome() {
    let (mut state, window, target) = fixture();
    state.enter(window).unwrap();
    assert!(state.hover(window, 17, Some((metrics(), -5, 10))).is_none());
    let lease = hit(&mut state, window).unwrap();
    assert_eq!(lease.destination, target);
    assert_eq!(hit(&mut state, window), Some(lease.clone()));
    assert!(state.enqueue(window, lease, vec!["first.txt".into()]));
    let requests = state.drain();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].window, window);
    assert_eq!(requests[0].destination, target);
    assert!(state.drain().is_empty());
}

#[test]
fn crossing_from_a_to_b_and_back_never_rebinds_the_drag() {
    let (mut state, window, _) = fixture();
    let a = SessionId::new();
    let b = SessionId::new();
    let policy = Policy::Shells(HashMap::from([
        (a, (0, "/a".into())),
        (b, (0, "/b".into())),
    ]));
    let regions = [a, b]
        .into_iter()
        .enumerate()
        .map(|(index, session)| Region {
            destination: Destination::Shell {
                session,
                revision: 0,
                cwd: if index == 0 { "/a" } else { "/b" }.into(),
            },
            bounds: iced::Rectangle::new(
                Point::new(index as f32 * 320.0, 0.0),
                iced::Size::new(320.0, 480.0),
            ),
        })
        .collect();
    state.reconcile(window, context(policy.clone()));
    state.publish(window, Some(metrics()), bounds(), policy, regions);
    state.enter(window);
    let lease = hit(&mut state, window).unwrap();
    assert!(state
        .hover(window, 17, Some((metrics(), 800, 150)))
        .is_none());
    assert!(hit(&mut state, window).is_none());
    assert!(!state.enqueue(window, lease, vec!["old.txt".into()]));
    state.enter(window);
    assert!(state
        .hover(window, 17, Some((metrics(), 800, 150)))
        .is_some());
}

#[test]
fn a_b_a_context_or_modal_round_trip_is_permanently_invalid_until_enter() {
    for modal in [false, true] {
        let (mut state, window, a) = fixture();
        state.enter(window);
        let lease = hit(&mut state, window).unwrap();
        if modal {
            state.reconcile(window, context(Policy::Disabled));
        } else {
            publish(&mut state, window, panel(SessionId::new(), "/b"));
        }
        publish(&mut state, window, a);
        assert!(hit(&mut state, window).is_none());
        assert!(!state.enqueue(window, lease, vec!["old.txt".into()]));
        state.enter(window);
        assert!(hit(&mut state, window).is_some());
    }
}

#[test]
fn geometry_dpi_window_handle_and_ambiguous_regions_fail_closed() {
    let (mut state, window, target) = fixture();
    state.enter(window);
    let lease = hit(&mut state, window).unwrap();
    assert!(state.hover(window, 18, Some((metrics(), 30, 30))).is_none());
    assert!(!state.enqueue(window, lease, vec!["wrong-window.txt".into()]));
    state.enter(window);
    let changed = Metrics {
        dpi: 192,
        ..metrics()
    };
    assert!(state.hover(window, 17, Some((changed, 30, 30))).is_none());
    state.enter(window);
    state.publish(
        window,
        Some(metrics()),
        bounds(),
        policy(&target),
        vec![region(target.clone()), region(target)],
    );
    state.enter(window);
    assert!(hit(&mut state, window).is_none());
}

#[test]
fn queue_keeps_a_large_batch_ordered_and_rejects_stale_or_closed_destinations() {
    let (mut state, window, target) = fixture();
    let paths: Vec<PathBuf> = (0..150).map(|i| format!("{i}.txt").into()).collect();
    state.enter(window);
    let lease = hit(&mut state, window).unwrap();
    assert!(state.enqueue(window, lease, paths.clone()));
    let requests = state.drain();
    assert_eq!(requests[0].paths, paths);
    // A request already removed from the queue still needs validation at use.
    publish(&mut state, window, panel(SessionId::new(), "/replacement"));
    assert!(!state.current(&requests[0]));
    publish(&mut state, window, target);
    state.enter(window);
    let lease = hit(&mut state, window).unwrap();
    assert!(state.enqueue(window, lease, paths));
    state.forget(window);
    assert!(state.drain().is_empty());
}

#[test]
fn leave_and_reentrant_enter_cannot_complete_an_old_lease() {
    let (mut state, window, _) = fixture();
    let old = state.enter(window).unwrap();
    let lease = hit(&mut state, window).unwrap();
    state.leave(window, old);
    assert!(hit(&mut state, window).is_none());
    state.enter(window);
    state.leave(window, old);
    assert!(hit(&mut state, window).is_some());
    assert!(!state.enqueue(window, lease, vec!["reentrant.txt".into()]));
}

#[test]
fn queue_capacity_and_generation_exhaustion_do_not_accept_partial_batches() {
    let (mut state, window, _) = fixture();
    for _ in 0..MAX_PENDING {
        state.enter(window);
        let lease = hit(&mut state, window).unwrap();
        assert!(state.enqueue(window, lease, vec!["a.txt".into(), "b.txt".into()]));
    }
    state.enter(window);
    let lease = hit(&mut state, window).unwrap();
    assert!(!state.enqueue(window, lease, vec!["overflow.txt".into()]));
    assert_eq!(state.drain().len(), MAX_PENDING);
    let slot = state.slots.get_mut(&window).unwrap();
    slot.generation = u64::MAX;
    slot.invalidate();
    state.enter(window);
    assert!(hit(&mut state, window).is_none());
}

#[test]
fn frame_regions_are_clipped_and_only_the_active_policy_is_published() {
    let session = SessionId::new();
    let destination = Destination::Shell {
        session,
        revision: 4,
        cwd: "/srv".into(),
    };
    begin_frame(policy(&destination));
    mark(Mark::Shell(SessionId::new()), bounds(), bounds());
    mark(Mark::Panel, bounds(), bounds());
    let clip = iced::Rectangle::new(Point::new(20.0, 30.0), iced::Size::new(40.0, 50.0));
    mark(Mark::Shell(session), bounds(), clip);
    let (_, regions) = FRAME.with(|frame| frame.borrow_mut().take().unwrap());
    assert_eq!(
        regions,
        vec![Region {
            destination,
            bounds: clip
        }]
    );
}

#[test]
fn native_path_budgets_refuse_whole_batches_and_return_queue_capacity() {
    let (mut state, window, _) = fixture();
    // Each path is within the Win32 single-path cap; only their sum exceeds
    // the batch cap. UTF-16 count, rather than UTF-8 bytes, is the budget unit.
    let path: PathBuf = "报".repeat(32766).into();
    let large = vec![path.clone(); MAX_BATCH_UNITS / path_units(&path) + 1];
    state.enter(window);
    let lease = hit(&mut state, window).unwrap();
    assert!(!state.enqueue(window, lease, large));
    assert_eq!(state.pending_units, 0);
    let batch = vec![path; MAX_BATCH_UNITS / 32767];
    let units: usize = batch.iter().map(|path| path_units(path)).sum();
    for _ in 0..(MAX_QUEUE_UNITS / units) {
        state.enter(window);
        let lease = hit(&mut state, window).unwrap();
        assert!(state.enqueue(window, lease, batch.clone()));
    }
    let pending = state.pending.len();
    state.enter(window);
    let lease = hit(&mut state, window).unwrap();
    assert!(!state.enqueue(window, lease, batch.clone()));
    assert_eq!(state.pending.len(), pending);
    assert_eq!(state.drain().len(), pending);
    assert_eq!(state.pending_units, 0);
    state.enter(window);
    let lease = hit(&mut state, window).unwrap();
    assert!(state.enqueue(window, lease, batch));
    state.forget(window);
    assert_eq!(state.pending_units, 0);
}
