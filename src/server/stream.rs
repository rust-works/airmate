//! Streaming responses.
//!
//! The response body is a state machine driven by hyper: it polls the upstream only when hyper
//! asks for the next frame, so a slow client pauses the upstream read (backpressure) and a client
//! that disconnects drops the body, which drops the upstream connection (cancellation).

use std::collections::VecDeque;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body::{Body, Frame};
use hyper::body::Incoming;
use tokio::sync::OwnedSemaphorePermit;
use tokio::time::{Instant, Sleep};

use airmate_core::ir::Usage;
use airmate_core::outbound::anthropic::PassthroughStream;
use airmate_core::outbound::json_passthrough::JsonPassthroughStream;
use airmate_core::stream::{IrEvent, StreamError, StreamErrorKind, StreamParse, StreamRender};

/// What a finished stream said, for the request log and the ledger.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamSummary {
    /// Token usage.
    pub usage: Usage,
    /// The finish or stop reason.
    pub finish: Option<String>,
    /// Why the stream failed, if it did.
    pub error: Option<String>,
    /// Whether the stream reached its terminal event.
    pub completed: bool,
    /// Whether the client went away first.
    pub cancelled: bool,
}

/// Turns upstream bytes into client bytes.
pub enum Pipe {
    /// Parse the upstream protocol into the IR and render it for the client.
    Translate {
        /// The upstream parser.
        parser: Box<dyn StreamParse>,
        /// The client renderer.
        render: Box<dyn StreamRender>,
        /// What the stream has said.
        summary: StreamSummary,
    },
    /// Anthropic both ways: rewrite and forward.
    Passthrough(PassthroughStream),
    /// Chat or Responses both ways: rewrite and forward.
    Json(JsonPassthroughStream),
}

impl Pipe {
    /// A translating pipe.
    #[must_use]
    pub fn translate(parser: Box<dyn StreamParse>, render: Box<dyn StreamRender>) -> Self {
        Self::Translate {
            parser,
            render,
            summary: StreamSummary::default(),
        }
    }

    fn render_events(
        render: &mut dyn StreamRender,
        summary: &mut StreamSummary,
        events: Vec<IrEvent>,
    ) -> Vec<u8> {
        let mut out = Vec::new();
        for ev in events {
            match &ev {
                IrEvent::Usage(u) => summary.usage = *u,
                IrEvent::Finish(r) => {
                    summary.finish = Some(format!("{r:?}"));
                    summary.completed = true;
                }
                IrEvent::Error(e) => summary.error = Some(e.message.clone()),
                _ => {}
            }
            out.extend(render.push(&ev));
        }
        out
    }

    /// Feeds upstream bytes.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self {
            Self::Translate {
                parser,
                render,
                summary,
            } => {
                let events = parser.push(bytes);
                Self::render_events(render.as_mut(), summary, events)
            }
            Self::Passthrough(p) => p.push(bytes),
            Self::Json(p) => p.push(bytes),
        }
    }

    /// The upstream ended cleanly.
    pub fn finish(&mut self) -> Vec<u8> {
        match self {
            Self::Translate {
                parser,
                render,
                summary,
            } => {
                let events = parser.finish();
                let mut out = Self::render_events(render.as_mut(), summary, events);
                out.extend(render.finish());
                out
            }
            Self::Passthrough(p) => p.finish(),
            Self::Json(p) => p.finish(),
        }
    }

    /// The upstream broke or went silent: end the stream with an error event.
    pub fn fail(&mut self, message: &str) -> Vec<u8> {
        match self {
            Self::Translate {
                render, summary, ..
            } => {
                let ev = IrEvent::Error(StreamError {
                    kind: StreamErrorKind::Upstream,
                    message: message.to_owned(),
                });
                summary.error = Some(message.to_owned());
                render.push(&ev)
            }
            Self::Passthrough(p) => p.fail(message),
            Self::Json(p) => p.fail(message),
        }
    }

    /// Bytes to send while the upstream is silent.
    #[must_use]
    pub fn keepalive(&self) -> Vec<u8> {
        match self {
            Self::Translate { render, .. } => render.keepalive(),
            Self::Passthrough(_) => PassthroughStream::keepalive(),
            Self::Json(_) => JsonPassthroughStream::keepalive(),
        }
    }

    /// What the stream has said so far.
    #[must_use]
    pub fn summary(&self) -> StreamSummary {
        match self {
            Self::Translate { summary, .. } => summary.clone(),
            Self::Passthrough(p) => StreamSummary {
                usage: p.summary().usage,
                finish: p.summary().stop_reason.clone(),
                error: None,
                completed: p.completed(),
                cancelled: false,
            },
            Self::Json(p) => StreamSummary {
                usage: p.usage(),
                finish: None,
                error: None,
                completed: p.completed(),
                cancelled: false,
            },
        }
    }
}

