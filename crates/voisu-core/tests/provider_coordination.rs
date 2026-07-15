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
}

impl ProviderStream for ControlledStream {
    fn provider(&self) -> Provider {
        self.provider
    }

    fn send_audio(&mut self, _chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        self.chunks.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(()) })
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
    })
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
    let sources = coordinator.complete(CapturedAudio).await.unwrap();

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
        .complete(CapturedAudio)
        .await
        .unwrap();

    assert_eq!(deepgram.load(Ordering::SeqCst), 1);
    assert_eq!(groq.load(Ordering::SeqCst), 1);
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].provider, Provider::Deepgram);
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
