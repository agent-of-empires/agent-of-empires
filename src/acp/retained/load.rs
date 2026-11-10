use std::{
    io::{self, Write},
    rc::Rc,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{Context as _, Result};
use tokio::runtime::Handle;
use tokio_util::sync::CancellationToken;

use super::{
    invalid,
    invocation::{Invocations, Replay},
    Context, Disk,
};
use crate::acp::{
    client::HttpClient,
    protocol::{RetainedSourceMeta, RetainedSourceQuery, RETAINED_SOURCE_BLOCK_BYTES},
    transcript::{event::View, reducer::Projection},
};

#[derive(Clone, Default)]
pub(crate) struct Cancellation {
    flag: Arc<AtomicBool>,
    network: CancellationToken,
}
impl Cancellation {
    pub fn cancel(&self) {
        self.flag.store(true, Ordering::Relaxed);
        self.network.cancel();
    }
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::Relaxed)
    }
    pub async fn cancelled(&self) {
        self.network.cancelled().await;
    }
}

pub(crate) struct Loaded {
    pub context: Rc<Context>,
    pub projection: Projection<Disk>,
    pub invocations: Invocations,
    pub identity: String,
    pub highest_seq: u64,
    pub incomplete: bool,
}

pub(crate) fn read(
    http: &HttpClient,
    session: &str,
    runtime: &Handle,
    cancellation: &Cancellation,
) -> Result<Loaded> {
    let context = Context::new(Arc::clone(&cancellation.flag))?;
    let mut model = Replay::new(Rc::clone(&context))?;
    let mut query = RetainedSourceQuery::default();
    let mut cap = None;
    let mut incomplete = false;
    loop {
        let (meta, response) = request(http, session, &query, runtime, cancellation)?;
        verify(&meta, &query, cap)?;
        cap.get_or_insert((meta.highest_seq, meta.lowest_seq));
        query
            .history_identity
            .get_or_insert_with(|| meta.history_identity.clone());
        let Some(sequence) = meta.seq else {
            body(response, &mut io::sink(), 0, runtime, cancellation)?;
            if query.after < meta.highest_seq {
                return Err(invalid("retained snapshot ended before its high sequence").into());
            }
            break;
        };
        incomplete |= meta.invalid || sequence != query.after.saturating_add(1);
        if meta.omitted {
            body(response, &mut io::sink(), 0, runtime, cancellation)?;
            apply(&context, &mut model, sequence, View::Other)?;
        } else {
            let total = meta.total_bytes;
            let mut writer = context.spool.writer();
            let mut offset = (total - meta.offset).min(RETAINED_SOURCE_BLOCK_BYTES as u64);
            body(response, &mut writer, offset, runtime, cancellation)?;
            while offset < total {
                query.seq = Some(sequence);
                query.offset = offset;
                let (meta, response) = request(http, session, &query, runtime, cancellation)?;
                verify(&meta, &query, cap)?;
                if meta.omitted || meta.invalid || meta.total_bytes != total {
                    return Err(invalid("retained event changed during read").into());
                }
                let count = (total - offset).min(RETAINED_SOURCE_BLOCK_BYTES as u64);
                body(response, &mut writer, count, runtime, cancellation)?;
                offset += count;
            }
            let input = writer.finish()?;
            let event = context
                .store
                .with_transaction(|_| context.parse(input).and_then(|value| context.event(value)));
            match event {
                Ok(event) => apply(&context, &mut model, sequence, event)?,
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    incomplete = true;
                    apply(&context, &mut model, sequence, View::Other)?;
                }
                Err(error) => return Err(error.into()),
            }
        }
        query.after = sequence;
        query.seq = None;
        query.offset = 0;
        if sequence == meta.highest_seq {
            break;
        }
    }
    let (highest_seq, _) = cap.ok_or_else(|| invalid("retained snapshot metadata missing"))?;
    let identity = query
        .history_identity
        .ok_or_else(|| invalid("retained snapshot identity missing"))?;
    let (projection, invocations) = model.into_projection()?;
    Ok(Loaded {
        context,
        projection,
        invocations,
        identity,
        highest_seq,
        incomplete,
    })
}

pub(crate) fn probe(
    http: &HttpClient,
    session: &str,
    runtime: &Handle,
    cancellation: &Cancellation,
) -> Result<RetainedSourceMeta> {
    let query = RetainedSourceQuery {
        after: u64::MAX,
        ..Default::default()
    };
    let (meta, response) = request(http, session, &query, runtime, cancellation)?;
    verify(&meta, &query, None)?;
    if meta.seq.is_some() || meta.total_bytes != 0 {
        return Err(invalid("retained identity probe returned an event").into());
    }
    body(response, &mut io::sink(), 0, runtime, cancellation)?;
    Ok(meta)
}

fn apply(
    context: &Context,
    model: &mut Replay,
    sequence: u64,
    event: View<'_, Disk>,
) -> io::Result<()> {
    context.store.with_transaction(|_| {
        model.apply(sequence, event)?;
        Ok(())
    })
}

fn request(
    http: &HttpClient,
    session: &str,
    query: &RetainedSourceQuery,
    runtime: &Handle,
    cancellation: &Cancellation,
) -> Result<(RetainedSourceMeta, reqwest::Response)> {
    runtime.block_on(async {
        tokio::select! {
            _ = cancellation.network.cancelled() => Err(cancelled().into()),
            response = http.retained_source(session, query) => response.map_err(anyhow::Error::from),
        }
    })
}

fn body(
    mut response: reqwest::Response,
    destination: &mut impl Write,
    expected: u64,
    runtime: &Handle,
    cancellation: &Cancellation,
) -> Result<()> {
    runtime.block_on(async {
        tokio::select! {
            _ = cancellation.network.cancelled() => Err(cancelled().into()),
            result = async {
                let mut received = 0u64;
                while let Some(chunk) = response.chunk().await.context("read retained source response")? {
                    let next = received.checked_add(chunk.len() as u64).ok_or_else(|| invalid("retained block length overflow"))?;
                    if next > expected { return Err(invalid("retained source block exceeds requested range").into()) }
                    destination.write_all(&chunk).context("store retained source bytes")?;
                    received = next;
                }
                if received != expected { return Err(invalid("incomplete retained source block").into()) }
                Ok(())
            } => result,
        }
    })
}

fn verify(
    meta: &RetainedSourceMeta,
    query: &RetainedSourceQuery,
    cap: Option<(u64, Option<u64>)>,
) -> io::Result<()> {
    if meta.offset != query.offset
        || query
            .history_identity
            .as_ref()
            .is_some_and(|identity| *identity != meta.history_identity)
        || cap.is_some_and(|cap| cap != (meta.highest_seq, meta.lowest_seq))
        || query.seq.is_some_and(|sequence| meta.seq != Some(sequence))
        || meta
            .seq
            .is_some_and(|sequence| sequence <= query.after || sequence > meta.highest_seq)
        || (meta.omitted && meta.total_bytes != 0)
        || (meta.invalid && !meta.omitted)
        || meta.offset > meta.total_bytes
    {
        return Err(invalid("inconsistent retained source metadata"));
    }
    Ok(())
}
fn cancelled() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "retained read cancelled")
}

#[cfg(test)]
mod tests;
