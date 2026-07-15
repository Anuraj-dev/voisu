use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use voisu_core::{
    AudioChunk, BoundaryError, BoundaryFuture, BoundaryKind, CapturedAudio, Provider,
    ProviderCoordinator, ProviderStream, ProviderStreams, SourceTranscript,
};

struct ControlledStream {
    provider: Provider,
    delay: Duration,
    completions: Arc<AtomicUsize>,
    chunks: Arc<AtomicUsize>,
    aborts: Arc<AtomicUsize>,
    abort_delay: Duration,
}

impl ProviderStream for ControlledStream {
    fn provider(&self) -> Provider {
        self.provider
    }

    fn send_audio(&mut self, _chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        self.chunks.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
    }

    fn abort(self: Box<Self>) -> BoundaryFuture<'static, ()> {
        self.aborts.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep(self.abort_delay).await;
            Ok(())
        })
    }

    fn complete(
        self: Box<Self>,
        _audio: CapturedAudio,
    ) -> BoundaryFuture<'static, SourceTranscript> {
        self.completions.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            Ok(SourceTranscript {
                provider: self.provider,
                text: format!("{:?} Source Transcript", self.provider),
            })
        })
    }
}

fn stream(
    provider: Provider,
    delay: Duration,
    completions: Arc<AtomicUsize>,
    chunks: Arc<AtomicUsize>,
) -> Box<dyn ProviderStream> {
    Box::new(ControlledStream {
        provider,
        delay,
        completions,
        chunks,
        aborts: Arc::new(AtomicUsize::new(0)),
        abort_delay: Duration::ZERO,
    })
}

// Paused time: the runtime advances virtual time to the abort deadline instead
// of racing wall-clock ceilings, so the bound is asserted deterministically.
#[tokio::test(start_paused = true)]
async fn coordinator_abort_is_bounded_and_attempts_both_provider_streams() {
    let deepgram_aborts = Arc::new(AtomicUsize::new(0));
    let groq_aborts = Arc::new(AtomicUsize::new(0));
    let controlled = |provider, aborts| {
        Box::new(ControlledStream {
            provider,
            delay: Duration::ZERO,
            completions: Arc::new(AtomicUsize::new(0)),
            chunks: Arc::new(AtomicUsize::new(0)),
            aborts,
            abort_delay: Duration::from_secs(1),
        }) as Box<dyn ProviderStream>
    };
    let coordinator = ProviderCoordinator::start(
        Duration::from_millis(50),
        ProviderStreams {
            deepgram: controlled(Provider::Deepgram, Arc::clone(&deepgram_aborts)),
            groq: controlled(Provider::Groq, Arc::clone(&groq_aborts)),
        },
    );

    let started = tokio::time::Instant::now();
    let error = coordinator.abort().await.unwrap_err();
    assert_eq!(
        started.elapsed(),
        Duration::from_millis(50),
        "abort must end exactly at its deadline, not at the stream abort delays"
    );
    assert_eq!(error.kind(), BoundaryKind::Provider);
    assert_eq!(error.diagnostic(), "provider abort deadline elapsed");
    assert_eq!(deepgram_aborts.load(Ordering::SeqCst), 1);
    assert_eq!(groq_aborts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn coordinator_starts_both_completions_once_and_orders_attributed_sources() {
    let deepgram = Arc::new(AtomicUsize::new(0));
    let groq = Arc::new(AtomicUsize::new(0));
    let deepgram_chunks = Arc::new(AtomicUsize::new(0));
    let groq_chunks = Arc::new(AtomicUsize::new(0));
    let mut coordinator = ProviderCoordinator::start(
        Duration::from_secs(1),
        ProviderStreams {
            deepgram: stream(
                Provider::Deepgram,
                Duration::from_millis(30),
                Arc::clone(&deepgram),
                Arc::clone(&deepgram_chunks),
            ),
            groq: stream(
                Provider::Groq,
                Duration::from_millis(1),
                Arc::clone(&groq),
                Arc::clone(&groq_chunks),
            ),
        },
    );
    coordinator.stream_audio(AudioChunk(vec![1, 2, 3])).await.unwrap();
    let sources = coordinator.complete(CapturedAudio::empty()).await.unwrap();

    assert_eq!(deepgram.load(Ordering::SeqCst), 1);
    assert_eq!(groq.load(Ordering::SeqCst), 1);
    assert_eq!(deepgram_chunks.load(Ordering::SeqCst), 1);
    assert_eq!(groq_chunks.load(Ordering::SeqCst), 1);
    assert_eq!(
        sources.iter().map(|source| source.provider).collect::<Vec<_>>(),
        vec![Provider::Deepgram, Provider::Groq]
    );
}

#[tokio::test]
async fn provider_deadline_returns_the_valid_source_already_available() {
    let deepgram = Arc::new(AtomicUsize::new(0));
    let groq = Arc::new(AtomicUsize::new(0));
    let sources = ProviderCoordinator::start(
        Duration::from_millis(50),
        ProviderStreams {
            deepgram: stream(
                Provider::Deepgram,
                Duration::from_millis(1),
                Arc::clone(&deepgram),
                Arc::new(AtomicUsize::new(0)),
            ),
            groq: stream(
                Provider::Groq,
                Duration::from_secs(1),
                Arc::clone(&groq),
                Arc::new(AtomicUsize::new(0)),
            ),
        },
    )
        .complete(CapturedAudio::empty())
        .await
        .unwrap();

    assert_eq!(deepgram.load(Ordering::SeqCst), 1);
    assert_eq!(groq.load(Ordering::SeqCst), 1);
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].provider, Provider::Deepgram);
}

#[tokio::test(start_paused = true)]
async fn ready_sources_at_the_deadline_instant_are_not_discarded() {
    // Both providers complete at exactly the Provider Deadline instant. With
    // paused time the runtime advances to the shared timer, firing the provider
    // completions and the deadline in the same poll. A biased select must accept
    // the ready valid Source Transcripts instead of breaking at the deadline.
    let deadline = Duration::from_millis(50);
    let deepgram = Arc::new(AtomicUsize::new(0));
    let groq = Arc::new(AtomicUsize::new(0));
    let sources = ProviderCoordinator::start(
        deadline,
        ProviderStreams {
            deepgram: stream(
                Provider::Deepgram,
                deadline,
                Arc::clone(&deepgram),
                Arc::new(AtomicUsize::new(0)),
            ),
            groq: stream(
                Provider::Groq,
                deadline,
                Arc::clone(&groq),
                Arc::new(AtomicUsize::new(0)),
            ),
        },
    )
    .complete(CapturedAudio::empty())
    .await
    .expect("ready sources at the deadline instant must not be discarded");

    assert_eq!(
        sources.iter().map(|source| source.provider).collect::<Vec<_>>(),
        vec![Provider::Deepgram, Provider::Groq]
    );
}

#[test]
fn boundary_errors_separate_redacted_public_text_from_local_diagnostics() {
    let error = BoundaryError::new(
        BoundaryKind::Provider,
        "authorization=Bearer controlled-secret",
    );
    assert_eq!(error.public_message(), "Source Transcripts are unavailable");
    assert_eq!(
        error.diagnostic(),
        "authorization=Bearer controlled-secret"
    );
    assert!(!error.public_message().contains("controlled-secret"));
}
