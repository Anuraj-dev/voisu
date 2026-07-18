use voisu_core::{BoundaryFuture, FocusProbe, WindowIdentity};

struct FixedProbe(Option<WindowIdentity>);

impl FocusProbe for FixedProbe {
    fn current(&mut self) -> BoundaryFuture<'_, Option<WindowIdentity>> {
        let identity = self.0.clone();
        Box::pin(async move { Ok(identity) })
    }
}

#[tokio::test]
async fn focus_probe_reports_the_compositor_stable_identity() {
    let expected = WindowIdentity {
        stable_id: "window-42".to_owned(),
        process_id: Some(4242),
        app_id: Some("org.example.Editor".to_owned()),
    };
    let mut probe = FixedProbe(Some(expected.clone()));

    assert_eq!(probe.current().await.unwrap(), Some(expected));
}
