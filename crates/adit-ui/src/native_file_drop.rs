//! Window-owned OLE drops. A drag acquires its lease at the first legal hit,
//! and losing that lease is permanent until another DragEnter.
use super::*;
use std::{
    cell::RefCell,
    collections::{HashMap, VecDeque},
    path::PathBuf,
};

const MAX_FILES: usize = 4096;
const MAX_PENDING: usize = 32;
// UTF-16 storage: 4 MiB per batch, 16 MiB across the unconsumed queue.
const MAX_BATCH_UNITS: usize = 2 * 1024 * 1024;
const MAX_QUEUE_UNITS: usize = 8 * 1024 * 1024;

fn path_units(path: &std::path::Path) -> usize {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().count() + 1
    }
    #[cfg(not(windows))]
    {
        path.to_string_lossy().encode_utf16().count() + 1
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Destination {
    Shell {
        session: SessionId,
        revision: u64,
        cwd: String,
    },
    Panel {
        instance: SessionId,
        revision: u64,
        cwd: String,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Region {
    pub destination: Destination,
    pub bounds: iced::Rectangle,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Policy {
    #[default]
    Disabled,
    Shells(HashMap<SessionId, (u64, String)>),
    Panel {
        instance: SessionId,
        revision: u64,
        cwd: String,
    },
}

impl Policy {
    pub(crate) fn permits(&self, destination: &Destination) -> bool {
        match (self, destination) {
            (
                Self::Shells(sessions),
                Destination::Shell {
                    session,
                    revision,
                    cwd,
                },
            ) => sessions
                .get(session)
                .is_some_and(|identity| identity.0 == *revision && identity.1 == *cwd),
            (
                Self::Panel {
                    instance,
                    revision,
                    cwd,
                },
                Destination::Panel {
                    instance: other,
                    revision: generation,
                    cwd: path,
                },
            ) => instance == other && revision == generation && cwd == path,
            _ => false,
        }
    }
}

/// Compared on every application update, including updates without a redraw.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Context {
    pub policy: Policy,
    pub selected: Option<SessionId>,
    pub panes: Vec<SessionId>,
    pub metrics: [f32; 6],
    pub main_view: MainView,
    pub tile_mode: TileMode,
    pub local_layout: [bool; 2],
}

#[derive(Clone, Copy)]
pub(crate) enum Mark {
    Shell(SessionId),
    Panel,
}

thread_local! {
    static FRAME: RefCell<Option<(Policy, Vec<Region>)>> = const { RefCell::new(None) };
}

pub(crate) fn begin_frame(policy: Policy) {
    FRAME.with(|frame| *frame.borrow_mut() = Some((policy, Vec::new())));
}

pub(crate) fn mark(mark: Mark, bounds: iced::Rectangle, viewport: iced::Rectangle) {
    let Some(bounds) = bounds.intersection(&viewport) else {
        return;
    };
    if !bounds.width.is_finite()
        || !bounds.height.is_finite()
        || bounds.width <= 0.0
        || bounds.height <= 0.0
    {
        return;
    }
    FRAME.with(|frame| {
        let mut frame = frame.borrow_mut();
        let Some((policy, regions)) = frame.as_mut() else {
            return;
        };
        let destination = match (policy, mark) {
            (Policy::Shells(sessions), Mark::Shell(session)) if sessions.contains_key(&session) => {
                let (revision, cwd) = &sessions[&session];
                Destination::Shell {
                    session,
                    revision: *revision,
                    cwd: cwd.clone(),
                }
            }
            (
                Policy::Panel {
                    instance,
                    revision,
                    cwd,
                },
                Mark::Panel,
            ) => Destination::Panel {
                instance: *instance,
                revision: *revision,
                cwd: cwd.clone(),
            },
            _ => return,
        };
        regions.push(Region {
            destination,
            bounds,
        });
    });
}

#[cfg(windows)]
pub(crate) fn finish_frame(window: window::Id, bounds: iced::Rectangle) {
    let frame = FRAME.with(|frame| frame.borrow_mut().take());
    if let Some((policy, regions)) = frame {
        let mut state = shared().lock().unwrap_or_else(|error| error.into_inner());
        let metrics = state
            .slots
            .get(&window)
            .and_then(|slot| ole::dimensions(slot.hwnd));
        state.publish(window, metrics, bounds, policy, regions);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Metrics {
    dpi: u32,
    width: i32,
    height: i32,
}

impl Metrics {
    fn logical_point(self, client_x: i32, client_y: i32) -> Option<Point> {
        if self.dpi == 0
            || client_x < 0
            || client_y < 0
            || client_x >= self.width
            || client_y >= self.height
        {
            return None;
        }
        let scale = self.dpi as f32 / 96.0;
        Some(Point::new(client_x as f32 / scale, client_y as f32 / scale))
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Geometry {
    metrics: Metrics,
    regions: Vec<Region>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Lease {
    hwnd: isize,
    generation: u64,
    serial: u64,
    destination: Destination,
}

#[derive(Default)]
enum Drag {
    #[default]
    Idle,
    Waiting,
    Bound(Lease),
    Invalid,
}

#[derive(Default)]
struct Slot {
    hwnd: isize,
    generation: u64,
    serial: u64,
    exhausted: bool,
    context: Option<Context>,
    geometry: Option<Geometry>,
    drag: Drag,
}

impl Slot {
    fn invalidate(&mut self) {
        if let Some(next) = self.generation.checked_add(1) {
            self.generation = next;
        } else {
            self.exhausted = true;
        }
        self.geometry = None;
        if !matches!(self.drag, Drag::Idle) {
            self.drag = Drag::Invalid;
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct DropRequest {
    pub window: window::Id,
    pub destination: Destination,
    pub paths: Vec<PathBuf>,
    lease: Lease,
    units: usize,
}

#[derive(Default)]
struct Shared {
    slots: HashMap<window::Id, Slot>,
    pending: VecDeque<DropRequest>,
    pending_units: usize,
}

impl Shared {
    fn install(&mut self, window: window::Id, hwnd: isize) {
        self.forget(window);
        self.slots.insert(
            window,
            Slot {
                hwnd,
                ..Slot::default()
            },
        );
    }

    fn forget(&mut self, window: window::Id) {
        self.slots.remove(&window);
        self.pending.retain(|item| {
            if item.window == window {
                self.pending_units -= item.units;
                false
            } else {
                true
            }
        });
    }

    fn reconcile(&mut self, window: window::Id, context: Context) {
        if let Some(slot) = self.slots.get_mut(&window) {
            if slot.context.as_ref() != Some(&context) {
                slot.invalidate();
                slot.context = Some(context);
            }
        }
    }

    fn publish(
        &mut self,
        window: window::Id,
        metrics: Option<Metrics>,
        bounds: iced::Rectangle,
        policy: Policy,
        regions: Vec<Region>,
    ) {
        let Some(slot) = self.slots.get_mut(&window) else {
            return;
        };
        let Some(metrics) = metrics else {
            slot.invalidate();
            return;
        };
        let scale = metrics.dpi as f32 / 96.0;
        if slot.exhausted
            || metrics.dpi == 0
            || metrics.width <= 0
            || metrics.height <= 0
            || !bounds.width.is_finite()
            || !bounds.height.is_finite()
            || bounds.x != 0.0
            || bounds.y != 0.0
            || (bounds.width * scale - metrics.width as f32).abs() > 2.0
            || (bounds.height * scale - metrics.height as f32).abs() > 2.0
            || slot
                .context
                .as_ref()
                .is_none_or(|context| context.policy != policy)
            || regions
                .iter()
                .any(|region| !policy.permits(&region.destination))
        {
            slot.invalidate();
            return;
        }
        let next = Geometry { metrics, regions };
        if slot.geometry.as_ref() != Some(&next) {
            slot.invalidate();
            if !slot.exhausted {
                slot.geometry = Some(next);
            }
        }
    }

    fn enter(&mut self, window: window::Id) -> Option<u64> {
        let slot = self.slots.get_mut(&window)?;
        let Some(serial) = slot.serial.checked_add(1) else {
            slot.exhausted = true;
            slot.drag = Drag::Invalid;
            return None;
        };
        slot.serial = serial;
        slot.drag = if slot.exhausted {
            Drag::Invalid
        } else {
            Drag::Waiting
        };
        Some(serial)
    }

    fn leave(&mut self, window: window::Id, serial: u64) {
        if let Some(slot) = self
            .slots
            .get_mut(&window)
            .filter(|slot| slot.serial == serial)
        {
            slot.drag = Drag::Idle;
        }
    }

    fn hover(
        &mut self,
        window: window::Id,
        hwnd: isize,
        sample: Option<(Metrics, i32, i32)>,
    ) -> Option<Lease> {
        let slot = self.slots.get_mut(&window)?;
        if slot.exhausted || matches!(slot.drag, Drag::Idle | Drag::Invalid) {
            return None;
        }
        let candidate = (|| {
            if hwnd == 0 || slot.hwnd != hwnd {
                return None;
            }
            let geometry = slot.geometry.as_ref()?;
            let (metrics, x, y) = sample?;
            if geometry.metrics != metrics {
                return None;
            }
            let point = metrics.logical_point(x, y)?;
            let mut hits = geometry
                .regions
                .iter()
                .filter(|region| region.bounds.contains(point));
            let destination = hits.next()?.destination.clone();
            if hits.next().is_some() {
                return None;
            }
            Some(Lease {
                hwnd,
                generation: slot.generation,
                serial: slot.serial,
                destination,
            })
        })();
        match &slot.drag {
            Drag::Waiting => {
                if let Some(lease) = &candidate {
                    slot.drag = Drag::Bound(lease.clone());
                }
                candidate
            }
            Drag::Bound(bound) if candidate.as_ref() == Some(bound) => candidate,
            _ => {
                slot.drag = Drag::Invalid;
                None
            }
        }
    }

    fn enqueue(&mut self, window: window::Id, lease: Lease, paths: Vec<PathBuf>) -> bool {
        if paths.is_empty() || paths.len() > MAX_FILES || self.pending.len() >= MAX_PENDING {
            return false;
        }
        let Some(units) = paths.iter().try_fold(0usize, |sum, path| {
            sum.checked_add(path_units(path))
                .filter(|sum| *sum <= MAX_BATCH_UNITS)
        }) else {
            return false;
        };
        let Some(total) = self
            .pending_units
            .checked_add(units)
            .filter(|total| *total <= MAX_QUEUE_UNITS)
        else {
            return false;
        };
        let Some(slot) = self.slots.get_mut(&window) else {
            return false;
        };
        if !matches!(&slot.drag, Drag::Bound(bound) if bound == &lease)
            || slot.generation != lease.generation
            || slot.geometry.is_none()
            || slot.exhausted
        {
            return false;
        }
        slot.drag = Drag::Idle;
        self.pending_units = total;
        self.pending.push_back(DropRequest {
            window,
            destination: lease.destination.clone(),
            paths,
            lease,
            units,
        });
        true
    }

    fn current(&self, request: &DropRequest) -> bool {
        self.slots.get(&request.window).is_some_and(|slot| {
            !slot.exhausted
                && slot.hwnd == request.lease.hwnd
                && slot.generation == request.lease.generation
                && slot
                    .context
                    .as_ref()
                    .is_some_and(|context| context.policy.permits(&request.destination))
                && slot.geometry.as_ref().is_some_and(|geometry| {
                    geometry
                        .regions
                        .iter()
                        .any(|region| region.destination == request.destination)
                })
        })
    }

    fn drain(&mut self) -> Vec<DropRequest> {
        self.pending_units = 0;
        std::mem::take(&mut self.pending)
            .into_iter()
            .filter(|request| self.current(request))
            .collect()
    }
}

#[cfg(windows)]
fn shared() -> &'static std::sync::Mutex<Shared> {
    static STATE: std::sync::OnceLock<std::sync::Mutex<Shared>> = std::sync::OnceLock::new();
    STATE.get_or_init(|| std::sync::Mutex::new(Shared::default()))
}

#[cfg(windows)]
pub(crate) fn reconcile(window: window::Id, context: Context) {
    shared()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .reconcile(window, context);
}

#[cfg(windows)]
pub(crate) fn invalidate(window: window::Id) {
    if let Some(slot) = shared()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .slots
        .get_mut(&window)
    {
        slot.invalidate();
    }
}

#[cfg(windows)]
pub(crate) fn forget(window: window::Id) {
    shared()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .forget(window);
}

#[cfg(windows)]
pub(crate) fn drain() -> Vec<DropRequest> {
    shared()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .drain()
}

#[cfg(windows)]
pub(crate) fn current(request: &DropRequest) -> bool {
    shared()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .current(request)
}

#[cfg(windows)]
mod ole;
#[cfg(windows)]
pub(crate) use ole::register_task;

#[cfg(test)]
mod tests;

#[cfg(all(windows, test))]
pub(crate) mod test_support {
    use super::*;
    pub(crate) static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    pub(crate) fn install(window: window::Id) {
        shared().lock().unwrap().install(window, 17);
    }
    pub(crate) fn publish(window: window::Id, policy: Policy, regions: Vec<Region>) {
        shared().lock().unwrap().publish(
            window,
            Some(Metrics {
                dpi: 144,
                width: 1440,
                height: 1050,
            }),
            iced::Rectangle::new(Point::ORIGIN, iced::Size::new(960.0, 700.0)),
            policy,
            regions,
        );
    }
    pub(crate) fn queue(window: window::Id, point: Point, paths: Vec<PathBuf>) -> bool {
        let mut state = shared().lock().unwrap();
        state.enter(window);
        let lease = state.hover(
            window,
            17,
            Some((
                Metrics {
                    dpi: 144,
                    width: 1440,
                    height: 1050,
                },
                (point.x * 1.5) as i32,
                (point.y * 1.5) as i32,
            )),
        );
        lease.is_some_and(|lease| state.enqueue(window, lease, paths))
    }
}
