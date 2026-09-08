//! Cloud-free provider slots so Local capture can reuse the capture pump
//! without constructing Groq/Deepgram clients or reading credentials.

use voisu_core::{
    AudioChunk, BoundaryError, BoundaryFuture, BoundaryKind, CapturedAudio, Provider,
    ProviderStream, SourceTranscript, TranscriptProvider,
};

pub struct CloudFreeProvider {
    slot: Provider,
}

impl CloudFreeProvider {
    #[must_use]
    pub fn slot(slot: Provider) -> Self {
        Self { slot }
    }
}

impl TranscriptProvider for CloudFreeProvider {
    fn start(&mut self, _recording_id: u64) -> Result<Box<dyn ProviderStream>, BoundaryError> {
        Ok(Box::new(CloudFreeStream { slot: self.slot }))
    }
}

struct CloudFreeStream {
    slot: Provider,
}

impl ProviderStream for CloudFreeStream {
    fn provider(&self) -> Provider {
        self.slot
    }

    fn send_audio(&mut self, _chunk: AudioChunk) -> BoundaryFuture<'_, ()> {
        Box::pin(async { Ok(()) })
    }

    fn abort(self: Box<Self>) -> BoundaryFuture<'static, ()> {
        Box::pin(async { Ok(()) })
    }

    fn complete(&mut self, _audio: CapturedAudio) -> BoundaryFuture<'_, SourceTranscript> {
        Box::pin(async {
            Err(BoundaryError::new(
                BoundaryKind::Provider,
                "Local path must not complete a Cloud Provider",
            ))
        })
    }
}

#[must_use]
pub fn cloud_free_slots() -> (Box<dyn TranscriptProvider>, Box<dyn TranscriptProvider>) {
    (
        Box::new(CloudFreeProvider::slot(Provider::Deepgram)),
        Box::new(CloudFreeProvider::slot(Provider::Groq)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::local_worker::CloudCapabilitySentinel;

    #[test]
    fn slots_do_not_touch_the_cloud_sentinel() {
        let sentinel = CloudCapabilitySentinel::new();
        let (deepgram, groq) = cloud_free_slots();
        drop(deepgram);
        drop(groq);
        assert!(sentinel.local_path_clean());
    }
}
