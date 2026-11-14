use std::{
    fs::File,
    io,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use tokio::{
    runtime::Handle,
    sync::{oneshot, watch},
};

use crate::acp::{
    client::HttpClient,
    retained::load::{self, Cancellation, Loaded},
    session_paths::SessionPathRoots,
};
use crate::tui::{
    rich_text::{
        disk::Document,
        window::{self, Anchor, Index, Selection},
    },
    styles::Theme,
};

pub(crate) enum CopyRange {
    All,
    Selection { width: u16, selection: Selection },
}
struct CopyRequest {
    identity: String,
    range: CopyRange,
    reply: oneshot::Sender<io::Result<File>>,
}
#[derive(Clone)]
pub(crate) struct Request {
    pub epoch: u64,
    pub document_epoch: u64,
    pub theme: Theme,
    pub width: u16,
    pub height: u16,
    pub offset: usize,
    pub anchor: Option<Anchor>,
}
pub(crate) struct Window {
    pub epoch: u64,
    pub document_epoch: u64,
    pub width: u16,
    pub height: u16,
    pub offset: usize,
    pub first: usize,
    pub total: usize,
    pub anchor: Option<Anchor>,
    pub text: ratatui::text::Text<'static>,
}
pub(crate) struct Snapshot {
    pub identity: String,
    pub highest_seq: u64,
    pub incomplete: bool,
    pub window: Option<Window>,
}
pub(crate) type Update = Result<Snapshot, String>;

pub(crate) struct Worker {
    pub requests: watch::Sender<Option<Request>>,
    pub results: watch::Receiver<Option<Arc<Update>>>,
    pub epoch: Arc<AtomicU64>,
    cancellation: Cancellation,
    task: tokio::task::JoinHandle<()>,
    copy: Arc<Mutex<Option<CopyRequest>>>,
}
impl Worker {
    pub fn new(http: HttpClient, session: String, roots: SessionPathRoots) -> Self {
        let (requests, receiver) = watch::channel(None);
        let (results, output) = watch::channel(None);
        let epoch = Arc::new(AtomicU64::new(0));
        let cancellation = Cancellation::default();
        let copy = Arc::new(Mutex::new(None));
        let runtime = Handle::current();
        let control = Control {
            runtime,
            requests: receiver,
            results,
            epoch: Arc::clone(&epoch),
            cancellation: cancellation.clone(),
            copy: Arc::clone(&copy),
        };
        let task = tokio::task::spawn_blocking(move || {
            if let Err(error) = run(&control, &http, &session, &roots) {
                if !control.cancellation.is_cancelled() {
                    control
                        .results
                        .send_replace(Some(Arc::new(Err(format!("{error:#}")))));
                }
            }
        });
        Self {
            requests,
            results: output,
            epoch,
            cancellation,
            task,
            copy,
        }
    }
    pub fn copy(&self, identity: String, range: CopyRange) -> oneshot::Receiver<io::Result<File>> {
        let (reply, result) = oneshot::channel();
        *self.copy.lock().unwrap() = Some(CopyRequest {
            identity,
            range,
            reply,
        });
        self.requests.send_modify(|_| {});
        result
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancellation.cancel();
        self.task.abort();
    }
}
struct Control {
    runtime: Handle,
    requests: watch::Receiver<Option<Request>>,
    results: watch::Sender<Option<Arc<Update>>>,
    epoch: Arc<AtomicU64>,
    cancellation: Cancellation,
    copy: Arc<Mutex<Option<CopyRequest>>>,
}
#[derive(Clone, Copy)]
enum Flow {
    Theme,
    Reload,
    Stop,
}
fn run(
    control: &Control,
    http: &HttpClient,
    session: &str,
    roots: &SessionPathRoots,
) -> anyhow::Result<()> {
    let mut requests = control.requests.clone();
    loop {
        let loaded = load::read(http, session, &control.runtime, &control.cancellation)?;
        control
            .results
            .send_replace(Some(Arc::new(Ok(snapshot(&loaded, None)))));
        let mut next_probe = Instant::now() + Duration::from_secs(2);
        loop {
            if control.cancellation.is_cancelled() {
                return Ok(());
            }
            let request = requests.borrow_and_update().clone();
            if history_changed(control, http, session, &loaded, &mut next_probe)? {
                break;
            }
            let Some(request) = request.filter(|request| request.width != 0 && request.height != 0)
            else {
                if !wait(control, &mut requests, next_probe)? {
                    return Ok(());
                }
                continue;
            };
            let document_epoch = request.document_epoch;
            let current_document = || {
                !control.cancellation.is_cancelled()
                    && control
                        .requests
                        .borrow()
                        .as_ref()
                        .is_some_and(|request| request.document_epoch == document_epoch)
            };
            let mut flow = Flow::Theme;
            let result = loaded.context.spool.with_speculation::<()>(|| {
                let document = loaded.context.store.with_transaction(|_| {
                    super::document(
                        &loaded.context,
                        &loaded.projection,
                        &loaded.invocations,
                        &request.theme,
                        Some(roots),
                        current_document,
                    )
                })?;
                let mut indexes = std::collections::VecDeque::with_capacity(2);
                let mut rendered_epoch = None;
                loop {
                    if !current_document() {
                        break;
                    }
                    let request = requests
                        .borrow_and_update()
                        .clone()
                        .ok_or_else(interrupted)?;
                    if request.document_epoch != document_epoch {
                        break;
                    }
                    if history_changed(control, http, session, &loaded, &mut next_probe)? {
                        flow = Flow::Reload;
                        break;
                    }
                    if request.width == 0 || request.height == 0 {
                        if !wait(control, &mut requests, next_probe)? {
                            flow = Flow::Stop;
                            break;
                        }
                        continue;
                    }
                    let width = request.width.max(1);
                    let current_width = || {
                        current_document()
                            && control
                                .requests
                                .borrow()
                                .as_ref()
                                .is_some_and(|request| request.width.max(1) == width)
                    };
                    let position = match cached_index(
                        &mut indexes,
                        &loaded.context.store,
                        &document,
                        width,
                        current_width,
                    ) {
                        Ok(position) => position,
                        Err(error)
                            if error.kind() == io::ErrorKind::Interrupted
                                && !control.cancellation.is_cancelled() =>
                        {
                            continue
                        }
                        Err(error) => return Err(error),
                    };
                    if history_changed(control, http, session, &loaded, &mut next_probe)? {
                        flow = Flow::Reload;
                        break;
                    }
                    let index = &indexes[position];
                    let total = usize::try_from(index.len()).map_err(io::Error::other)?;
                    let maximum = total.saturating_sub(usize::from(request.height));
                    let first = if let Some(anchor) = request.anchor {
                        usize::try_from(index.locate(anchor)?)
                            .map_err(io::Error::other)?
                            .min(maximum)
                    } else {
                        maximum.saturating_sub(request.offset)
                    };
                    let current = || {
                        !control.cancellation.is_cancelled()
                            && control.epoch.load(Ordering::Relaxed) == request.epoch
                    };
                    if rendered_epoch != Some(request.epoch) {
                        match index.window(
                            &document,
                            first as u64..index.len().min(first as u64 + u64::from(request.height)),
                            current,
                        ) {
                            Ok(text) if current() => {
                                let anchor = if total != 0 {
                                    let row = index.row(first as u64)?;
                                    Some(Anchor {
                                        line: row.line,
                                        byte: row.start,
                                    })
                                } else {
                                    None
                                };
                                let window = Window {
                                    epoch: request.epoch,
                                    document_epoch,
                                    width,
                                    height: request.height,
                                    offset: maximum - first,
                                    first,
                                    total,
                                    anchor,
                                    text,
                                };
                                control.results.send_replace(Some(Arc::new(Ok(snapshot(
                                    &loaded,
                                    Some(window),
                                )))));
                                rendered_epoch = Some(request.epoch);
                            }
                            Ok(_) => {}
                            Err(error)
                                if error.kind() == io::ErrorKind::Interrupted
                                    && !control.cancellation.is_cancelled() =>
                            {
                                continue
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    let copy = control.copy.lock().unwrap().take();
                    if let Some(copy) = copy {
                        let current = || current_document() && !copy.reply.is_closed();
                        let result = (|| {
                            if copy.identity != loaded.identity {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "retained history changed before copy",
                                ));
                            }
                            let mut file = tempfile::NamedTempFile::new()?;
                            match copy.range {
                                CopyRange::All => {
                                    window::copy_all(&document, file.as_file_mut(), current)?
                                }
                                CopyRange::Selection { width, selection } => {
                                    let position = cached_index(
                                        &mut indexes,
                                        &loaded.context.store,
                                        &document,
                                        width,
                                        current,
                                    )?;
                                    if !indexes[position].copy_selection(
                                        &document,
                                        selection,
                                        file.as_file_mut(),
                                        current,
                                    )? {
                                        return Err(io::Error::new(
                                            io::ErrorKind::InvalidInput,
                                            "selection contains no text",
                                        ));
                                    }
                                }
                            }
                            if !current() {
                                return Err(interrupted());
                            }
                            let meta =
                                load::probe(http, session, &control.runtime, &control.cancellation)
                                    .map_err(io::Error::other)?;
                            if meta.history_identity != loaded.identity {
                                flow = Flow::Reload;
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "retained history changed during copy",
                                ));
                            }
                            crate::tui::clipboard::finalize_export(file)
                        })();
                        let _ = copy.reply.send(result);
                        if matches!(flow, Flow::Reload) {
                            break;
                        }
                    }
                    if !wait(control, &mut requests, next_probe)? {
                        flow = Flow::Stop;
                        break;
                    }
                }
                Ok(None)
            });
            match result {
                Ok(_) => {}
                Err(error)
                    if error.kind() == io::ErrorKind::Interrupted
                        && !control.cancellation.is_cancelled() =>
                {
                    continue
                }
                Err(error) => return Err(error.into()),
            }
            match flow {
                Flow::Theme => continue,
                Flow::Reload => break,
                Flow::Stop => return Ok(()),
            }
        }
    }
}
fn snapshot(loaded: &Loaded, window: Option<Window>) -> Snapshot {
    Snapshot {
        identity: loaded.identity.clone(),
        highest_seq: loaded.highest_seq,
        incomplete: loaded.incomplete,
        window,
    }
}
fn wait(
    control: &Control,
    requests: &mut watch::Receiver<Option<Request>>,
    deadline: Instant,
) -> io::Result<bool> {
    control.runtime.block_on(async {
        tokio::select! {
            _ = control.cancellation.cancelled() => Ok(false),
            result = requests.changed() => Ok(result.is_ok()),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => Ok(true),
        }
    })
}
fn interrupted() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "retained viewport superseded")
}

fn cached_index<'a>(
    indexes: &mut std::collections::VecDeque<Index<'a>>,
    store: &pulldown_cmark::disk::Store,
    document: &Document<'a>,
    width: u16,
    current: impl Fn() -> bool,
) -> io::Result<usize> {
    let width = width.max(1);
    if let Some(position) = indexes.iter().position(|index| index.width == width) {
        return Ok(position);
    }
    if indexes.len() == 2 {
        indexes.pop_front();
    }
    let index = store.with_transaction(|_| Index::build(document, width, current))?;
    indexes.push_back(index);
    Ok(indexes.len() - 1)
}

fn history_changed(
    control: &Control,
    http: &HttpClient,
    session: &str,
    loaded: &Loaded,
    next: &mut Instant,
) -> io::Result<bool> {
    if Instant::now() < *next {
        return Ok(false);
    }
    let meta = load::probe(http, session, &control.runtime, &control.cancellation)
        .map_err(io::Error::other)?;
    *next = Instant::now() + Duration::from_secs(2);
    Ok(meta.history_identity != loaded.identity)
}