/// Called once when the stream ends or is dropped.
pub type OnDone = Box<dyn FnOnce(StreamSummary) + Send>;

/// The streaming response body.
pub struct StreamBody {
    upstream: Incoming,
    pipe: Pipe,
    out: VecDeque<Bytes>,
    idle: Pin<Box<Sleep>>,
    idle_for: Duration,
    ping: Pin<Box<Sleep>>,
    ping_every: Duration,
    finished: bool,
    on_done: Option<OnDone>,
    _permit: Option<OwnedSemaphorePermit>,
}

impl StreamBody {
    /// A body that starts with `first` (already read from the upstream) and then follows it.
    #[must_use]
    pub fn new(
        upstream: Incoming,
        first: &[u8],
        mut pipe: Pipe,
        idle_for: Duration,
        ping_every: Duration,
        permit: Option<OwnedSemaphorePermit>,
        on_done: Option<OnDone>,
    ) -> Self {
        let mut out = VecDeque::new();
        let initial = pipe.push(first);
        if !initial.is_empty() {
            out.push_back(Bytes::from(initial));
        }
        Self {
            upstream,
            pipe,
            out,
            idle: Box::pin(tokio::time::sleep(idle_for)),
            idle_for,
            ping: Box::pin(tokio::time::sleep(ping_every)),
            ping_every,
            finished: false,
            on_done,
            _permit: permit,
        }
    }

    fn queue(&mut self, bytes: Vec<u8>) {
        if !bytes.is_empty() {
            self.out.push_back(Bytes::from(bytes));
        }
    }

    fn done(&mut self, cancelled: bool) {
        if let Some(cb) = self.on_done.take() {
            let mut summary = self.pipe.summary();
            summary.cancelled = cancelled;
            cb(summary);
        }
    }
}

impl Drop for StreamBody {
    fn drop(&mut self) {
        // Dropped before the end: the client went away.
        let cancelled = !self.finished || !self.out.is_empty();
        self.done(cancelled);
    }
}

impl Body for StreamBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        let this = self.get_mut();
        loop {
            if let Some(b) = this.out.pop_front() {
                this.ping.as_mut().reset(Instant::now() + this.ping_every);
                return Poll::Ready(Some(Ok(Frame::data(b))));
            }
            if this.finished {
                this.done(false);
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.upstream).poll_frame(cx) {
                Poll::Ready(Some(Ok(frame))) => {
                    this.idle.as_mut().reset(Instant::now() + this.idle_for);
                    if let Ok(data) = frame.into_data() {
                        let out = this.pipe.push(&data);
                        this.queue(out);
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    let out = this
                        .pipe
                        .fail(&format!("the upstream connection broke: {e}"));
                    this.queue(out);
                    this.finished = true;
                }
                Poll::Ready(None) => {
                    let out = this.pipe.finish();
                    this.queue(out);
                    this.finished = true;
                }
                Poll::Pending => {
                    if this.idle.as_mut().poll(cx).is_ready() {
                        let out = this.pipe.fail(&format!(
                            "the upstream was silent for {} seconds",
                            this.idle_for.as_secs()
                        ));
                        this.queue(out);
                        this.finished = true;
                        continue;
                    }
                    if this.ping.as_mut().poll(cx).is_ready() {
                        let ping = this.pipe.keepalive();
                        this.queue(ping);
                        this.ping.as_mut().reset(Instant::now() + this.ping_every);
                        continue;
                    }
                    return Poll::Pending;
                }
            }
        }
    }
}
